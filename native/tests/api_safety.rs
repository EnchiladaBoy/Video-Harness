use std::collections::VecDeque;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION,
};
use reqwest::{Method, StatusCode};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::sync::Notify;
use url::Url;
use video_harness::api::{
    ApiErrorKind, ClientOptions, HttpExecutor, HttpRequest, HttpResponse, MAX_VIDEO_DOWNLOAD_BYTES,
    OpenRouterClient, TransportError,
};
use video_harness::config::partial_path;
use video_harness::domain::{MediaKind, VideoRequest};

const BASE_URL: &str = "https://api.fixture.invalid/api/v1";
const FIXTURE_KEY: &str = "sk-test-placeholder";

#[derive(Clone)]
struct CapturedRequest {
    method: Method,
    url: Url,
    headers: HeaderMap,
    json_body: Option<Value>,
}

enum Reply {
    Json(StatusCode, Value),
    Bytes(StatusCode, HeaderMap, Vec<u8>),
    WaitBytes {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
    TransportError,
}

#[derive(Default)]
struct ScriptedExecutor {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<CapturedRequest>>,
}

impl ScriptedExecutor {
    fn with_replies(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("request lock").clone()
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
                headers: request.headers.clone(),
                json_body: request.json_body.clone(),
            });
        let reply = self
            .replies
            .lock()
            .expect("reply lock")
            .pop_front()
            .expect("scripted response");
        match reply {
            Reply::Json(status, value) => {
                HttpResponse::from_json(status, request.url, &value).map_err(|_| TransportError)
            }
            Reply::Bytes(status, headers, body) => Ok(HttpResponse::from_bytes(
                status,
                request.url,
                headers,
                Bytes::from(body),
            )),
            Reply::WaitBytes {
                status,
                headers,
                body,
                started,
                release,
            } => {
                started.notify_one();
                release.notified().await;
                Ok(HttpResponse::from_bytes(
                    status,
                    request.url,
                    headers,
                    Bytes::from(body),
                ))
            }
            Reply::TransportError => Err(TransportError),
        }
    }
}

fn client(executor: Arc<ScriptedExecutor>, max_retries: usize) -> OpenRouterClient {
    OpenRouterClient::with_executor(
        SecretString::from(FIXTURE_KEY.to_owned()),
        ClientOptions {
            base_url: Url::parse(BASE_URL).expect("fixture base URL"),
            max_retries,
            backoff_base: Duration::ZERO,
            ..ClientOptions::default()
        },
        executor,
    )
    .expect("fixture client")
}

fn fixture_json(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    serde_json::from_slice(&fs::read(path).expect("read fixture")).expect("parse fixture")
}

fn has_authorization(request: &CapturedRequest) -> bool {
    request.headers.contains_key(AUTHORIZATION)
}

#[tokio::test]
async fn typed_reference_models_are_enriched_from_the_public_general_catalog() {
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(
            StatusCode::OK,
            json!({"data": [{
                "id": "bytedance/seedance-2.0",
                "name": "Seedance 2.0"
            }]}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({"data": [{
                "id": "bytedance/seedance-2.0",
                "architecture": {
                    "input_modalities": ["text", "image", "video", "audio"],
                    "output_modalities": ["video"]
                }
            }]}),
        ),
    ]);
    let catalog = client(executor.clone(), 0)
        .list_video_models()
        .await
        .expect("enriched video catalog");
    assert_eq!(
        catalog.models[0].input_modalities,
        Some(vec![MediaKind::Image, MediaKind::Video, MediaKind::Audio])
    );
    let requests = executor.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url.path(), "/api/v1/videos/models");
    assert_eq!(requests[1].url.path(), "/api/v1/models");
    assert!(
        requests[1]
            .url
            .query_pairs()
            .any(|(key, value)| key == "output_modalities" && value == "video")
    );
    assert!(requests.iter().all(|request| !has_authorization(request)));
}

#[tokio::test]
async fn failed_typed_capability_enrichment_leaves_modalities_unknown() {
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(
            StatusCode::OK,
            json!({"data": [{"id": "bytedance/seedance-2.0"}]}),
        ),
        Reply::Json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"message": "fixture outage"}}),
        ),
    ]);
    let catalog = client(executor.clone(), 0)
        .list_video_models()
        .await
        .expect("base catalog remains usable");
    assert_eq!(catalog.models[0].input_modalities, None);
    assert_eq!(executor.requests().len(), 2);
}

