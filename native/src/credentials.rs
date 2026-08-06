//! OS-keyring API key storage with a process-memory fallback.

use keyring::{Entry, Error as KeyringError};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::config::APP_NAME;
use crate::domain::{FAL_PROVIDER_ID, OPENROUTER_PROVIDER_ID, ProviderId};

/// Compatibility-sensitive username used by every previous OpenRouter release.
pub const DEFAULT_USERNAME: &str = "openrouter-api-key";
pub const FAL_USERNAME: &str = "provider:fal:api-key";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    pub backend: String,
    pub available: bool,
    pub persistent: bool,
    pub message: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CredentialError {
    #[error("API key cannot be empty")]
    Empty,
    #[error("API key cannot contain whitespace")]
    Whitespace,
}

/// Stores a single OpenRouter API key without writing plaintext credentials to disk.
///
/// Platform failures deliberately degrade to in-process memory and do not expose the
/// backend error text: credential backends are not trusted to avoid echoing inputs.
pub struct CredentialStore {
    service_name: String,
    username: String,
    entry: Option<Entry>,
    memory_key: Option<SecretString>,
    status: CredentialStatus,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .field("service_name", &self.service_name)
            .field("username", &self.username)
            .field("status", &self.status)
            .field("memory_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore {
    pub fn new() -> Self {
        Self::with_identifiers(APP_NAME, DEFAULT_USERNAME)
    }

    /// Construct the independent credential session for a provider. OpenRouter
    /// keeps the exact legacy service/username pair; all other providers are
    /// isolated under a provider-scoped username in the same service.
    pub fn for_provider(provider_id: &ProviderId) -> Self {
        Self::with_identifiers(APP_NAME, username_for_provider(provider_id))
    }

    /// Construct a store which never initializes or reads the OS keyring.
    /// Useful for deterministic tests and explicitly ephemeral sessions.
    pub fn memory_only() -> Self {
        Self::memory_only_for_provider(&ProviderId::openrouter())
    }

    pub fn memory_only_for_provider(provider_id: &ProviderId) -> Self {
        Self {
            service_name: APP_NAME.into(),
            username: username_for_provider(provider_id),
            entry: None,
            memory_key: None,
            status: memory_status(
                "System keyring disabled; key will be kept in memory for this session",
            ),
        }
    }

    pub fn with_identifiers(service_name: impl Into<String>, username: impl Into<String>) -> Self {
        let service_name = service_name.into();
        let username = username.into();
        let entry = Entry::new(&service_name, &username).ok();
        let status = if entry.is_some() {
            CredentialStatus {
                backend: "system keyring".into(),
                available: true,
                persistent: true,
                message: "API key will be stored in the system keyring".into(),
            }
        } else {
            memory_status("System keyring unavailable; key will be kept in memory for this session")
        };
        Self {
            service_name,
            username,
            entry,
            memory_key: None,
            status,
        }
    }

    pub fn status(&self) -> CredentialStatus {
        self.status.clone()
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn persistent_available(&self) -> bool {
        self.status.persistent
    }

    pub fn get(&mut self) -> Option<SecretString> {
        if let Some(entry) = &self.entry {
            match entry.get_password() {
                Ok(value) if !value.is_empty() => {
                    let secret = SecretString::from(value);
                    self.memory_key = Some(secret.clone());
                    return Some(secret);
                }
                Ok(_) | Err(KeyringError::NoEntry) => {}
                Err(_) => self.degrade_to_memory(),
            }
        }
        self.memory_key.clone()
    }

    /// Store a key, returning whether it was persisted in the OS keyring.
    pub fn set(&mut self, api_key: SecretString) -> Result<bool, CredentialError> {
        let normalized = api_key.expose_secret().trim().to_owned();
        validate_key(&normalized)?;
        let api_key = SecretString::from(normalized);
        let persisted = if let Some(entry) = &self.entry {
            match entry.set_password(api_key.expose_secret()) {
                Ok(()) => true,
                Err(_) => {
                    self.degrade_to_memory();
                    false
                }
            }
        } else {
            false
        };
        self.memory_key = Some(api_key);
        Ok(persisted)
    }

    pub fn set_str(&mut self, api_key: impl Into<String>) -> Result<bool, CredentialError> {
        self.set(SecretString::from(api_key.into()))
    }

    /// Forget the key, returning whether an existing persistent entry was deleted.
    pub fn delete(&mut self) -> bool {
        self.memory_key = None;
        let Some(entry) = &self.entry else {
            return false;
        };
        match entry.delete_credential() {
            Ok(()) => true,
            Err(KeyringError::NoEntry) => false,
            Err(_) => {
                self.degrade_to_memory();
                false
            }
        }
    }

    fn degrade_to_memory(&mut self) {
        self.entry = None;
        self.status =
            memory_status("System keyring failed; key is kept in memory for this session only");
    }
}

pub fn username_for_provider(provider_id: &ProviderId) -> String {
    match provider_id.as_str() {
        OPENROUTER_PROVIDER_ID => DEFAULT_USERNAME.into(),
        FAL_PROVIDER_ID => FAL_USERNAME.into(),
        provider => format!("provider:{provider}:api-key"),
    }
}

fn validate_key(value: &str) -> Result<(), CredentialError> {
    if value.trim().is_empty() {
        return Err(CredentialError::Empty);
    }
    if value.chars().any(char::is_whitespace) {
        return Err(CredentialError::Whitespace);
    }
    Ok(())
}

fn memory_status(message: &str) -> CredentialStatus {
    CredentialStatus {
        backend: "memory".into(),
        available: false,
        persistent: false,
        message: message.into(),
    }
}
