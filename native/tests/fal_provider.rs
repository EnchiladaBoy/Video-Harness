use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{TimeDelta, Utc};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, LOCATION};
use reqwest::{Method, StatusCode};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::tempdir;
use url::Url;
use video_harness::api::{HttpExecutor, HttpRequest, HttpResponse, TransportError};
use video_harness::domain::{
    DraftMedia, GenerationDraft, JobLocator, JobStatus, MediaRole, ProviderId, QuoteConfidence,
    VideoRequest,
};
use video_harness::providers::fal::{FalOptions, FalProvider, FalUploadExecutor};
use video_harness::providers::{
    MediaStager, ProviderError, ProviderErrorKind, StagedVisibility, UploadProgress, VideoProvider,
};

const KEY: &str = "fal-test-placeholder";
const PNG_FIXTURE: &[u8] = b"\x89PNG\r\n\x1a\nfixture image bytes";

#[derive(Clone)]
struct CapturedRequest {
    method: Method,
    url: Url,
    authorized: bool,
    object_lifecycle: Option<String>,
    body: Option<Value>,
}

enum Reply {
    Json(StatusCode, Value),
    DelayedJson(Duration, StatusCode, Value),
    Redirect(String),
    Bytes(Vec<u8>),
    InterruptedBody,
}

#[derive(Default)]
struct ScriptedExecutor {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<CapturedRequest>>,
    create_during_download: Mutex<Option<(PathBuf, Vec<u8>)>>,
}

impl ScriptedExecutor {
    fn with_replies(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            ..Self::default()
        })
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("request lock").clone()
    }

    fn create_destination_during_download(&self, path: PathBuf, contents: &[u8]) {
        *self.create_during_download.lock().expect("collision lock") =
            Some((path, contents.to_vec()));
    }
}

#[async_trait]
impl HttpExecutor for ScriptedExecutor {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.requests
            .lock()
            .expect("request lock")
            .push(CapturedRequest {
                method: request.method.clone(),
                url: request.url.clone(),
                authorized: request.headers.contains_key(AUTHORIZATION),
                object_lifecycle: request
                    .headers
                    .get("x-fal-object-lifecycle")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned),
                body: request.json_body.clone(),
            });
        let reply = { self.replies.lock().expect("reply lock").pop_front() };
        match reply {
            Some(Reply::Json(status, value)) => {
                HttpResponse::from_json(status, request.url, &value).map_err(|_| TransportError)
            }
            Some(Reply::DelayedJson(delay, status, value)) => {
                tokio::time::sleep(delay).await;
                HttpResponse::from_json(status, request.url, &value).map_err(|_| TransportError)
            }
            Some(Reply::Redirect(location)) => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    LOCATION,
                    HeaderValue::from_str(&location).map_err(|_| TransportError)?,
                );
                Ok(HttpResponse::from_bytes(
                    StatusCode::FOUND,
                    request.url,
                    headers,
                    Bytes::new(),
                ))
            }
            Some(Reply::Bytes(body)) => {
                if let Some((path, contents)) = self
                    .create_during_download
                    .lock()
                    .expect("collision lock")
                    .take()
                {
                    fs::write(path, contents).expect("create racing destination");
                }
                Ok(HttpResponse::from_bytes(
                    StatusCode::OK,
                    request.url,
                    HeaderMap::new(),
                    Bytes::from(body),
                ))
            }
            Some(Reply::InterruptedBody) => Ok(HttpResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                final_url: request.url,
                body: Box::pin(futures_util::stream::once(async { Err(TransportError) })),
            }),
            None => Err(TransportError),
        }
    }
}

#[derive(Debug, Clone)]
struct CapturedUpload {
    url: Url,
    path: PathBuf,
    content_type: String,
    size_bytes: u64,
    multipart: bool,
}

#[derive(Default)]
struct ScriptedUploadExecutor {
    uploads: Mutex<Vec<CapturedUpload>>,
    replace_during_upload: Mutex<Option<Vec<u8>>>,
}

impl ScriptedUploadExecutor {
    fn uploads(&self) -> Vec<CapturedUpload> {
        self.uploads.lock().expect("upload lock").clone()
    }

    fn replace_file_during_upload(&self, contents: &[u8]) {
        *self
            .replace_during_upload
            .lock()
            .expect("upload mutation lock") = Some(contents.to_vec());
    }
}