#[tokio::test]
async fn authorization_matrix_limits_credentials_to_authenticated_api_calls() {
    let jobs = fixture_json("jobs.json");
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(
            StatusCode::OK,
            json!({"data": {"label": "fixture", "limit_remaining": "5"}}),
        ),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
        Reply::Json(StatusCode::OK, jobs["submitted"].clone()),
        Reply::Json(StatusCode::OK, jobs["completed"].clone()),
        Reply::Bytes(StatusCode::OK, HeaderMap::new(), b"api video".to_vec()),
        Reply::Bytes(StatusCode::OK, HeaderMap::new(), b"cdn video".to_vec()),
    ]);
    let client = client(executor.clone(), 0);

    client.validate_key().await.expect("validate fixture key");
    client
        .list_video_models()
        .await
        .expect("load fixture catalog");
    let request = VideoRequest::new("black-forest-labs/flux-3-video", "A harmless local fixture")
        .expect("fixture request");
    client.submit(&request).await.expect("scripted submission");
    client
        .poll("/api/v1/videos/job-fixture-1")
        .await
        .expect("scripted poll");

    let directory = tempdir().expect("temporary video directory");
    let content_url = client.content_url("job-fixture-1", 0).expect("content URL");
    client
        .download(&content_url, &directory.path().join("api.mp4"), None)
        .await
        .expect("scripted API download");
    client
        .download(
            &Url::parse("https://cdn.fixture.invalid/video.mp4?signature=fixture")
                .expect("fixture CDN URL"),
            &directory.path().join("cdn.mp4"),
            None,
        )
        .await
        .expect("scripted CDN download");

    let requests = executor.requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0].url.path(), "/api/v1/key");
    assert_eq!(requests[1].url.path(), "/api/v1/videos/models");
    assert_eq!(requests[2].method, Method::POST);
    assert_eq!(requests[2].url.path(), "/api/v1/videos");
    assert!(requests[2].json_body.is_some());
    assert!(requests[2].headers.contains_key(CONTENT_TYPE));
    assert_eq!(requests[3].url.path(), "/api/v1/videos/job-fixture-1");
    assert_eq!(
        requests.iter().map(has_authorization).collect::<Vec<_>>(),
        vec![true, false, true, true, true, false]
    );
}

#[tokio::test]
async fn cross_origin_polling_url_is_rejected_before_transport() {
    let executor = ScriptedExecutor::with_replies([]);
    let client = client(executor.clone(), 0);
    let error = client
        .poll("https://attacker.fixture.invalid/api/v1/videos/job-1")
        .await
        .expect_err("cross-origin polling must fail");
    assert_eq!(error.kind, ApiErrorKind::UnsafeUrl);
    assert!(executor.requests().is_empty());
}

#[tokio::test]
async fn ambiguous_submission_transport_failure_performs_exactly_one_post() {
    let executor = ScriptedExecutor::with_replies([
        Reply::TransportError,
        Reply::Json(
            StatusCode::OK,
            json!({
                "id": "must-not-be-read",
                "status": "pending",
                "polling_url": "/api/v1/videos/must-not-be-read"
            }),
        ),
    ]);
    let client = client(executor.clone(), 5);
    let request = VideoRequest::new("black-forest-labs/flux-3-video", "A harmless local fixture")
        .expect("fixture request");
    let error = client
        .submit(&request)
        .await
        .expect_err("uncertain submission must not retry");
    assert_eq!(error.kind, ApiErrorKind::SubmissionUncertain);
    let requests = executor.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::POST);
}

#[tokio::test]
async fn ambiguous_submission_5xx_performs_exactly_one_post_and_is_never_retryable() {
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"message": "upstream failed after accepting the request"}}),
        ),
        Reply::Json(
            StatusCode::OK,
            json!({
                "id": "must-not-be-read",
                "status": "pending",
                "polling_url": "/api/v1/videos/must-not-be-read"
            }),
        ),
    ]);
    let client = client(executor.clone(), 5);
    let request = VideoRequest::new("black-forest-labs/flux-3-video", "A harmless local fixture")
        .expect("fixture request");
    let error = client
        .submit(&request)
        .await
        .expect_err("a paid 5xx response cannot prove rejection");
    assert_eq!(error.kind, ApiErrorKind::SubmissionUncertain);
    assert_eq!(error.status_code, Some(503));
    assert!(!error.is_retryable());
    assert!(error.message.contains("may exist"));
    let requests = executor.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::POST);
}

