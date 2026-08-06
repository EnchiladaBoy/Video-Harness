//! Provider adapters and the object-safe provider boundary.

pub mod fal;
pub mod openrouter;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::api::DownloadProgress;
use crate::domain::{
    CostQuote, DraftMedia, GenerationDraft, JobLocator, MediaKind, MediaSource, ProviderDescriptor,
    ProviderId, StagedMedia, UploadReceipt, VideoArtifact, VideoCatalog, VideoJob, VideoRequest,
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

/// Provider-level media behavior used to update the GUI before Review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaCapabilities {
    pub remote_urls: bool,
    pub local_files: bool,
    /// fal CDN input uploads are accessible to anyone who has their URL.
    pub uploaded_files_public: bool,
    /// The lifetime requested for provider-managed input uploads.
    pub upload_retention: Option<Duration>,
}

impl MediaCapabilities {
    pub const fn urls_only() -> Self {
        Self {
            remote_urls: true,
            local_files: false,
            uploaded_files_public: false,
            upload_retention: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadProgress {
    pub sent: u64,
    pub total: u64,
}

/// Hash a local media file without retaining its contents in memory. The
/// digest is used as the provider-upload cache key.
pub async fn media_sha256(path: &Path) -> Result<(String, u64), std::io::Error> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}

#[async_trait]
pub trait VideoProvider: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;

    fn media_capabilities(&self) -> MediaCapabilities {
        MediaCapabilities::urls_only()
    }

    /// Provider/model-specific local constraints that can be checked without
    /// uploading bytes or submitting a generation.
    fn validate_draft_media_constraints(
        &self,
        _draft: &GenerationDraft,
    ) -> Result<(), ProviderError> {
        Ok(())
    }

    /// Provider/model-specific constraints checked against the media that was
    /// actually staged. This closes the gap between early draft validation
    /// and async upload work: implementations can trust uploaded receipt sizes
    /// here before Review, quoting, or any potentially billable submission.
    fn validate_staged_media_constraints(
        &self,
        _draft: &GenerationDraft,
        _staged_media: &[StagedMedia],
    ) -> Result<(), ProviderError> {
        Ok(())
    }

    /// Validate an editable draft without uploading media or performing a
    /// potentially billable submission. Local files are represented by inert
    /// public-HTTPS placeholders only after their paths and provider media
    /// capabilities have been checked.
    async fn validate_draft(&self, draft: &GenerationDraft) -> Result<(), ProviderError> {
        let descriptor = self.descriptor();
        if draft.provider_id != descriptor.id {
            return Err(ProviderError::new(
                descriptor.id,
                ProviderErrorKind::Validation,
                "The draft belongs to a different provider",
            ));
        }
        draft.validate().map_err(|error| {
            ProviderError::new(
                descriptor.id.clone(),
                ProviderErrorKind::Validation,
                error.to_string(),
            )
        })?;
        self.validate_draft_media_constraints(draft)?;

        let capabilities = self.media_capabilities();
        let mut validation_media = Vec::with_capacity(draft.media.len());
        for (index, media) in draft.media.iter().enumerate() {
            let public_url = match &media.source {
                MediaSource::RemoteUrl { url } if capabilities.remote_urls => url.clone(),
                MediaSource::RemoteUrl { .. } => {
                    return Err(ProviderError::new(
                        descriptor.id,
                        ProviderErrorKind::Validation,
                        format!(
                            "{} does not support remote reference URLs",
                            descriptor.display_name
                        ),
                    ));
                }
                MediaSource::LocalFile { .. } if capabilities.local_files => {
                    let extension = match media.role.kind() {
                        MediaKind::Image => "png",
                        MediaKind::Video => "mp4",
                        MediaKind::Audio => "mp3",
                    };
                    format!(
                        "https://validation.invalid/reference-{}.{extension}",
                        index + 1
                    )
                }
                MediaSource::LocalFile { .. } => {
                    return Err(ProviderError::new(
                        descriptor.id,
                        ProviderErrorKind::Validation,
                        format!(
                            "{} does not support local reference files; use a public HTTPS URL",
                            descriptor.display_name
                        ),
                    ));
                }
            };
            validation_media.push(StagedMedia::remote(media.role, public_url).map_err(
                |error| {
                    ProviderError::new(
                        descriptor.id.clone(),
                        ProviderErrorKind::Validation,
                        error.to_string(),
                    )
                },
            )?);
        }
        let request = draft.to_video_request(&validation_media).map_err(|error| {
            ProviderError::new(
                descriptor.id,
                ProviderErrorKind::Validation,
                error.to_string(),
            )
        })?;
        self.validate_request(&request).await
    }

    /// Validate a fully resolved provider request without a paid POST. The
    /// default enforces domain and provider ownership; adapters should extend
    /// this with current catalog/schema checks.
    async fn validate_request(&self, request: &VideoRequest) -> Result<(), ProviderError> {
        let descriptor = self.descriptor();
        if request.provider_id != descriptor.id {
            return Err(ProviderError::new(
                descriptor.id,
                ProviderErrorKind::Validation,
                "The request belongs to a different provider",
            ));
        }
        request.validate().map_err(|error| {
            ProviderError::new(
                descriptor.id,
                ProviderErrorKind::Validation,
                error.to_string(),
            )
        })
    }

    /// Resolve a draft media source into the public HTTPS URL accepted by
    /// provider request schemas. Implementations may reuse a supplied receipt
    /// only after verifying its provider, digest, and expiration.
    async fn stage_media(
        &self,
        media: &DraftMedia,
        _cached_receipt: Option<&UploadReceipt>,
        _progress: Option<mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<StagedMedia, ProviderError> {
        media.validate().map_err(|error| {
            ProviderError::new(
                self.descriptor().id,
                ProviderErrorKind::Validation,
                error.to_string(),
            )
        })?;
        match &media.source {
            MediaSource::RemoteUrl { url } => {
                StagedMedia::remote(media.role, url.clone()).map_err(|error| {
                    ProviderError::new(
                        self.descriptor().id,
                        ProviderErrorKind::Validation,
                        error.to_string(),
                    )
                })
            }
            MediaSource::LocalFile { .. } => {
                let descriptor = self.descriptor();
                Err(ProviderError::new(
                    descriptor.id,
                    ProviderErrorKind::Validation,
                    format!(
                        "{} does not support local reference files; use a public HTTPS URL",
                        descriptor.display_name
                    ),
                ))
            }
        }
    }

    async fn validate_credentials(&self) -> Result<ProviderAccount, ProviderError>;

    async fn load_catalog(&self) -> Result<VideoCatalog, ProviderError>;

    async fn quote(&self, request: &VideoRequest) -> Result<CostQuote, ProviderError>;

    /// Perform exactly one potentially billable submission. Implementations
    /// must turn ambiguous transport failures into `SubmissionUncertain` and
    /// must never retry this method internally.
    async fn submit(&self, request: &VideoRequest) -> Result<VideoJob, ProviderError>;

    /// Submit a reviewed request whose quote and provider-staged inputs are
    /// usable only until `submit_before`. Adapters with async preflight work
    /// must recheck this deadline immediately before their paid request.
    async fn submit_prepared(
        &self,
        request: &VideoRequest,
        submit_before: Option<DateTime<Utc>>,
    ) -> Result<VideoJob, ProviderError> {
        if submit_before.is_some() {
            let descriptor = self.descriptor();
            return Err(ProviderError::new(
                descriptor.id,
                ProviderErrorKind::Configuration,
                format!(
                    "{} cannot safely submit a deadline-bound Review; its adapter must implement a post-preflight deadline check",
                    descriptor.display_name
                ),
            ));
        }
        self.submit(request).await
    }

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