#[async_trait]
impl FalUploadExecutor for ScriptedUploadExecutor {
    async fn upload(
        &self,
        upload_url: &Url,
        path: &std::path::Path,
        content_type: &str,
        size_bytes: u64,
        multipart: bool,
        progress: Option<tokio::sync::mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<(), ProviderError> {
        self.uploads
            .lock()
            .expect("upload lock")
            .push(CapturedUpload {
                url: upload_url.clone(),
                path: path.to_owned(),
                content_type: content_type.to_owned(),
                size_bytes,
                multipart,
            });
        if let Some(contents) = self
            .replace_during_upload
            .lock()
            .expect("upload mutation lock")
            .take()
        {
            fs::write(path, contents).expect("replace fixture during upload");
        }
        if let Some(progress) = progress {
            let _ = progress.send(UploadProgress {
                sent: size_bytes,
                total: size_bytes,
            });
        }
        Ok(())
    }
}

fn provider(executor: Arc<ScriptedExecutor>) -> FalProvider {
    FalProvider::with_executor(
        SecretString::from(KEY.to_owned()),
        FalOptions {
            max_retries: 0,
            backoff_base: Duration::ZERO,
            ..FalOptions::default()
        },
        executor,
    )
    .expect("fixture fal provider")
}

fn provider_with_upload(
    executor: Arc<ScriptedExecutor>,
    upload_executor: Arc<ScriptedUploadExecutor>,
) -> FalProvider {
    FalProvider::with_executors(
        SecretString::from(KEY.to_owned()),
        FalOptions {
            max_retries: 0,
            backoff_base: Duration::ZERO,
            ..FalOptions::default()
        },
        executor,
        upload_executor,
    )
    .expect("fixture fal provider")
}

fn discovery_model() -> Value {
    json!({
        "endpoint_id": "fal-ai/fixture/text-to-video",
        "metadata": {
            "display_name": "Fixture Video",
            "description": "Offline fixture",
            "category": "text-to-video",
            "status": "active",
            "updated_at": "2026-08-06T00:00:00Z"
        }
    })
}

fn expanded_model() -> Value {
    let mut model = discovery_model();
    model["openapi"] = json!({
        "openapi": "3.0.0",
        "paths": {
            "/": {
                "post": {
                    "requestBody": {
                        "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/Input"}
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "content": {
                                "application/json": {
                                    "schema": {"$ref": "#/components/schemas/Output"}
                                }
                            }
                        }
                    }
                }
            }
        },
        "components": {
            "schemas": {
                "Input": {
                    "type": "object",
                    "required": ["prompt"],
                    "additionalProperties": false,
                    "properties": {
                        "prompt": {"type": "string"},
                        "duration": {"type": "integer", "enum": [4, 8]},
                        "aspect_ratio": {"type": "string", "enum": ["16:9", "9:16"]},
                        "seed": {"type": "integer"},
                        "custom_strength": {
                            "allOf": [
                                {"type": "number", "minimum": 0},
                                {"maximum": 1}
                            ]
                        },
                        "mode": {"type": "string", "const": "standard"},
                        "tags": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 2,
                            "items": {"type": "string", "minLength": 2, "maxLength": 4}
                        }
                    }
                },
                "Output": {
                    "type": "object",
                    "required": ["video"],
                    "properties": {
                        "video": {"$ref": "#/components/schemas/File"}
                    }
                },
                "File": {
                    "type": "object",
                    "required": ["url"],
                    "properties": {
                        "url": {"type": "string"},
                        "content_type": {"type": "string"}
                    }
                }
            }
        }
    });
    model
}

fn expanded_string_duration_model() -> Value {
    let mut model = expanded_model();
    model["openapi"]["components"]["schemas"]["Input"]["properties"]["duration"] =
        json!({"type": "string", "enum": ["4", "8"]});
    model
}

