use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use openrouter_video_studio::api::{HttpExecutor, HttpRequest, HttpResponse, TransportError};
use openrouter_video_studio::domain::{
    JobLocator, JobStatus, ProviderId, QuoteConfidence, VideoRequest,
};
use openrouter_video_studio::providers::fal::{FalOptions, FalProvider};
use openrouter_video_studio::providers::{ProviderErrorKind, VideoProvider};
use reqwest::header::{AUTHORIZATION, HeaderMap};
use reqwest::{Method, StatusCode};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::tempdir;
use url::Url;

const KEY: &str = "fal-test-placeholder";

#[derive(Clone)]
struct CapturedRequest {
    method: Method,
    url: Url,
    authorized: bool,
    body: Option<Value>,
}

enum Reply {
    Json(StatusCode, Value),
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
                body: request.json_body.clone(),
            });
        match self.replies.lock().expect("reply lock").pop_front() {
            Some(Reply::Json(status, value)) => {
                HttpResponse::from_json(status, request.url, &value).map_err(|_| TransportError)
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

fn catalog_replies() -> [Reply; 3] {
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
    assert_eq!(requests.len(), 8);
    assert!(requests[..3].iter().all(|request| !request.authorized));
    assert!(requests[3..7].iter().all(|request| request.authorized));
    assert!(!requests[7].authorized);
    assert_eq!(requests[4].method, Method::POST);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    let body = requests[4].body.as_ref().expect("paid POST body");
    assert_eq!(body["prompt"], "A harmless offline fixture");
    assert_eq!(body["duration"], 4);
    assert_eq!(body["custom_strength"], 0.75);
    assert_eq!(requests[7].url.host_str(), Some("cdn.fal.media"));
    assert_eq!(
        requests[6].url.path(),
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
    assert_eq!(requests.len(), 4);
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
    assert_eq!(requests.len(), 4);
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
    let artifact =
        openrouter_video_studio::domain::VideoArtifact::new("https://cdn.fal.media/race.mp4", 0)
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
async fn failed_or_empty_download_cleans_up_the_partial_file() {
    let artifact = openrouter_video_studio::domain::VideoArtifact::new(
        "https://cdn.fal.media/incomplete.mp4",
        0,
    )
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