#[tokio::test]
async fn nonstandard_4xx_cannot_prove_rejection_and_is_not_retried() {
    let status = StatusCode::from_u16(499).expect("fixture status");
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(
            status,
            json!({"error": {"message": "proxy closed after forwarding the request"}}),
        ),
        Reply::Json(StatusCode::OK, json!({"id": "must-not-be-read"})),
    ]);
    let client = client(executor.clone(), 5);
    let request = VideoRequest::new("black-forest-labs/flux-3-video", "A harmless local fixture")
        .expect("fixture request");
    let error = client
        .submit(&request)
        .await
        .expect_err("an unknown proxy status cannot prove paid-request rejection");
    assert_eq!(error.kind, ApiErrorKind::SubmissionUncertain);
    assert_eq!(error.status_code, Some(499));
    assert!(!error.is_retryable());
    assert_eq!(executor.requests().len(), 1);
}

#[tokio::test]
async fn paid_submission_preserves_deterministic_4xx_rejection() {
    let executor = ScriptedExecutor::with_replies([Reply::Json(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"error": {"message": "fixture validation rejection"}}),
    )]);
    let client = client(executor.clone(), 5);
    let request = VideoRequest::new("black-forest-labs/flux-3-video", "A harmless local fixture")
        .expect("fixture request");
    let error = client
        .submit(&request)
        .await
        .expect_err("422 proves the paid request was rejected");
    assert_eq!(error.kind, ApiErrorKind::RequestValidation);
    assert_eq!(error.status_code, Some(422));
    assert_eq!(executor.requests().len(), 1);
}

#[tokio::test]
async fn malformed_successful_submission_is_uncertain_and_performs_one_post() {
    let executor = ScriptedExecutor::with_replies([
        Reply::Json(StatusCode::OK, json!({"status": "pending"})),
        Reply::Json(
            StatusCode::OK,
            json!({
                "id": "must-not-be-read",
                "status": "pending",
                "polling_url": "/api/v1/videos/must-not-be-read"
            }),
        ),
    ]);
    let client = client(executor.clone(), 5);
    let request = VideoRequest::new("black-forest-labs/flux-3-video", "A harmless local fixture")
        .expect("fixture request");
    let error = client
        .submit(&request)
        .await
        .expect_err("missing accepted id must be uncertain");
    assert_eq!(error.kind, ApiErrorKind::SubmissionUncertain);
    let requests = executor.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::POST);
}

#[tokio::test]
async fn accepted_job_requires_string_identity_bound_to_its_polling_locator() {
    for payload in [
        json!({
            "id": 42,
            "status": "pending",
            "polling_url": "/api/v1/videos/42"
        }),
        json!({
            "id": "job-a",
            "status": "pending",
            "polling_url": 42
        }),
        json!({
            "id": "job-a",
            "status": "pending",
            "polling_url": "/api/v1/videos/job-b"
        }),
    ] {
        let executor = ScriptedExecutor::with_replies([Reply::Json(StatusCode::OK, payload)]);
        let client = client(executor.clone(), 0);
        let request = VideoRequest::new("black-forest-labs/flux-3-video", "Identity fixture")
            .expect("fixture request");

        let error = client
            .submit(&request)
            .await
            .expect_err("malformed accepted identity must be uncertain");

        assert_eq!(error.kind, ApiErrorKind::SubmissionUncertain);
        assert_eq!(executor.requests().len(), 1);
    }
}

#[tokio::test]
async fn poll_response_must_match_both_the_requested_job_and_reported_locator() {
    for payload in [
        json!({
            "id": "job-b",
            "status": "pending",
            "polling_url": "/api/v1/videos/job-b"
        }),
        json!({
            "id": "job-a",
            "status": "pending",
            "polling_url": "/api/v1/videos/job-b"
        }),
    ] {
        let executor = ScriptedExecutor::with_replies([Reply::Json(StatusCode::OK, payload)]);
        let client = client(executor.clone(), 0);

        let error = client
            .poll("job-a")
            .await
            .expect_err("cross-job poll response must be rejected");

        assert_eq!(error.kind, ApiErrorKind::ResponseFormat);
        assert_eq!(executor.requests().len(), 1);
        assert_eq!(executor.requests()[0].url.path(), "/api/v1/videos/job-a");
    }
}