fn catalog_replies() -> [Reply; 5] {
    [
        Reply::Json(
            StatusCode::OK,
            json!({"models": [discovery_model()], "next_cursor": null, "has_more": false}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({"models": [], "next_cursor": null, "has_more": false}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({"models": [], "next_cursor": null, "has_more": false}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({"models": [], "next_cursor": null, "has_more": false}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({"models": [expanded_model()], "next_cursor": null, "has_more": false}),
        ),
    ]
}

fn request() -> VideoRequest {
    let mut request = VideoRequest::for_provider(
        ProviderId::fal(),
        "fal-ai/fixture/text-to-video",
        "A harmless offline fixture",
    )
    .expect("fixture request");
    request.duration = Some(4);
    request.aspect_ratio = Some("16:9".into());
    request.adapter_options = Some(json!({"custom_strength": 0.75}));
    request
}

#[tokio::test]
async fn catalog_queue_pricing_and_download_use_the_correct_auth_scope() {
    let mut replies = Vec::from(catalog_replies());
    replies.extend([
        Reply::Json(
            StatusCode::OK,
            json!({
                "prices": [{
                    "endpoint_id": "fal-ai/fixture/text-to-video",
                    "unit_price": "0.25",
                    "unit": "video_second",
                    "currency": "AUD"
                }]
            }),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({
                "request_id": "request-fixture",
                "status_url": "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-fixture/status",
                "response_url": "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-fixture/response"
            }),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({
                "status": "COMPLETED",
                "request_id": "request-fixture",
                "response_url": "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-fixture/response"
            }),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({
                "data": {
                    "video": {
                        "url": "https://cdn.fal.media/fixture.mp4",
                        "content_type": "video/mp4"
                    }
                }
            }),
        ),
        Reply::Bytes(b"fixture video bytes".to_vec()),
    ]);
    let executor = ScriptedExecutor::with_replies(replies);
    let provider = provider(executor.clone());

    let catalog = provider.load_catalog().await.expect("public fal catalog");
    assert_eq!(catalog.provider_id, ProviderId::fal());
    assert_eq!(catalog.models.len(), 1);
    let model = &catalog.models[0];
    assert_eq!(model.supported_durations, vec![4, 8]);
    assert_eq!(model.field_map["prompt"], "prompt");
    assert!(model.input_schema.is_some());

    let request = request();
    let quote = provider.quote(&request).await.expect("fal quote");
    assert_eq!(
        quote
            .amount
            .and_then(|amount| amount.to_string().parse::<f64>().ok()),
        Some(1.0)
    );
    assert_eq!(quote.currency, "AUD");
    assert_eq!(quote.confidence, QuoteConfidence::Estimated);

    let submitted = provider.submit(&request).await.expect("fal submit");
    assert_eq!(submitted.id, "request-fixture");
    let completed = provider
        .poll(&submitted.locator)
        .await
        .expect("fal status and result");
    assert_eq!(completed.status, JobStatus::Completed);
    assert_eq!(completed.artifacts.len(), 1);

    let directory = tempdir().expect("temporary output");
    let destination = directory.path().join("fixture.mp4");
    provider
        .download(&completed.artifacts[0], &destination, None)
        .await
        .expect("anonymous CDN download");
    assert_eq!(
        fs::read(&destination).expect("saved video"),
        b"fixture video bytes"
    );

    let requests = executor.requests();
    assert_eq!(requests.len(), 10);
    assert!(requests[..5].iter().all(|request| !request.authorized));
    assert!(requests[5..9].iter().all(|request| request.authorized));
    assert!(!requests[9].authorized);
    assert_eq!(requests[6].method, Method::POST);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    let body = requests[6].body.as_ref().expect("paid POST body");
    assert_eq!(body["prompt"], "A harmless offline fixture");
    assert_eq!(body["duration"], 4);
    assert_eq!(body["custom_strength"], 0.75);
    assert_eq!(requests[9].url.host_str(), Some("cdn.fal.media"));
    assert_eq!(
        requests[8].url.path(),
        "/fal-ai/fixture/text-to-video/requests/request-fixture/response"
    );
}

#[tokio::test]
async fn interrupted_paid_response_is_uncertain_and_never_retried() {
    let mut replies = Vec::from(catalog_replies());
    replies.push(Reply::InterruptedBody);
    replies.push(Reply::Json(
        StatusCode::OK,
        json!({"request_id": "must-not-be-read"}),
    ));
    let executor = ScriptedExecutor::with_replies(replies);
    let provider = provider(executor.clone());
    provider.load_catalog().await.expect("cache fixture model");

    let error = provider
        .submit(&request())
        .await
        .expect_err("interrupted paid response must be uncertain");
    assert_eq!(error.kind, ProviderErrorKind::SubmissionUncertain);
    let requests = executor.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    assert_eq!(requests.len(), 6);
}

#[tokio::test]
async fn delayed_schema_resolution_consuming_media_margin_never_reaches_paid_post() {
    let schema_delay = Duration::from_millis(250);
    let executor = ScriptedExecutor::with_replies([
        Reply::DelayedJson(
            schema_delay,
            StatusCode::OK,
            json!({
                "models": [expanded_model()],
                "next_cursor": null,
                "has_more": false
            }),
        ),
        Reply::Json(StatusCode::OK, json!({"request_id": "must-not-be-read"})),
    ]);
    let provider = provider(executor.clone());
    let submit_before = Utc::now() + TimeDelta::milliseconds(100);
    let started = Instant::now();

    let error = provider
        .submit_prepared(&request(), Some(submit_before))
        .await
        .expect_err("expired staged media must stop after schema lookup and before paid POST");
    assert!(started.elapsed() >= schema_delay);
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("too close to expiring"));
    let requests = executor.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::GET);
    assert_eq!(requests[0].url.path(), "/v1/models");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        0
    );
}

#[tokio::test]
async fn paid_5xx_is_uncertain_and_never_retried_even_with_retries_configured() {
    let mut replies = Vec::from(catalog_replies());
    replies.extend([
        Reply::Json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"message": "runner failed after accepting the request"}}),
        ),
        Reply::Json(StatusCode::OK, json!({"request_id": "must-not-be-read"})),
    ]);
    let executor = ScriptedExecutor::with_replies(replies);
    let provider = FalProvider::with_executor(
        SecretString::from(KEY.to_owned()),
        FalOptions {
            max_retries: 5,
            backoff_base: Duration::ZERO,
            ..FalOptions::default()
        },
        executor.clone(),
    )
    .expect("fixture fal provider");
    provider.load_catalog().await.expect("cache fixture model");

    let error = provider
        .submit(&request())
        .await
        .expect_err("a paid 5xx response cannot prove rejection");
    assert_eq!(error.kind, ProviderErrorKind::SubmissionUncertain);
    assert_eq!(error.status_code, Some(503));
    assert!(!error.retryable());
    assert!(error.message.contains("may exist"));
    let requests = executor.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    assert_eq!(requests.len(), 6);
}

