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
    CostQuote, DraftMedia, FAL_PROVIDER_ID, GenerationDraft, JobLocator, MediaKind, MediaSource,
    OPENROUTER_PROVIDER_ID, ProviderDescriptor, ProviderId, StagedMedia, UploadReceipt,
    VideoArtifact, VideoCatalog, VideoJob, VideoRequest,
};

/// Conservative cross-provider fallback limits for one video request. A
/// provider schema may advertise a higher model-specific maximum where the
/// adapter explicitly supports it.
pub const MAX_MEDIA_INPUTS_TOTAL: usize = 12;
pub const MAX_IMAGE_INPUTS: usize = 9;
pub const MAX_VIDEO_INPUTS: usize = 3;
pub const MAX_AUDIO_INPUTS: usize = 3;

/// Whether audio input for this provider/model must be accompanied by at
/// least one image, frame image, or video input.
pub fn audio_input_requires_visual(provider_id: &ProviderId, model_id: &str) -> bool {
    match provider_id.as_str() {
        OPENROUTER_PROVIDER_ID => true,
        FAL_PROVIDER_ID => is_seedance_2_model_id(model_id),
        _ => false,
    }
}

pub(crate) fn is_seedance_2_model_id(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    model_id.starts_with("bytedance/seedance-2.0") || model_id.contains("/seedance-2.0/")
}

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

/// Who can retrieve media after a stager has uploaded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedVisibility {
    /// The staged object is reachable without authentication by anyone who
    /// has its capability URL.
    PublicByLink,
}

/// Stable metadata for a service that turns local media into provider-ready
/// public HTTPS URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaStagerDescriptor {
    pub id: String,
    pub display_name: String,
    /// Credential provider needed by this stager, if any.
    pub credential_provider: Option<ProviderId>,
    pub visibility: StagedVisibility,
    /// Requested lifetime of newly staged objects.
    pub retention: Option<Duration>,
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

/// Provider-independent boundary for uploading local input media.
///
/// Generator adapters consume the resulting public HTTPS URL but do not need
/// to own the upload service or its credential. Remote URLs bypass this
/// boundary and should be passed through by the caller.
#[async_trait]
pub trait MediaStager: Send + Sync {
    fn descriptor(&self) -> MediaStagerDescriptor;

    async fn stage_local(
        &self,
        media: &DraftMedia,
        cached_receipt: Option<&UploadReceipt>,
        progress: Option<mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<StagedMedia, ProviderError>;
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
        self.validate_draft_with_local_staging(draft, false).await
    }

    /// Validate an editable draft when the caller has explicitly resolved an
    /// independent local-media stager. Passing `false` is identical to
    /// [`VideoProvider::validate_draft`]; passing `true` permits local files to
    /// use inert public-HTTPS placeholders when this generator accepts remote
    /// reference URLs. This method never uploads bytes.
    async fn validate_draft_with_local_staging(
        &self,
        draft: &GenerationDraft,
        local_staging_available: bool,
    ) -> Result<(), ProviderError> {
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
                MediaSource::LocalFile { .. }
                    if capabilities.local_files
                        || (local_staging_available && capabilities.remote_urls) =>
                {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reusable_media_limits_match_adapter_policy() {
        assert_eq!(MAX_MEDIA_INPUTS_TOTAL, 12);
        assert_eq!(MAX_IMAGE_INPUTS, 9);
        assert_eq!(MAX_VIDEO_INPUTS, 3);
        assert_eq!(MAX_AUDIO_INPUTS, 3);
    }

    #[test]
    fn audio_visual_companion_policy_is_provider_and_model_specific() {
        assert!(audio_input_requires_visual(
            &ProviderId::openrouter(),
            "any/audio-capable-model"
        ));
        assert!(audio_input_requires_visual(
            &ProviderId::fal(),
            "bytedance/seedance-2.0/reference-to-video"
        ));
        assert!(audio_input_requires_visual(
            &ProviderId::fal(),
            "FAL-AI/SEEDANCE-2.0/REFERENCE-TO-VIDEO"
        ));
        assert!(!audio_input_requires_visual(
            &ProviderId::fal(),
            "fal-ai/kling-video"
        ));
        assert!(!audio_input_requires_visual(
            &ProviderId::new("future-provider").expect("provider id"),
            "future-model"
        ));
    }
}
