//! OS-keyring API key storage with a process-memory fallback.

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::Duration;

use keyring::{Entry, Error as KeyringError};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::sync::oneshot;

use crate::config::APP_NAME;
use crate::domain::{FAL_PROVIDER_ID, OPENROUTER_PROVIDER_ID, ProviderId};

/// Compatibility-sensitive username used by every previous OpenRouter release.
pub const DEFAULT_USERNAME: &str = "openrouter-api-key";
pub const FAL_USERNAME: &str = "provider:fal:api-key";
/// Maximum time the service actor waits for an operating-system credential
/// backend. The dedicated worker can remain blocked without freezing the rest
/// of the application.
pub const CREDENTIAL_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);

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
    #[error("The saved API key could not be removed from the system keyring")]
    PersistentDeleteFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialDeleteOutcome {
    Deleted,
    NotFound,
    MemoryOnly,
}

/// A bounded failure returned by the isolated credential worker. These
/// messages deliberately contain no backend-provided text because credential
/// backends are not trusted to avoid echoing secrets.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CredentialWorkerError {
    #[error("The system keyring is still handling an earlier request")]
    Busy,
    #[error("The system keyring worker is unavailable")]
    Unavailable,
    #[error("The system keyring did not respond in time")]
    Timeout,
}

trait CredentialEntry: Send + Sync {
    fn get_password(&self) -> Result<String, KeyringError>;
    fn set_password(&self, value: &str) -> Result<(), KeyringError>;
    fn delete_credential(&self) -> Result<(), KeyringError>;
}

impl CredentialEntry for Entry {
    fn get_password(&self) -> Result<String, KeyringError> {
        Entry::get_password(self)
    }

    fn set_password(&self, value: &str) -> Result<(), KeyringError> {
        Entry::set_password(self, value)
    }

    fn delete_credential(&self) -> Result<(), KeyringError> {
        Entry::delete_credential(self)
    }
}

/// Stores a single OpenRouter API key without writing plaintext credentials to disk.
///
/// Platform failures deliberately degrade to in-process memory and do not expose the
/// backend error text: credential backends are not trusted to avoid echoing inputs.
pub struct CredentialStore {
    service_name: String,
    username: String,
    entry: Option<Box<dyn CredentialEntry>>,
    memory_key: Option<SecretString>,
    status: CredentialStatus,
}

enum CredentialWorkerCommand {
    Get {
        reply: oneshot::Sender<(Option<SecretString>, CredentialStatus)>,
    },
    Set {
        api_key: SecretString,
        reply: oneshot::Sender<(Result<bool, CredentialError>, CredentialStatus)>,
    },
    Delete {
        reply: oneshot::Sender<(
            Result<CredentialDeleteOutcome, CredentialError>,
            CredentialStatus,
        )>,
    },
}

/// Owns a credential store on a dedicated bounded worker thread.
///
/// A platform keyring call is allowed to block that one worker indefinitely,
/// but it can never block the async service actor. The single-slot queue keeps
/// operations ordered without accumulating secrets or unbounded work behind a
/// stuck backend.
pub struct CredentialWorker {
    commands: SyncSender<CredentialWorkerCommand>,
}

impl std::fmt::Debug for CredentialWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialWorker")
            .finish_non_exhaustive()
    }
}

impl CredentialWorker {
    /// Start an isolated worker for one provider. Store construction also runs
    /// on the worker because some platform keyring implementations perform IPC
    /// while creating an entry.
    pub fn for_provider(
        provider_id: &ProviderId,
        use_system_credentials: bool,
    ) -> std::io::Result<Self> {
        let provider_id = provider_id.clone();
        Self::spawn_with_factory(move || {
            if use_system_credentials {
                CredentialStore::for_provider(&provider_id)
            } else {
                CredentialStore::memory_only_for_provider(&provider_id)
            }
        })
    }