#[tokio::test]
async fn fal_nonstandard_4xx_cannot_prove_rejection_and_is_not_retried() {
    let mut replies = Vec::from(catalog_replies());
    replies.extend([
        Reply::Json(
            StatusCode::from_u16(499).expect("fixture status"),
            json!({"error": {"message": "proxy closed after forwarding the request"}}),
        ),
        Reply::Json(StatusCode::OK, json!({"request_id": "must-not-be-read"})),
    ]);
    let executor = ScriptedExecutor::with_replies(replies);
    let provider = FalProvider::with_executor(
        SecretString::from(KEY.to_owned()),
        FalOptions {
            max_retries: 5,
            backoff_base: Duration::ZERO,
            ..FalOptions::default()
        },
        executor.clone(),
    )
    .expect("fixture fal provider");
    provider.load_catalog().await.expect("cache fixture model");

    let error = provider
        .submit(&request())
        .await
        .expect_err("an unknown proxy status cannot prove paid-request rejection");
    assert_eq!(error.kind, ProviderErrorKind::SubmissionUncertain);
    assert_eq!(error.status_code, Some(499));
    assert!(!error.retryable());
    assert_eq!(
        executor
            .requests()
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
}

#[tokio::test]
async fn fal_paid_submission_preserves_deterministic_4xx_rejection() {
    let mut replies = Vec::from(catalog_replies());
    replies.push(Reply::Json(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"error": {"message": "fixture validation rejection"}}),
    ));
    let executor = ScriptedExecutor::with_replies(replies);
    let provider = provider(executor.clone());
    provider.load_catalog().await.expect("cache fixture model");

    let error = provider
        .submit(&request())
        .await
        .expect_err("422 proves the paid request was rejected");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert_eq!(error.status_code, Some(422));
    assert_eq!(
        executor
            .requests()
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
}

#[tokio::test]
async fn advanced_json_cannot_override_common_fields() {
    let executor = ScriptedExecutor::with_replies(catalog_replies());
    let provider = provider(executor.clone());
    provider.load_catalog().await.expect("cache fixture model");
    let mut request = request();
    request.adapter_options = Some(json!({"prompt": "hidden override"}));
    let error = provider
        .submit(&request)
        .await
        .expect_err("common-field override must fail before POST");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method == Method::GET)
    );
}

#[tokio::test]
async fn schema_constraints_reject_advanced_json_before_paid_post() {
    let executor = ScriptedExecutor::with_replies(catalog_replies());
    let provider = provider(executor.clone());
    provider.load_catalog().await.expect("cache fixture model");

    for options in [
        json!({"custom_strength": 2.0}),
        json!({"custom_strength": 0.5, "mode": "unsafe"}),
        json!({"custom_strength": 0.5, "tags": []}),
        json!({"custom_strength": 0.5, "tags": ["x"]}),
    ] {
        let mut request = request();
        request.adapter_options = Some(options);
        let error = provider
            .submit(&request)
            .await
            .expect_err("schema constraint must fail before POST");
        assert_eq!(error.kind, ProviderErrorKind::Validation);
    }
    let mut unsupported = request();
    unsupported.resolution = Some("1080p".into());
    let error = provider
        .submit(&unsupported)
        .await
        .expect_err("unsupported common control must not be omitted");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method == Method::GET)
    );
}

