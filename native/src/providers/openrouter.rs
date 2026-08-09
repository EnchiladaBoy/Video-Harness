use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::mpsc;
use url::Url;

use crate::api::{ApiError, ApiErrorKind, DownloadProgress, OpenRouterClient};
use crate::domain::{
    CostQuote, InputReferenceKind, JobLocator, ProviderDescriptor, ProviderId, QuoteConfidence,
    VideoArtifact, VideoCatalog, VideoJob, VideoModel, VideoRequest, estimate_cost,
};

use super::{
    MAX_AUDIO_INPUTS, MAX_IMAGE_INPUTS, MAX_MEDIA_INPUTS_TOTAL, MAX_VIDEO_INPUTS,
    MediaCapabilities, ProviderAccount, ProviderError, ProviderErrorKind, VideoProvider,
    audio_input_requires_visual,
};

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
        let mut quote = estimate_cost(&model, request);
        if request_has_variable_cost_inputs(request) && quote.amount.is_some() {
            quote.exact = false;
            quote.confidence = QuoteConfidence::Estimated;
            quote.basis.push_str(
                "; input media or provider-specific options can affect final usage, so the final charge is authoritative",
            );
        }
        Ok(quote)
    }

    async fn submit(&self, request: &VideoRequest) -> Result<VideoJob, ProviderError> {
        self.validate_submission_request(request).await?;
        let job = self.client.submit(request).await.map_err(map_error)?;
        self.with_artifact(job)
    }

    async fn submit_prepared(
        &self,
        request: &VideoRequest,
        submit_before: Option<DateTime<Utc>>,
    ) -> Result<VideoJob, ProviderError> {
        if submit_before.is_some_and(|deadline| Utc::now() >= deadline) {
            return Err(expired_review());
        }
        // A prepared request can enter through the public direct-generate
        // command as well as Review. Revalidate media-bearing requests before
        // the paid POST, then recheck the Review deadline after safe catalog
        // work. Text-only direct requests retain the legacy lightweight path.
        self.validate_submission_request(request).await?;
        if submit_before.is_some_and(|deadline| Utc::now() >= deadline) {
            return Err(expired_review());
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
    async fn validate_submission_request(
        &self,
        request: &VideoRequest,
    ) -> Result<(), ProviderError> {
        ensure_provider(request)?;
        request.validate().map_err(|error| {
            ProviderError::new(
                ProviderId::openrouter(),
                ProviderErrorKind::Validation,
                error.to_string(),
            )
        })?;
        if request.frame_images.is_empty() && request.input_references.is_empty() {
            return Ok(());
        }
        self.validated_model(request).await.map(|_| ())
    }

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
        validate_reference_policy(&model.id, request)?;
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

fn validate_reference_policy(model_id: &str, request: &VideoRequest) -> Result<(), ProviderError> {
    if !request.frame_images.is_empty() && !request.input_references.is_empty() {
        return Err(validation(
            "OpenRouter frame_images cannot be combined with general input references because the frame inputs take precedence",
        ));
    }
    let images = request
        .input_references
        .iter()
        .filter(|reference| reference.kind == InputReferenceKind::Image)
        .count();
    let videos = request
        .input_references
        .iter()
        .filter(|reference| reference.kind == InputReferenceKind::Video)
        .count();
    let audio = request
        .input_references
        .iter()
        .filter(|reference| reference.kind == InputReferenceKind::Audio)
        .count();
    if images > MAX_IMAGE_INPUTS {
        return Err(validation(format!(
            "OpenRouter accepts at most {MAX_IMAGE_INPUTS} reference images"
        )));
    }
    if videos > MAX_VIDEO_INPUTS {
        return Err(validation(format!(
            "OpenRouter accepts at most {MAX_VIDEO_INPUTS} reference videos"
        )));
    }
    if audio > MAX_AUDIO_INPUTS {
        return Err(validation(format!(
            "OpenRouter accepts at most {MAX_AUDIO_INPUTS} reference audio files"
        )));
    }
    if images + videos + audio > MAX_MEDIA_INPUTS_TOTAL {
        return Err(validation(format!(
            "OpenRouter accepts at most {MAX_MEDIA_INPUTS_TOTAL} reference media items"
        )));
    }
    if audio > 0
        && images + videos == 0
        && audio_input_requires_visual(&ProviderId::openrouter(), model_id)
    {
        return Err(validation(
            "OpenRouter audio references require at least one image or video reference",
        ));
    }
    Ok(())
}

fn request_has_variable_cost_inputs(request: &VideoRequest) -> bool {
    !request.frame_images.is_empty()
        || !request.input_references.is_empty()
        || request
            .adapter_options
            .as_ref()
            .and_then(Value::as_object)
            .is_some_and(|options| !options.is_empty())
}

fn validation(message: impl Into<String>) -> ProviderError {
    ProviderError::new(
        ProviderId::openrouter(),
        ProviderErrorKind::Validation,
        message,
    )
}

fn expired_review() -> ProviderError {
    validation(
        "Review or staged input media expired before submission; Review again before generating",
    )
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::{FrameImage, FrameType, InputReference};

    fn seedance_model() -> VideoModel {
        VideoModel::from_api(&json!({
            "id": "bytedance/seedance-2.0",
            "input_modalities": ["image", "video", "audio"]
        }))
        .expect("Seedance fixture model")
    }

    #[test]
    fn typed_reference_policy_enforces_companions_and_precedence() {
        let model = seedance_model();
        let mut audio_only = VideoRequest::new(&model.id, "fixture prompt").expect("request");
        audio_only.input_references.push(
            InputReference::with_kind("https://media.example/audio.mp3", InputReferenceKind::Audio)
                .expect("audio reference"),
        );
        assert!(validate_reference_policy(&model.id, &audio_only).is_err());

        audio_only.input_references.push(
            InputReference::with_kind("https://media.example/video.mp4", InputReferenceKind::Video)
                .expect("video reference"),
        );
        validate_reference_policy(&model.id, &audio_only).expect("mixed references");

        audio_only.frame_images.push(
            FrameImage::new("https://media.example/frame.png", FrameType::FirstFrame)
                .expect("frame"),
        );
        assert!(validate_reference_policy(&model.id, &audio_only).is_err());
    }

    #[test]
    fn typed_reference_policy_accepts_future_models_and_enforces_limits() {
        let future = VideoModel::from_api(&json!({
            "id": "vendor/future-video-v1",
            "input_modalities": ["image", "video", "audio"]
        }))
        .expect("future fixture model");
        let mut request = VideoRequest::new(&future.id, "fixture prompt").expect("request");
        request.input_references.push(
            InputReference::with_kind("https://media.example/video.mp4", InputReferenceKind::Video)
                .expect("video reference"),
        );
        validate_reference_policy(&future.id, &request)
            .expect("catalog-advertised future model is not blocked by its ID");

        for index in 1..=MAX_VIDEO_INPUTS {
            request.input_references.push(
                InputReference::with_kind(
                    format!("https://media.example/video-{index}.mp4"),
                    InputReferenceKind::Video,
                )
                .expect("video reference"),
            );
        }
        assert!(validate_reference_policy(&future.id, &request).is_err());
    }

    #[test]
    fn provider_options_make_price_confidence_conservative() {
        let mut request = VideoRequest::new("example/video", "fixture prompt").expect("request");
        assert!(!request_has_variable_cost_inputs(&request));
        request.adapter_options = Some(json!({
            "options": {"byteplus": {"parameters": {"quality": "high"}}}
        }));
        assert!(request_has_variable_cost_inputs(&request));

        request.adapter_options = Some(json!({}));
        assert!(!request_has_variable_cost_inputs(&request));
    }
}
