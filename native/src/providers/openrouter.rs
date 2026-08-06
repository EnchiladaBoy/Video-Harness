use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use url::Url;

use crate::api::{ApiError, ApiErrorKind, DownloadProgress, OpenRouterClient};
use crate::domain::{
    CostQuote, JobLocator, ProviderDescriptor, ProviderId, VideoArtifact, VideoCatalog, VideoJob,
    VideoRequest, estimate_cost,
};

use super::{ProviderAccount, ProviderError, ProviderErrorKind, VideoProvider};

#[derive(Debug)]
pub struct OpenRouterProvider {
    client: Arc<OpenRouterClient>,
}

impl OpenRouterProvider {
    pub fn new(client: OpenRouterClient) -> Self {
        Self {
            client: Arc::new(client),
        }
    }

    pub fn from_shared(client: Arc<OpenRouterClient>) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Arc<OpenRouterClient> {
        &self.client
    }
}

#[async_trait]
impl VideoProvider for OpenRouterProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::openrouter(),
            display_name: "OpenRouter".into(),
            website: "https://openrouter.ai".into(),
        }
    }

    async fn validate_credentials(&self) -> Result<ProviderAccount, ProviderError> {
        let info = self.client.validate_key().await.map_err(map_error)?;
        Ok(ProviderAccount {
            label: info.label,
            balance: info.limit_remaining,
            raw: info.raw,
        })
    }

    async fn load_catalog(&self) -> Result<VideoCatalog, ProviderError> {
        self.client.list_video_models().await.map_err(map_error)
    }

    async fn quote(&self, request: &VideoRequest) -> Result<CostQuote, ProviderError> {
        ensure_provider(request)?;
        let catalog = self.load_catalog().await?;
        let model = catalog.find(&request.model).ok_or_else(|| {
            ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::Validation,
                format!(
                    "OpenRouter model {} is not in the current catalog",
                    request.model
                ),
            )
        })?;
        Ok(estimate_cost(model, request))
    }

    async fn submit(&self, request: &VideoRequest) -> Result<VideoJob, ProviderError> {
        ensure_provider(request)?;
        let job = self.client.submit(request).await.map_err(map_error)?;
        self.with_artifact(job)
    }

    async fn poll(&self, locator: &JobLocator) -> Result<VideoJob, ProviderError> {
        let JobLocator::OpenRouter { polling_url } = locator else {
            return Err(ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::Validation,
                "OpenRouter cannot poll a locator owned by another provider",
            ));
        };
        let job = self.client.poll(polling_url).await.map_err(map_error)?;
        self.with_artifact(job)
    }

    async fn download(
        &self,
        artifact: &VideoArtifact,
        destination: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadProgress>>,
    ) -> Result<PathBuf, ProviderError> {
        let url = Url::parse(&artifact.url).map_err(|_| {
            ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::UnsafeEndpoint,
                "OpenRouter returned an invalid artifact URL",
            )
        })?;
        self.client
            .download(&url, destination, progress)
            .await
            .map_err(map_error)
    }
}

impl OpenRouterProvider {
    fn with_artifact(&self, mut job: VideoJob) -> Result<VideoJob, ProviderError> {
        if job.status == crate::domain::JobStatus::Completed && job.artifacts.is_empty() {
            let url = self.client.content_url(&job.id, 0).map_err(map_error)?;
            job.artifacts
                .push(VideoArtifact::new(url.as_str(), 0).map_err(|error| {
                    ProviderError::new(
                        ProviderId::openrouter(),
                        ProviderErrorKind::Response,
                        error.to_string(),
                    )
                })?);
        }
        Ok(job)
    }
}

fn ensure_provider(request: &VideoRequest) -> Result<(), ProviderError> {
    if request.provider_id != ProviderId::openrouter() {
        return Err(ProviderError::new(
            ProviderId::openrouter(),
            ProviderErrorKind::Validation,
            "The request belongs to a different provider",
        ));
    }
    Ok(())
}

fn map_error(error: ApiError) -> ProviderError {
    let kind = match error.kind {
        ApiErrorKind::Authentication => ProviderErrorKind::Authentication,
        ApiErrorKind::InsufficientCredits => ProviderErrorKind::InsufficientCredits,
        ApiErrorKind::RequestValidation | ApiErrorKind::ResourceNotFound => {
            ProviderErrorKind::Validation
        }
        ApiErrorKind::ContentPolicy => ProviderErrorKind::ContentPolicy,
        ApiErrorKind::RateLimit => ProviderErrorKind::RateLimit,
        ApiErrorKind::Provider => ProviderErrorKind::Unavailable,
        ApiErrorKind::Network => ProviderErrorKind::Network,
        ApiErrorKind::SubmissionUncertain => ProviderErrorKind::SubmissionUncertain,
        ApiErrorKind::ResponseFormat => ProviderErrorKind::Response,
        ApiErrorKind::UnsafeUrl => ProviderErrorKind::UnsafeEndpoint,
        ApiErrorKind::Download => ProviderErrorKind::Download,
        ApiErrorKind::Configuration => ProviderErrorKind::Configuration,
    };
    ProviderError {
        provider_id: ProviderId::openrouter(),
        kind,
        message: error.message,
        status_code: error.status_code,
        code: error.code,
        details: error.details,
        retry_after: error.retry_after,
    }
}