#[tokio::test]
async fn dry_validation_rejects_advanced_json_before_local_reference_upload() {
    let directory = tempdir().expect("temporary media");
    let media_path = directory.path().join("reference.png");
    fs::write(&media_path, PNG_FIXTURE).expect("fixture media");
    let executor = ScriptedExecutor::with_replies(catalog_replies());
    let upload_executor = Arc::new(ScriptedUploadExecutor::default());
    let provider = provider_with_upload(executor.clone(), upload_executor.clone());
    provider.load_catalog().await.expect("cache fixture model");

    let mut draft = GenerationDraft::new(
        ProviderId::fal(),
        "fal-ai/fixture/text-to-video",
        "A dry-validation fixture",
    )
    .expect("fixture draft");
    draft.duration = Some(4);
    draft.aspect_ratio = Some("16:9".into());
    draft.adapter_options = Some(json!({"prompt": "hidden override"}));
    draft
        .media
        .push(DraftMedia::local(media_path, MediaRole::Reference));

    let error = provider
        .validate_draft(&draft)
        .await
        .expect_err("invalid advanced JSON must fail before staging local media");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("cannot override"));
    assert!(upload_executor.uploads().is_empty());
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method == Method::GET),
        "dry validation must not initiate an upload or paid queue POST"
    );
}

#[tokio::test]
async fn fresh_submit_fetches_only_the_selected_model_schema() {
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(
            StatusCode::OK,
            json!({"models": [expanded_string_duration_model()], "next_cursor": null, "has_more": false}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({
                "request_id": "request-targeted",
                "status_url": "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-targeted/status",
                "response_url": "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-targeted/response"
            }),
        ),
    ]);
    let provider = provider(executor.clone());
    provider.submit(&request()).await.expect("targeted submit");

    let requests = executor.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::GET);
    assert!(!requests[0].authorized);
    assert!(
        requests[0]
            .url
            .query_pairs()
            .any(|(key, value)| key == "endpoint_id" && value == "fal-ai/fixture/text-to-video")
    );
    assert_eq!(requests[1].method, Method::POST);
    assert!(requests[1].authorized);
    assert_eq!(
        requests[1].body.as_ref().expect("submit body")["duration"],
        "4"
    );
}

#[tokio::test]
async fn malformed_accepted_job_is_uncertain_and_not_retried() {
    let mut replies = Vec::from(catalog_replies());
    replies.extend([
        Reply::Json(StatusCode::OK, json!({"queue_position": 0})),
        Reply::Json(StatusCode::OK, json!({"request_id": "must-not-be-read"})),
    ]);
    let executor = ScriptedExecutor::with_replies(replies);
    let provider = provider(executor.clone());
    provider.load_catalog().await.expect("cache fixture model");
    let error = provider
        .submit(&request())
        .await
        .expect_err("missing accepted id must be uncertain");
    assert_eq!(error.kind, ProviderErrorKind::SubmissionUncertain);
    let requests = executor.requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
}

#[tokio::test]
async fn completed_error_is_failed_without_fetching_result() {
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(
            StatusCode::OK,
            json!({
                "status": "COMPLETED",
                "request_id": "request-failed",
                "error_type": "content_policy",
                "error": "Fixture moderation failure"
            }),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({"data": {"video": {"url": "https://cdn.fal.media/must-not-read.mp4"}}}),
        ),
    ]);
    let provider = provider(executor.clone());
    let locator =
        FalProvider::locator("fal-ai/fixture/text-to-video", "request-failed").expect("locator");
    let job = provider.poll(&locator).await.expect("failed status");
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.error.as_deref(), Some("Fixture moderation failure"));
    assert_eq!(executor.requests().len(), 1);
}

