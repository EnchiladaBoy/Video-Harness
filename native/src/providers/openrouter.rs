use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use url::Url;

use crate::api::{ApiError, ApiErrorKind, DownloadProgress, OpenRouterClient};
use crate::domain::{
    CostQuote, JobLocator, ProviderDescriptor, ProviderId, VideoArtifact, VideoCatalog, VideoJob,
    VideoModel, VideoRequest, estimate_cost,
};

use super::{MediaCapabilities, ProviderAccount, ProviderError, ProviderErrorKind, VideoProvider};

#[derive(Debug)]
pub struct OpenRouterProvider {
    client: Arc<OpenRouterClient>,
    models: RwLock<BTreeMap<String, VideoModel>>,
}

impl OpenRouterProvider {
    pub fn new(client: OpenRouterClient) -> Self {
        Self {
            client: Arc::new(client),
            models: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn from_shared(client: Arc<OpenRouterClient>) -> Self {
        Self {
            client,
            models: RwLock::new(BTreeMap::new()),
        }
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

    fn media_capabilities(&self) -> MediaCapabilities {
        // OpenRouter's video request schema accepts stable public HTTPS URLs.
        // It does not currently document local input upload/staging for video.
        MediaCapabilities::urls_only()
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
        let catalog = self.client.list_video_models().await.map_err(map_error)?;
        let mut models = self
            .models
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *models = catalog
            .models
            .iter()
            .map(|model| (model.id.clone(), model.clone()))
            .collect();
        drop(models);
        Ok(catalog)
    }

    async fn validate_request(&self, request: &VideoRequest) -> Result<(), ProviderError> {
        self.validated_model(request).await.map(|_| ())
    }

    async fn quote(&self, request: &VideoRequest) -> Result<CostQuote, ProviderError> {
        let model = self.validated_model(request).await?;
        Ok(estimate_cost(&model, request))
    }

    async fn submit(&self, request: &VideoRequest) -> Result<VideoJob, ProviderError> {
        ensure_provider(request)?;
        let job = self.client.submit(request).await.map_err(map_error)?;
        self.with_artifact(job)
    }

    async fn submit_prepared(
        &self,
        request: &VideoRequest,
        submit_before: Option<DateTime<Utc>>,
    ) -> Result<VideoJob, ProviderError> {
        ensure_provider(request)?;
        if submit_before.is_some_and(|deadline| Utc::now() >= deadline) {
            return Err(ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::Validation,
                "Review or staged input media expired before submission; Review again before generating",
            ));
        }
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
    async fn ensure_model(&self, model_id: &str) -> Result<VideoModel, ProviderError> {
        if let Some(model) = self
            .models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(model_id)
            .cloned()
        {
            return Ok(model);
        }
        let catalog = self.load_catalog().await?;
        catalog.find(model_id).cloned().ok_or_else(|| {
            ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::Validation,
                format!("OpenRouter model {model_id} is not in the current catalog"),
            )
        })
    }

    async fn validated_model(&self, request: &VideoRequest) -> Result<VideoModel, ProviderError> {
        ensure_provider(request)?;
        request.validate().map_err(|error| {
            ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::Validation,
                error.to_string(),
            )
        })?;
        let model = self.ensure_model(&request.model).await?;
        let problems = model.supports_request(request);
        if !problems.is_empty() {
            return Err(ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::Validation,
                format!(
                    "OpenRouter model {} does not support this request: {}",
                    request.model,
                    problems.join("; ")
                ),
            ));
        }
        Ok(model)
    }

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