    fn spawn_with_factory(
        factory: impl FnOnce() -> CredentialStore + Send + 'static,
    ) -> std::io::Result<Self> {
        let (commands, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("video-harness-credentials".into())
            .spawn(move || {
                let mut store = factory();
                while let Ok(command) = receiver.recv() {
                    match command {
                        CredentialWorkerCommand::Get { reply } => {
                            let value = store.get();
                            let _ = reply.send((value, store.status()));
                        }
                        CredentialWorkerCommand::Set { api_key, reply } => {
                            let result = store.set(api_key);
                            let _ = reply.send((result, store.status()));
                        }
                        CredentialWorkerCommand::Delete { reply } => {
                            let result = store.delete();
                            let _ = reply.send((result, store.status()));
                        }
                    }
                }
            })?;
        Ok(Self { commands })
    }

    pub async fn get(
        &self,
        timeout: Duration,
    ) -> Result<(Option<SecretString>, CredentialStatus), CredentialWorkerError> {
        let receiver = self.dispatch(|reply| CredentialWorkerCommand::Get { reply })?;
        receive_worker_reply(receiver, timeout).await
    }

    pub async fn set(
        &self,
        api_key: SecretString,
        timeout: Duration,
    ) -> Result<(Result<bool, CredentialError>, CredentialStatus), CredentialWorkerError> {
        let receiver = self.dispatch(|reply| CredentialWorkerCommand::Set { api_key, reply })?;
        receive_worker_reply(receiver, timeout).await
    }

    pub async fn delete(
        &self,
        timeout: Duration,
    ) -> Result<
        (
            Result<CredentialDeleteOutcome, CredentialError>,
            CredentialStatus,
        ),
        CredentialWorkerError,
    > {
        let receiver = self.dispatch(|reply| CredentialWorkerCommand::Delete { reply })?;
        receive_worker_reply(receiver, timeout).await
    }

    fn dispatch<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> CredentialWorkerCommand,
    ) -> Result<oneshot::Receiver<T>, CredentialWorkerError> {
        let (reply, receiver) = oneshot::channel();
        match self.commands.try_send(command(reply)) {
            Ok(()) => Ok(receiver),
            Err(TrySendError::Full(_)) => Err(CredentialWorkerError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(CredentialWorkerError::Unavailable),
        }
    }
}

async fn receive_worker_reply<T>(
    receiver: oneshot::Receiver<T>,
    timeout: Duration,
) -> Result<T, CredentialWorkerError> {
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(CredentialWorkerError::Unavailable),
        Err(_) => Err(CredentialWorkerError::Timeout),
    }
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
        let entry = Entry::new(&service_name, &username)
            .ok()
            .map(|entry| Box::new(entry) as Box<dyn CredentialEntry>);
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