#[tokio::test]
async fn import_urls_and_result_fallback_use_only_expected_queue_paths() {
    let bare = FalProvider::parse_import_url(
        "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-import",
    )
    .expect("bare import URL");
    let JobLocator::Fal {
        response_url,
        status_url,
        ..
    } = &bare
    else {
        unreachable!()
    };
    assert!(response_url.is_none());
    assert!(status_url.is_none());
    let explicit = FalProvider::parse_import_url(
        "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-import/response",
    )
    .expect("response import URL");
    assert!(matches!(
        explicit,
        JobLocator::Fal {
            response_url: Some(_),
            ..
        }
    ));
    assert!(
        FalProvider::parse_import_url(
            "https://queue.fal.run/fal-ai/fixture/text-to-video/requests/request-import/cancel"
        )
        .is_err()
    );

    let executor = ScriptedExecutor::with_replies([
        Reply::Json(StatusCode::OK, json!({"status": "COMPLETED"})),
        Reply::Json(
            StatusCode::NOT_FOUND,
            json!({"error": {"message": "response suffix unavailable"}}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({
                "data": {"video": {
                    "url": "https://cdn.fal.media/import.mp4",
                    "content_type": "video/mp4"
                }}
            }),
        ),
    ]);
    let provider = provider(executor.clone());
    let job = provider
        .poll(&bare)
        .await
        .expect("documented base fallback");
    assert_eq!(job.status, JobStatus::Completed);
    let requests = executor.requests();
    assert!(requests[1].url.path().ends_with("/response"));
    assert!(requests[2].url.path().ends_with("/request-import"));
}

#[tokio::test]
async fn anonymous_download_does_not_overwrite_a_racing_destination() {
    let executor = ScriptedExecutor::with_replies([Reply::Bytes(b"new video".to_vec())]);
    let provider = provider(executor.clone());
    let directory = tempdir().expect("temporary output");
    let destination = directory.path().join("race.mp4");
    executor.create_destination_during_download(destination.clone(), b"existing video");
    let artifact = video_harness::domain::VideoArtifact::new("https://cdn.fal.media/race.mp4", 0)
        .expect("artifact");

    let error = provider
        .download(&artifact, &destination, None)
        .await
        .expect_err("racing destination must win without overwrite");
    assert_eq!(error.kind, ProviderErrorKind::Download);
    assert_eq!(
        fs::read(&destination).expect("preserved video"),
        b"existing video"
    );
    assert!(!directory.path().join("race.mp4.part").exists());
    assert!(!executor.requests()[0].authorized);
}

#[tokio::test]
async fn anonymous_download_rejects_non_public_redirects_before_a_second_get() {
    for location in [
        "https://127.0.0.1/private.mp4",
        "https://100.64.0.1/private.mp4",
        "https://240.0.0.1/private.mp4",
        "https://[4000::1]/private.mp4",
    ] {
        let executor = ScriptedExecutor::with_replies([Reply::Redirect(location.into())]);
        let provider = provider(executor.clone());
        let directory = tempdir().expect("temporary output");
        let destination = directory.path().join("redirect.mp4");
        let artifact = video_harness::domain::VideoArtifact::new(
            "https://cdn.fal.media/public-artifact.mp4",
            0,
        )
        .expect("public artifact");

        let error = provider
            .download(&artifact, &destination, None)
            .await
            .expect_err("non-public redirect must fail closed");

        assert_eq!(error.kind, ProviderErrorKind::UnsafeEndpoint);
        let requests = executor.requests();
        assert_eq!(requests.len(), 1, "followed unsafe redirect {location}");
        assert_eq!(requests[0].url.host_str(), Some("cdn.fal.media"));
        assert!(!destination.exists());
        assert!(!directory.path().join("redirect.mp4.part").exists());
    }
}

#[tokio::test]
async fn failed_or_empty_download_cleans_up_the_partial_file() {
    let artifact =
        video_harness::domain::VideoArtifact::new("https://cdn.fal.media/incomplete.mp4", 0)
            .expect("artifact");
    for reply in [Reply::InterruptedBody, Reply::Bytes(Vec::new())] {
        let executor = ScriptedExecutor::with_replies([reply]);
        let provider = provider(executor);
        let directory = tempdir().expect("temporary output");
        let destination = directory.path().join("incomplete.mp4");
        let error = provider
            .download(&artifact, &destination, None)
            .await
            .expect_err("incomplete download must fail");
        assert_eq!(error.kind, ProviderErrorKind::Download);
        assert!(!destination.exists());
        assert!(!directory.path().join("incomplete.mp4.part").exists());
    }
}

#[tokio::test]
async fn local_media_uses_documented_cdn_initiation_and_reuses_valid_receipt() {
    let directory = tempdir().expect("temporary media");
    let media_path = directory.path().join("first-frame.png");
    fs::write(&media_path, PNG_FIXTURE).expect("fixture media");
    let executor = ScriptedExecutor::with_replies([Reply::Json(
        StatusCode::OK,
        json!({
            "file_url": "https://v3.fal.media/files/fixture/first-frame.png",
            "upload_url": "https://uploads.example/signed/object?signature=placeholder"
        }),
    )]);
    let upload_executor = Arc::new(ScriptedUploadExecutor::default());
    let provider = provider_with_upload(executor.clone(), upload_executor.clone());
    let stager = <FalProvider as MediaStager>::descriptor(&provider);
    assert_eq!(stager.id, "fal-cdn-v3");
    assert_eq!(stager.display_name, "fal.ai CDN");
    assert_eq!(stager.credential_provider, Some(ProviderId::fal()));
    assert_eq!(stager.visibility, StagedVisibility::PublicByLink);
    assert_eq!(stager.retention, Some(Duration::from_secs(24 * 60 * 60)));
    assert!(provider.media_capabilities().local_files);
    assert!(provider.media_capabilities().uploaded_files_public);
    assert_eq!(
        provider.media_capabilities().upload_retention,
        Some(Duration::from_secs(24 * 60 * 60))
    );

    let media = DraftMedia::local(&media_path, MediaRole::StartFrame);
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let staged =
        <FalProvider as MediaStager>::stage_local(&provider, &media, None, Some(progress_tx))
            .await
            .expect("stage local fal media");
    let receipt = staged.receipt.clone().expect("upload receipt");
    assert_eq!(receipt.provider_id, ProviderId::fal());
    assert_eq!(receipt.size_bytes, PNG_FIXTURE.len() as u64);
    assert_eq!(receipt.content_type.as_deref(), Some("image/png"));
    assert_eq!(receipt.sha256.len(), 64);
    assert_eq!(
        receipt.expires_at - receipt.uploaded_at,
        chrono::Duration::hours(24)
    );
    assert_eq!(
        progress_rx.recv().await.expect("upload progress"),
        UploadProgress {
            sent: receipt.size_bytes,
            total: receipt.size_bytes,
        }
    );

    let requests = executor.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::POST);
    assert!(requests[0].authorized);
    assert_eq!(requests[0].url.host_str(), Some("rest.fal.ai"));
    assert_eq!(requests[0].url.path(), "/storage/upload/initiate");
    assert!(
        requests[0]
            .url
            .query_pairs()
            .any(|(key, value)| key == "storage_type" && value == "fal-cdn-v3")
    );
    assert_eq!(
        requests[0].object_lifecycle.as_deref(),
        Some("{\"expiration_duration_seconds\":86400}")
    );
    assert_eq!(
        requests[0].body.as_ref().expect("initiation body")["file_name"],
        "first-frame.png"
    );
    let uploads = upload_executor.uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].url.host_str(), Some("uploads.example"));
    assert_eq!(uploads[0].path, media_path);
    assert_eq!(uploads[0].content_type, "image/png");
    assert_eq!(uploads[0].size_bytes, receipt.size_bytes);
    assert!(!uploads[0].multipart);

    let reused = provider
        .stage_media(&media, Some(&receipt), None)
        .await
        .expect("reuse unexpired matching receipt");
    assert_eq!(reused.receipt.as_ref(), Some(&receipt));
    assert_eq!(
        executor.requests().len(),
        1,
        "cache reuse must not initiate"
    );
    assert_eq!(
        upload_executor.uploads().len(),
        1,
        "cache reuse must not upload"
    );
}