#[tokio::test]
async fn poll_accepts_an_omitted_redundant_locator_and_restores_the_canonical_one() {
    for payload in [
        json!({"id": "job-a", "status": "pending"}),
        json!({"id": "job-a", "status": "pending", "polling_url": null}),
    ] {
        let executor = ScriptedExecutor::with_replies([Reply::Json(StatusCode::OK, payload)]);
        let client = client(executor.clone(), 0);

        let job = client
            .poll("job-a")
            .await
            .expect("the requested URL and response id bind the job identity");

        assert_eq!(job.id, "job-a");
        assert_eq!(
            job.polling_url,
            "https://api.fixture.invalid/api/v1/videos/job-a"
        );
        assert_eq!(executor.requests().len(), 1);
        assert_eq!(executor.requests()[0].url.path(), "/api/v1/videos/job-a");
    }
}

#[tokio::test]
async fn safe_get_retries_but_never_adds_catalog_authorization() {
    let executor = ScriptedExecutor::with_replies([
        Reply::TransportError,
        Reply::Json(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": {"message": "fixture rate limit"}}),
        ),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
    ]);
    let client = client(executor.clone(), 2);
    client
        .list_video_models()
        .await
        .expect("safe catalog retry");
    let requests = executor.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| request.method == Method::GET));
    assert!(requests.iter().all(|request| !has_authorization(request)));
}

#[tokio::test]
async fn cross_origin_download_redirect_strips_authorization() {
    let mut redirect_headers = HeaderMap::new();
    redirect_headers.insert(
        LOCATION,
        HeaderValue::from_static("https://cdn.fixture.invalid/video.mp4?signature=local-fixture"),
    );
    let mut video_headers = HeaderMap::new();
    video_headers.insert(
        reqwest::header::CONTENT_LENGTH,
        HeaderValue::from_static("11"),
    );
    let executor = ScriptedExecutor::with_replies([
        Reply::Bytes(StatusCode::FOUND, redirect_headers, Vec::new()),
        Reply::Bytes(StatusCode::OK, video_headers, b"video bytes".to_vec()),
    ]);
    let client = client(executor.clone(), 0);
    let directory = tempdir().expect("temporary directory");
    let destination = directory.path().join("redirected.mp4");
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    client
        .download(
            &client.content_url("job-fixture-1", 0).expect("content URL"),
            &destination,
            Some(progress_tx),
        )
        .await
        .expect("redirected fixture download");

    assert_eq!(fs::read(&destination).expect("saved video"), b"video bytes");
    assert!(!destination.with_file_name("redirected.mp4.part").exists());
    let progress = progress_rx.try_recv().expect("download progress");
    assert_eq!(progress.written, 11);
    assert_eq!(progress.total, Some(11));
    let requests = executor.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url.host_str(), Some("api.fixture.invalid"));
    assert!(has_authorization(&requests[0]));
    assert_eq!(requests[1].url.host_str(), Some("cdn.fixture.invalid"));
    assert!(!has_authorization(&requests[1]));
}

#[tokio::test]
async fn download_redirect_to_a_non_public_host_stops_before_the_second_get() {
    let mut redirect_headers = HeaderMap::new();
    redirect_headers.insert(
        LOCATION,
        HeaderValue::from_static("https://127.0.0.1/private-video.mp4"),
    );
    let executor = ScriptedExecutor::with_replies([Reply::Bytes(
        StatusCode::FOUND,
        redirect_headers,
        Vec::new(),
    )]);
    let client = client(executor.clone(), 0);
    let directory = tempdir().expect("temporary directory");
    let destination = directory.path().join("blocked-redirect.mp4");

    let error = client
        .download(
            &client.content_url("job-fixture-1", 0).expect("content URL"),
            &destination,
            None,
        )
        .await
        .expect_err("private redirect must fail");

    assert_eq!(error.kind, ApiErrorKind::UnsafeUrl);
    assert_eq!(executor.requests().len(), 1, "no private-host GET was sent");
    assert!(!destination.exists());
    assert!(
        !destination
            .with_file_name("blocked-redirect.mp4.part")
            .exists()
    );
}

