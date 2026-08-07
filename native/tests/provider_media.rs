use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use reqwest::{Method, StatusCode};
use secrecy::SecretString;
use serde_json::json;
use tempfile::tempdir;
use url::Url;
use video_harness::api::{
    ClientOptions, HttpExecutor, HttpRequest, HttpResponse, OpenRouterClient, TransportError,
};
use video_harness::domain::{DraftMedia, GenerationDraft, MediaRole, ProviderId, VideoRequest};
use video_harness::providers::openrouter::OpenRouterProvider;
use video_harness::providers::{ProviderErrorKind, VideoProvider};

#[derive(Default)]
struct CatalogExecutor {
    methods: Mutex<Vec<Method>>,
}

#[async_trait]
impl HttpExecutor for CatalogExecutor {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.methods
            .lock()
            .expect("method lock")
            .push(request.method);
        HttpResponse::from_json(
            StatusCode::OK,
            request.url,
            &json!({
                "data": [{
                    "id": "fixture/video",
                    "name": "Fixture video",
                    "supported_durations": [4, 8]
                }]
            }),
        )
        .map_err(|_| TransportError)
    }
}

#[tokio::test]
async fn openrouter_accepts_public_urls_and_explicitly_blocks_local_media() {
    let provider = OpenRouterProvider::new(
        OpenRouterClient::from_key("openrouter-test-placeholder").expect("fixture client"),
    );
    let capabilities = provider.media_capabilities();
    assert!(capabilities.remote_urls);
    assert!(!capabilities.local_files);
    assert!(!capabilities.uploaded_files_public);
    assert!(capabilities.upload_retention.is_none());

    let remote = DraftMedia::remote("https://images.example/reference.png", MediaRole::Reference)
        .expect("remote reference");
    let staged = provider
        .stage_media(&remote, None, None)
        .await
        .expect("OpenRouter URL reference");
    assert_eq!(staged.public_url, "https://images.example/reference.png");
    assert!(staged.receipt.is_none());

    let directory = tempdir().expect("temporary media");
    let path = directory.path().join("reference.png");
    fs::write(&path, b"\x89PNG\r\n\x1a\nfixture").expect("fixture local media");
    let error = provider
        .stage_media(
            &DraftMedia::local(path.clone(), MediaRole::Reference),
            None,
            None,
        )
        .await
        .expect_err("OpenRouter local media must be blocked before Review");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(
        error
            .message
            .contains("does not support local reference files")
    );

    let mut draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "A local-reference validation fixture",
    )
    .expect("fixture draft");
    draft
        .media
        .push(DraftMedia::local(path, MediaRole::Reference));
    let error = provider
        .validate_draft(&draft)
        .await
        .expect_err("dry validation must reject local media before catalog transport");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(
        error
            .message
            .contains("does not support local reference files")
    );
}

#[tokio::test]
async fn openrouter_dry_validation_accepts_local_placeholders_only_with_explicit_staging() {
    let executor = Arc::new(CatalogExecutor::default());
    let provider = OpenRouterProvider::new(
        OpenRouterClient::with_executor(
            SecretString::from("openrouter-test-placeholder".to_owned()),
            ClientOptions {
                base_url: Url::parse("https://api.fixture.invalid/api/v1")
                    .expect("fixture base URL"),
                max_retries: 0,
                backoff_base: Duration::ZERO,
                ..ClientOptions::default()
            },
            executor.clone(),
        )
        .expect("fixture client"),
    );
    let directory = tempdir().expect("temporary media");
    let path = directory.path().join("reference.png");
    fs::write(&path, b"\x89PNG\r\n\x1a\nfixture").expect("fixture local media");
    let mut draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "fixture/video",
        "An explicitly staged local-reference fixture",
    )
    .expect("fixture draft");
    draft
        .media
        .push(DraftMedia::local(path, MediaRole::Reference));

    let error = provider
        .validate_draft_with_local_staging(&draft, false)
        .await
        .expect_err("local staging must be explicitly available");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(
        error
            .message
            .contains("does not support local reference files")
    );
    assert!(
        executor.methods.lock().expect("method lock").is_empty(),
        "URL-only validation must reject before catalog transport"
    );

    provider
        .validate_draft_with_local_staging(&draft, true)
        .await
        .expect("an explicit stager permits inert local placeholders");
    assert_eq!(
        *executor.methods.lock().expect("method lock"),
        vec![Method::GET],
        "dry validation may fetch the catalog but must never upload or POST"
    );
}

#[tokio::test]
async fn openrouter_dry_validation_enforces_catalog_capabilities_without_posting() {
    let executor = Arc::new(CatalogExecutor::default());
    let provider = OpenRouterProvider::new(
        OpenRouterClient::with_executor(
            SecretString::from("openrouter-test-placeholder".to_owned()),
            ClientOptions {
                base_url: Url::parse("https://api.fixture.invalid/api/v1")
                    .expect("fixture base URL"),
                max_retries: 0,
                backoff_base: Duration::ZERO,
                ..ClientOptions::default()
            },
            executor.clone(),
        )
        .expect("fixture client"),
    );
    let mut draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "fixture/video",
        "An unsupported-duration fixture",
    )
    .expect("fixture draft");
    draft.duration = Some(5);

    let error = provider
        .validate_draft(&draft)
        .await
        .expect_err("catalog capability mismatch must fail before submission");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("duration 5s is not supported"));
    assert_eq!(
        *executor.methods.lock().expect("method lock"),
        vec![Method::GET],
        "dry validation may fetch the public catalog but must never POST"
    );
}

#[tokio::test]
async fn openrouter_expired_review_is_rejected_immediately_without_posting() {
    let executor = Arc::new(CatalogExecutor::default());
    let provider = OpenRouterProvider::new(
        OpenRouterClient::with_executor(
            SecretString::from("openrouter-test-placeholder".to_owned()),
            ClientOptions {
                base_url: Url::parse("https://api.fixture.invalid/api/v1")
                    .expect("fixture base URL"),
                max_retries: 0,
                backoff_base: Duration::ZERO,
                ..ClientOptions::default()
            },
            executor.clone(),
        )
        .expect("fixture client"),
    );
    let request =
        VideoRequest::new("fixture/video", "An expired Review fixture").expect("fixture request");

    let error = provider
        .submit_prepared(&request, Some(Utc::now() - TimeDelta::seconds(1)))
        .await
        .expect_err("an expired Review must not reach the paid transport");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("expired before submission"));
    assert!(
        executor.methods.lock().expect("method lock").is_empty(),
        "deadline rejection must occur before any provider request"
    );
}