#[tokio::test]
async fn changed_local_media_is_rejected_after_upload_without_a_receipt() {
    let directory = tempdir().expect("temporary media");
    let media_path = directory.path().join("changing.png");
    fs::write(&media_path, PNG_FIXTURE).expect("fixture media");
    let executor = ScriptedExecutor::with_replies([Reply::Json(
        StatusCode::OK,
        json!({
            "file_url": "https://v3.fal.media/files/fixture/changing.png",
            "upload_url": "https://uploads.example/signed/changing?signature=placeholder"
        }),
    )]);
    let upload_executor = Arc::new(ScriptedUploadExecutor::default());
    upload_executor.replace_file_during_upload(b"different bytes and size");
    let provider = provider_with_upload(executor, upload_executor.clone());

    let error = provider
        .stage_media(
            &DraftMedia::local(&media_path, MediaRole::Reference),
            None,
            None,
        )
        .await
        .expect_err("changed input must invalidate the upload");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("changed while fal was preparing"));
    assert_eq!(upload_executor.uploads().len(), 1);
}

#[test]
fn seedance_local_byte_limits_are_checked_before_upload() {
    let directory = tempdir().expect("temporary media");
    let audio_path = directory.path().join("oversized.mp3");
    fs::write(&audio_path, b"ID3fixture").expect("audio fixture");
    fs::OpenOptions::new()
        .write(true)
        .open(&audio_path)
        .expect("open audio fixture")
        .set_len(15_000_001)
        .expect("size sparse audio");
    let mut draft = GenerationDraft::new(
        ProviderId::fal(),
        "bytedance/seedance-2.0/reference-to-video",
        "A local size fixture",
    )
    .expect("draft");
    draft
        .media
        .push(DraftMedia::local(audio_path, MediaRole::AudioInput));
    let provider = provider(ScriptedExecutor::with_replies([]));
    let error = provider
        .validate_draft_media_constraints(&draft)
        .expect_err("oversized Seedance audio must fail before upload");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("15 MB"));

    let image_path = directory.path().join("oversized.png");
    fs::write(&image_path, b"\x89PNG\r\n\x1a\nfixture").expect("image fixture");
    fs::OpenOptions::new()
        .write(true)
        .open(&image_path)
        .expect("open image fixture")
        .set_len(30_000_001)
        .expect("extend image fixture");
    let mut image_draft = GenerationDraft::new(
        ProviderId::fal(),
        "bytedance/seedance-2.0/reference-to-video",
        "Animate it",
    )
    .expect("image draft");
    image_draft
        .media
        .push(DraftMedia::local(image_path, MediaRole::Reference));
    let error = provider
        .validate_draft_media_constraints(&image_draft)
        .expect_err("oversized Seedance image must fail before upload");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("30 MB"));

    let gif_path = directory.path().join("reference.gif");
    fs::write(&gif_path, b"GIF89a").expect("GIF fixture");
    let mut gif_draft = GenerationDraft::new(
        ProviderId::fal(),
        "bytedance/seedance-2.0/reference-to-video",
        "Animate it",
    )
    .expect("GIF draft");
    gif_draft
        .media
        .push(DraftMedia::local(gif_path, MediaRole::Reference));
    let error = provider
        .validate_draft_media_constraints(&gif_draft)
        .expect_err("unsupported Seedance image format must fail before upload");
    assert_eq!(error.kind, ProviderErrorKind::Validation);
    assert!(error.message.contains("JPEG, PNG, or WebP"));
}