    /// Forget the in-memory key and remove any persistent entry. Backend
    /// failures are explicit so callers never report that a secret was
    /// forgotten while it may still be present in the system keyring.
    pub fn delete(&mut self) -> Result<CredentialDeleteOutcome, CredentialError> {
        self.memory_key = None;
        let Some(entry) = &self.entry else {
            return Ok(CredentialDeleteOutcome::MemoryOnly);
        };
        match entry.delete_credential() {
            Ok(()) => Ok(CredentialDeleteOutcome::Deleted),
            Err(KeyringError::NoEntry) => Ok(CredentialDeleteOutcome::NotFound),
            // Retain the entry handle so a transient backend failure can be
            // retried. Dropping it here would make a surviving secret
            // impossible to remove until the next process launch.
            Err(_) => Err(CredentialError::PersistentDeleteFailed),
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Condvar, Mutex};

    use super::*;

    #[derive(Default)]
    struct Gate {
        released: Mutex<bool>,
        wake: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut released = self.released.lock().expect("gate lock");
            while !*released {
                released = self.wake.wait(released).expect("gate wait");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("gate lock") = true;
            self.wake.notify_all();
        }
    }

    struct ScriptedEntry {
        password: Mutex<Option<String>>,
        delete_results: Mutex<Vec<Result<(), KeyringError>>>,
    }

    impl CredentialEntry for ScriptedEntry {
        fn get_password(&self) -> Result<String, KeyringError> {
            self.password
                .lock()
                .expect("password lock")
                .clone()
                .ok_or(KeyringError::NoEntry)
        }

        fn set_password(&self, value: &str) -> Result<(), KeyringError> {
            *self.password.lock().expect("password lock") = Some(value.to_owned());
            Ok(())
        }

        fn delete_credential(&self) -> Result<(), KeyringError> {
            let result = self
                .delete_results
                .lock()
                .expect("delete-results lock")
                .remove(0);
            if result.is_ok() {
                *self.password.lock().expect("password lock") = None;
            }
            result
        }
    }

    fn store_with_entry(entry: ScriptedEntry) -> CredentialStore {
        store_with_boxed_entry(Box::new(entry))
    }

    fn store_with_boxed_entry(entry: Box<dyn CredentialEntry>) -> CredentialStore {
        CredentialStore {
            service_name: "fixture-service".into(),
            username: "fixture-user".into(),
            entry: Some(entry),
            memory_key: Some(SecretString::from("fixture-secret".to_owned())),
            status: CredentialStatus {
                backend: "system keyring".into(),
                available: true,
                persistent: true,
                message: "fixture".into(),
            },
        }
    }

    #[test]
    fn persistent_delete_failure_is_truthful_and_retryable() {
        let entry = ScriptedEntry {
            password: Mutex::new(Some("fixture-secret".into())),
            delete_results: Mutex::new(vec![
                Err(KeyringError::PlatformFailure(Box::new(
                    std::io::Error::other("fixture failure"),
                ))),
                Ok(()),
            ]),
        };
        let mut store = store_with_entry(entry);

        assert_eq!(store.delete(), Err(CredentialError::PersistentDeleteFailed));
        assert!(store.memory_key.is_none());
        assert!(
            store.entry.is_some(),
            "failed deletion must remain retryable"
        );
        assert_eq!(store.delete(), Ok(CredentialDeleteOutcome::Deleted));
    }

    #[test]
    fn delete_distinguishes_memory_only_and_missing_persistent_entries() {
        let mut memory = CredentialStore::memory_only();
        memory
            .set_str("fixture-secret")
            .expect("set memory credential");
        assert_eq!(memory.delete(), Ok(CredentialDeleteOutcome::MemoryOnly));
        assert!(memory.get().is_none());

        let entry = ScriptedEntry {
            password: Mutex::new(None),
            delete_results: Mutex::new(vec![Err(KeyringError::NoEntry)]),
        };
        let mut missing = store_with_entry(entry);
        assert_eq!(missing.delete(), Ok(CredentialDeleteOutcome::NotFound));
    }

    struct BlockingGetEntry {
        gate: Arc<Gate>,
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        password: Mutex<Option<String>>,
    }

    impl CredentialEntry for BlockingGetEntry {
        fn get_password(&self) -> Result<String, KeyringError> {
            if let Some(entered) = self.entered.lock().expect("entered lock").take() {
                let _ = entered.send(());
            }
            self.gate.wait();
            self.password
                .lock()
                .expect("password lock")
                .clone()
                .ok_or(KeyringError::NoEntry)
        }

        fn set_password(&self, value: &str) -> Result<(), KeyringError> {
            *self.password.lock().expect("password lock") = Some(value.into());
            Ok(())
        }

        fn delete_credential(&self) -> Result<(), KeyringError> {
            *self.password.lock().expect("password lock") = None;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_startup_read_times_out_without_losing_queued_operation_order() {
        let gate = Arc::new(Gate::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let store = store_with_boxed_entry(Box::new(BlockingGetEntry {
            gate: Arc::clone(&gate),
            entered: Mutex::new(Some(entered_tx)),
            password: Mutex::new(Some("persisted-secret".into())),
        }));
        let worker = Arc::new(
            CredentialWorker::spawn_with_factory(move || store).expect("credential worker"),
        );
        let read = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.get(Duration::from_millis(100)).await }
        });
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
            .await
            .expect("entered waiter")
            .expect("worker entered get");
        assert!(matches!(
            read.await.expect("read task"),
            Err(CredentialWorkerError::Timeout)
        ));

        let delete = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.delete(Duration::from_secs(1)).await }
        });
        tokio::task::yield_now().await;
        gate.release();
        assert!(matches!(
            delete.await.expect("delete task"),
            Ok((Ok(CredentialDeleteOutcome::Deleted), _))
        ));
    }

    struct BlockingSetEntry {
        gate: Arc<Gate>,
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        password: Mutex<Option<String>>,
        operations: Arc<Mutex<Vec<&'static str>>>,
    }

    impl CredentialEntry for BlockingSetEntry {
        fn get_password(&self) -> Result<String, KeyringError> {
            self.password
                .lock()
                .expect("password lock")
                .clone()
                .ok_or(KeyringError::NoEntry)
        }

        fn set_password(&self, value: &str) -> Result<(), KeyringError> {
            if let Some(entered) = self.entered.lock().expect("entered lock").take() {
                let _ = entered.send(());
            }
            self.gate.wait();
            self.operations.lock().expect("operations lock").push("set");
            *self.password.lock().expect("password lock") = Some(value.into());
            Ok(())
        }

        fn delete_credential(&self) -> Result<(), KeyringError> {
            self.operations
                .lock()
                .expect("operations lock")
                .push("delete");
            *self.password.lock().expect("password lock") = None;
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_persist_times_out_and_forget_stays_ordered_behind_it() {
        let gate = Arc::new(Gate::default());
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let store = store_with_boxed_entry(Box::new(BlockingSetEntry {
            gate: Arc::clone(&gate),
            entered: Mutex::new(Some(entered_tx)),
            password: Mutex::new(None),
            operations: Arc::clone(&operations),
        }));
        let worker = Arc::new(
            CredentialWorker::spawn_with_factory(move || store).expect("credential worker"),
        );
        let persist = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move {
                worker
                    .set(
                        SecretString::from("new-secret".to_owned()),
                        Duration::from_millis(100),
                    )
                    .await
            }
        });
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
            .await
            .expect("entered waiter")
            .expect("worker entered set");
        assert_eq!(
            persist.await.expect("persist task"),
            Err(CredentialWorkerError::Timeout)
        );

        let delete = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.delete(Duration::from_secs(1)).await }
        });
        tokio::task::yield_now().await;
        gate.release();
        assert!(matches!(
            delete.await.expect("delete task"),
            Ok((Ok(CredentialDeleteOutcome::Deleted), _))
        ));
        assert_eq!(
            *operations.lock().expect("operations lock"),
            ["set", "delete"]
        );
    }

    struct BlockingDeleteEntry {
        gate: Arc<Gate>,
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        delete_results: Mutex<Vec<Result<(), KeyringError>>>,
    }

    impl CredentialEntry for BlockingDeleteEntry {
        fn get_password(&self) -> Result<String, KeyringError> {
            Ok("fixture-secret".into())
        }

        fn set_password(&self, _value: &str) -> Result<(), KeyringError> {
            Ok(())
        }

        fn delete_credential(&self) -> Result<(), KeyringError> {
            if let Some(entered) = self.entered.lock().expect("entered lock").take() {
                let _ = entered.send(());
                self.gate.wait();
            }
            self.delete_results
                .lock()
                .expect("delete results lock")
                .remove(0)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_delete_is_not_confirmed_and_can_be_retried_in_order() {
        let gate = Arc::new(Gate::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let store = store_with_boxed_entry(Box::new(BlockingDeleteEntry {
            gate: Arc::clone(&gate),
            entered: Mutex::new(Some(entered_tx)),
            delete_results: Mutex::new(vec![
                Err(KeyringError::PlatformFailure(Box::new(
                    std::io::Error::other("fixture failure"),
                ))),
                Ok(()),
            ]),
        }));
        let worker = Arc::new(
            CredentialWorker::spawn_with_factory(move || store).expect("credential worker"),
        );
        let first_delete = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.delete(Duration::from_millis(100)).await }
        });
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(1)))
            .await
            .expect("entered waiter")
            .expect("worker entered delete");
        assert_eq!(
            first_delete.await.expect("first delete task"),
            Err(CredentialWorkerError::Timeout)
        );

        let retry = tokio::spawn({
            let worker = Arc::clone(&worker);
            async move { worker.delete(Duration::from_secs(1)).await }
        });
        tokio::task::yield_now().await;
        gate.release();
        assert!(matches!(
            retry.await.expect("retry task"),
            Ok((Ok(CredentialDeleteOutcome::Deleted), _))
        ));
    }
}