#[tokio::test]
async fn unsafe_or_existing_download_targets_are_rejected_before_transport() {
    let executor = ScriptedExecutor::with_replies([]);
    let client = client(executor.clone(), 0);
    let directory = tempdir().expect("temporary directory");
    for (index, value) in [
        "http://cdn.fixture.invalid/video.mp4",
        "file:///tmp/video.mp4",
        "https://name:password@cdn.fixture.invalid/video.mp4",
        "https://100.64.0.1/video.mp4",
        "https://renderbox.local/video.mp4",
    ]
    .into_iter()
    .enumerate()
    {
        let destination = directory.path().join(format!("unsafe-{index}.mp4"));
        let error = client
            .download(
                &Url::parse(value).expect("parse unsafe fixture URL"),
                &destination,
                None,
            )
            .await
            .expect_err("unsafe URL must fail");
        assert_eq!(error.kind, ApiErrorKind::UnsafeUrl);
        assert!(!destination.exists());
    }

    let destination = directory.path().join("existing.mp4");
    fs::write(&destination, b"keep me").expect("create existing destination");
    let error = client
        .download(
            &Url::parse("https://cdn.fixture.invalid/video.mp4").expect("fixture URL"),
            &destination,
            None,
        )
        .await
        .expect_err("existing destination must fail");
    assert_eq!(error.kind, ApiErrorKind::Download);
    assert_eq!(fs::read(&destination).expect("existing bytes"), b"keep me");
    assert!(executor.requests().is_empty());
}

#[tokio::test]
async fn download_rejects_oversized_content_before_creating_a_partial_file() {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&(MAX_VIDEO_DOWNLOAD_BYTES + 1).to_string())
            .expect("oversized content length"),
    );
    let executor = ScriptedExecutor::with_replies([Reply::Bytes(
        StatusCode::OK,
        headers,
        b"unused fixture bytes".to_vec(),
    )]);
    let client = client(executor, 0);
    let directory = tempdir().expect("temporary directory");
    let destination = directory.path().join("oversized.mp4");

    let error = client
        .download(
            &Url::parse("https://cdn.fixture.invalid/oversized.mp4").expect("fixture URL"),
            &destination,
            None,
        )
        .await
        .expect_err("oversized download must fail closed");

    assert_eq!(error.kind, ApiErrorKind::Download);
    assert!(!destination.exists());
    assert!(!partial_path(&destination).exists());
}

#[tokio::test]
async fn download_rejects_non_success_responses_before_creating_a_partial_file() {
    for status in [StatusCode::MULTIPLE_CHOICES, StatusCode::NOT_MODIFIED] {
        let executor = ScriptedExecutor::with_replies([Reply::Bytes(
            status,
            HeaderMap::new(),
            b"not a video".to_vec(),
        )]);
        let client = client(executor, 0);
        let directory = tempdir().expect("temporary directory");
        let destination = directory
            .path()
            .join(format!("status-{}.mp4", status.as_u16()));

        let error = client
            .download(
                &Url::parse("https://cdn.fixture.invalid/not-video").expect("fixture URL"),
                &destination,
                None,
            )
            .await
            .expect_err("a non-success response must not be saved as video");

        assert_eq!(error.status_code, Some(status.as_u16()));
        assert!(!destination.exists());
        assert!(!partial_path(&destination).exists());
    }
}

#[tokio::test]
async fn failed_download_never_removes_a_partial_file_owned_by_a_peer() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let executor = ScriptedExecutor::with_replies([Reply::WaitBytes {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        body: b"new video".to_vec(),
        started: started.clone(),
        release: release.clone(),
    }]);
    let client = client(executor, 0);
    let directory = tempdir().expect("temporary directory");
    let destination = directory.path().join("racing.mp4");
    let partial = partial_path(&destination);
    let download_url = Url::parse("https://cdn.fixture.invalid/racing.mp4").expect("fixture URL");
    let download_destination = destination.clone();
    let download = tokio::spawn(async move {
        client
            .download(&download_url, &download_destination, None)
            .await
    });

    started.notified().await;
    fs::write(&partial, b"peer download").expect("create peer-owned partial");
    release.notify_one();

    let error = download
        .await
        .expect("download task")
        .expect_err("partial collision must fail");
    assert_eq!(error.kind, ApiErrorKind::Download);
    assert_eq!(
        fs::read(&partial).expect("peer partial remains"),
        b"peer download"
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn api_errors_redact_the_key_from_messages_and_metadata() {
    let executor = ScriptedExecutor::with_replies([Reply::Json(
        StatusCode::BAD_REQUEST,
        json!({
            "error": {
                "code": "fixture_error",
                "message": format!("bad value {FIXTURE_KEY}"),
                "metadata": {"debug": FIXTURE_KEY}
            }
        }),
    )]);
    let client = client(executor, 0);
    let error = client
        .poll("job-fixture-1")
        .await
        .expect_err("scripted API error");
    assert!(!error.message.contains(FIXTURE_KEY));
    assert!(!format!("{:?}", error.details).contains(FIXTURE_KEY));
    assert!(error.message.contains("[REDACTED]"));
}