#[tokio::test]
async fn fal_rejects_non_public_signed_upload_urls_before_sending_bytes() {
    let directory = tempdir().expect("temporary media");
    let media_path = directory.path().join("reference.jpg");
    fs::write(&media_path, b"\xff\xd8\xff\xe0fixture").expect("fixture media");
    let executor = ScriptedExecutor::with_replies([Reply::Json(
        StatusCode::OK,
        json!({
            "file_url": "https://v3.fal.media/files/fixture/reference.jpg",
            "upload_url": "https://127.0.0.1/private-upload"
        }),
    )]);
    let upload_executor = Arc::new(ScriptedUploadExecutor::default());
    let provider = provider_with_upload(executor, upload_executor.clone());
    let error = provider
        .stage_media(
            &DraftMedia::local(media_path, MediaRole::Reference),
            None,
            None,
        )
        .await
        .expect_err("local signed target must be rejected");
    assert_eq!(error.kind, ProviderErrorKind::UnsafeEndpoint);
    assert!(upload_executor.uploads().is_empty());
}

#[tokio::test]
async fn large_local_media_selects_fal_multipart_protocol() {
    let directory = tempdir().expect("temporary media");
    let media_path = directory.path().join("large-reference.png");
    fs::write(&media_path, PNG_FIXTURE).expect("fixture image header");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&media_path)
        .expect("create sparse media");
    file.set_len(90 * 1024 * 1024 + 1)
        .expect("size sparse media");
    drop(file);
    let executor = ScriptedExecutor::with_replies([Reply::Json(
        StatusCode::OK,
        json!({
            "file_url": "https://v3.fal.media/files/fixture/large-reference.png",
            "upload_url": "https://uploads.example/multipart/session?signature=placeholder"
        }),
    )]);
    let upload_executor = Arc::new(ScriptedUploadExecutor::default());
    let provider = provider_with_upload(executor.clone(), upload_executor.clone());
    provider
        .stage_media(
            &DraftMedia::local(media_path, MediaRole::Reference),
            None,
            None,
        )
        .await
        .expect("stage sparse multipart fixture");
    assert_eq!(
        executor.requests()[0].url.path(),
        "/storage/upload/initiate-multipart"
    );
    assert!(upload_executor.uploads()[0].multipart);
}
