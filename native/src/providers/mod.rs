//! Provider adapters and the object-safe provider boundary.

pub mod fal;
pub mod openrouter;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::api::DownloadProgress;
use crate::domain::{
    CostQuote, JobLocator, ProviderDescriptor, ProviderId, VideoArtifact, VideoCatalog, VideoJob,
    VideoRequest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    InsufficientCredits,
    Validation,
    ContentPolicy,
    RateLimit,
    Unavailable,
    Network,
    SubmissionUncertain,
    UnsafeEndpoint,
    Response,
    Download,
    Configuration,
}

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ProviderError {
    pub provider_id: ProviderId,
    pub kind: ProviderErrorKind,
    pub message: String,
    pub status_code: Option<u16>,
    pub code: Option<String>,
    pub details: Map<String, Value>,
    pub retry_after: Option<Duration>,
}

impl ProviderError {
    pub fn new(
        provider_id: ProviderId,
        kind: ProviderErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            provider_id,
            kind,
            message: message.into(),
            status_code: None,
            code: None,
            details: Map::new(),
            retry_after: None,
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::Network
                | ProviderErrorKind::RateLimit
                | ProviderErrorKind::Unavailable
                | ProviderErrorKind::Download
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAccount {
    pub label: String,
    pub balance: Option<Decimal>,
    pub raw: Value,
}

#[async_trait]
pub trait VideoProvider: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;

    async fn validate_credentials(&self) -> Result<ProviderAccount, ProviderError>;

    async fn load_catalog(&self) -> Result<VideoCatalog, ProviderError>;

    async fn quote(&self, request: &VideoRequest) -> Result<CostQuote, ProviderError>;

    /// Perform exactly one potentially billable submission. Implementations
    /// must turn ambiguous transport failures into `SubmissionUncertain` and
    /// must never retry this method internally.
    async fn submit(&self, request: &VideoRequest) -> Result<VideoJob, ProviderError>;

    async fn poll(&self, locator: &JobLocator) -> Result<VideoJob, ProviderError>;

    async fn import(&self, locator: &JobLocator) -> Result<VideoJob, ProviderError> {
        self.poll(locator).await
    }

    async fn download(
        &self,
        artifact: &VideoArtifact,
        destination: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadProgress>>,
    ) -> Result<PathBuf, ProviderError>;
}

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Arc<RwLock<BTreeMap<ProviderId, Arc<dyn VideoProvider>>>>,
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("provider_ids", &self.ids())
            .finish()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, provider: Arc<dyn VideoProvider>) -> Option<Arc<dyn VideoProvider>> {
        let provider_id = provider.descriptor().id;
        self.providers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(provider_id, provider)
    }

    pub fn get(&self, provider_id: &ProviderId) -> Option<Arc<dyn VideoProvider>> {
        self.providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(provider_id)
            .cloned()
    }

    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}
