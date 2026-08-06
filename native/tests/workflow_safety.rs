use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, HeaderMap};
use reqwest::{Method, StatusCode};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tokio::sync::Notify;
use url::Url;
use video_harness::AppPaths;
use video_harness::api::{ClientOptions, HttpExecutor, HttpRequest, HttpResponse, TransportError};
use video_harness::domain::{
    DraftMedia, GenerationDraft, JobLocator, JobStatus, MediaRole, ProviderId, ProviderJobKey,
    VideoCatalog, VideoJob, VideoRequest,
};
use video_harness::gui_state::{DraftEditorState, GuiStateStore, StoredDraft};
use video_harness::history::HistoryStore;
use video_harness::workflow::{
    PreparedGenerationId, ServiceCommand, ServiceConfig, ServiceEvent, ServiceHandle, ServiceScope,
    spawn_service_with_executor,
};

const BASE_URL: &str = "https://api.workflow.invalid/api/v1";
const FIXTURE_KEY: &str = "sk-test-workflow-placeholder";

#[derive(Clone)]
struct CapturedRequest {
    method: Method,
    url: Url,
    authorized: bool,
}

enum Reply {
    Json(StatusCode, Value),
    WaitJson {
        status: StatusCode,
        value: Value,
        started: Arc<Notify>,
        release: Arc<Notify>,
    },
    Bytes(Vec<u8>),
}

#[derive(Default)]
struct WorkflowExecutor {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<CapturedRequest>>,
}

impl WorkflowExecutor {
    fn scripted(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
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
impl HttpExecutor for WorkflowExecutor {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.requests
            .lock()
            .expect("request lock")
            .push(CapturedRequest {
                method: request.method,
                url: request.url.clone(),
                authorized: request.headers.contains_key(AUTHORIZATION),
            });
        let reply = { self.replies.lock().expect("reply lock").pop_front() };
        match reply {
            Some(Reply::Json(status, value)) => {
                HttpResponse::from_json(status, request.url, &value).map_err(|_| TransportError)
            }
            Some(Reply::WaitJson {
                status,
                value,
                started,
                release,
            }) => {
                started.notify_one();
                release.notified().await;
                HttpResponse::from_json(status, request.url, &value).map_err(|_| TransportError)
            }
            Some(Reply::Bytes(body)) => Ok(HttpResponse::from_bytes(
                StatusCode::OK,
                request.url,
                HeaderMap::new(),
                Bytes::from(body),
            )),
            None => Err(TransportError),
        }
    }
}

fn fixture_json(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    serde_json::from_slice(&fs::read(path).expect("read fixture")).expect("parse fixture")
}

fn fixture_paths() -> (TempDir, AppPaths) {
    let root = tempdir().expect("temporary workflow directory");
    let paths = AppPaths {
        data_dir: root.path().join("data"),
        cache_dir: root.path().join("cache"),
        config_dir: root.path().join("config"),
        videos_dir: root.path().join("Videos"),
    };
    (root, paths)
}

fn config(poll_interval: Duration) -> ServiceConfig {
    ServiceConfig {
        poll_interval,
        max_poll_attempts: 3,
        client_options: ClientOptions {
            base_url: Url::parse(BASE_URL).expect("fixture base URL"),
            max_retries: 0,
            backoff_base: Duration::ZERO,
            ..ClientOptions::default()
        },
        use_system_credentials: false,
    }
}

async fn next_event(service: &mut ServiceHandle) -> ServiceEvent {
    tokio::time::timeout(Duration::from_secs(5), service.events.recv())
        .await
        .expect("workflow event timeout")
        .expect("workflow event channel closed")
}

async fn event_matching(
    service: &mut ServiceHandle,
    predicate: impl Fn(&ServiceEvent) -> bool,
) -> ServiceEvent {
    loop {
        let event = next_event(service).await;
        if predicate(&event) {
            return event;
        }
        if matches!(event, ServiceEvent::Error { .. }) {
            panic!("unexpected workflow error: {event:?}");
        }
    }
}

async fn connect_fixture_key(service: &mut ServiceHandle, op_id: u64) {
    let ServiceEvent::Ready {
        providers,
        default_provider,
    } = next_event(service).await
    else {
        panic!("expected workflow ready event");
    };
    assert_eq!(default_provider, ProviderId::openrouter());
    let openrouter = providers
        .iter()
        .find(|provider| provider.descriptor.id == ProviderId::openrouter())
        .expect("OpenRouter connection state");
    assert!(!openrouter.connected);
    assert_eq!(openrouter.credential_status.backend, "memory");
    assert!(!openrouter.credential_status.persistent);

    service
        .commands
        .send(ServiceCommand::ConnectApiKey {
            op_id,
            provider_id: ProviderId::openrouter(),
            key: SecretString::from(FIXTURE_KEY.to_owned()),
            // The memory-only seam must remain memory-only even when persistence is requested.
            persist_on_success: true,
        })
        .await
        .expect("send connect command");
    let event = event_matching(service, |event| {
        matches!(event, ServiceEvent::ApiKeyConnected { op_id: value, .. } if *value == op_id)
    })
    .await;
    let ServiceEvent::ApiKeyConnected {
        credential_status, ..
    } = event
    else {
        unreachable!()
    };
    assert_eq!(credential_status.backend, "memory");
    assert!(!credential_status.persistent);
}

async fn shutdown(service: &mut ServiceHandle) {
    loop {
        service
            .commands
            .send(ServiceCommand::Shutdown)
            .await
            .expect("send shutdown command");
        loop {
            match next_event(service).await {
                ServiceEvent::ShutdownComplete => return,
                ServiceEvent::ShutdownBlocked { .. } => break,
                ServiceEvent::Error { message, .. } => {
                    panic!("unexpected workflow error while shutting down: {message}")
                }
                _ => {}
            }
        }
        tokio::task::yield_now().await;
    }
}

fn pending_job(id: &str) -> Value {
    json!({
        "id": id,
        "status": "pending",
        "polling_url": format!("/api/v1/videos/{id}"),
        "generation_id": format!("generation-{id}")
    })
}

fn completed_job(id: &str, download: &str) -> Value {
    json!({
        "id": id,
        "status": "completed",
        "polling_url": format!("/api/v1/videos/{id}"),
        "generation_id": format!("generation-{id}"),
        "unsigned_urls": [download],
        "usage": {"cost": "0.85"}
    })
}

fn fal_expanded_model() -> Value {
    json!({
        "endpoint_id": "fal-ai/fixture/text-to-video",
        "metadata": {
            "display_name": "Fixture Video",
            "description": "Offline fixture",
            "category": "text-to-video",
            "status": "active"
        },
        "openapi": {
            "openapi": "3.0.0",
            "paths": {"/": {"post": {
                "requestBody": {"content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Input"}
                }}},
                "responses": {"200": {"content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/Output"}
                }}}}
            }}},
            "components": {"schemas": {
                "Input": {
                    "type": "object",
                    "required": ["prompt"],
                    "additionalProperties": false,
                    "properties": {
                        "prompt": {"type": "string"},
                        "duration": {"type": "integer", "enum": [4, 8]},
                        "aspect_ratio": {"type": "string", "enum": ["16:9", "9:16"]}
                    }
                },
                "Output": {
                    "type": "object",
                    "required": ["video"],
                    "properties": {"video": {"$ref": "#/components/schemas/File"}}
                },
                "File": {
                    "type": "object",
                    "required": ["url"],
                    "properties": {"url": {"type": "string"}}
                }
            }}
        }
    })
}

#[tokio::test]
async fn startup_catalog_settings_and_history_are_offline_and_memory_only() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
        Reply::Json(
            StatusCode::OK,
            json!({"data": {"label": "workflow fixture"}}),
        ),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
    ]);
    let mut service =
        spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor.clone())
            .expect("spawn service");

    // Public catalogs load before a provider key is connected.
    let ready = next_event(&mut service).await;
    assert!(matches!(ready, ServiceEvent::Ready { providers, .. }
        if providers.iter().all(|provider| !provider.connected)));
    service
        .commands
        .send(ServiceCommand::RefreshCatalog {
            op_id: 1,
            provider_id: ProviderId::openrouter(),
        })
        .await
        .expect("send pre-key catalog command");
    let event = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::CatalogLoaded { op_id: 1, .. })
    })
    .await;
    assert!(
        matches!(event, ServiceEvent::CatalogLoaded { ref catalog, .. }
        if catalog.models.len() == 2)
    );

    // Ready was consumed above, so connect directly here.
    service
        .commands
        .send(ServiceCommand::ConnectApiKey {
            op_id: 2,
            provider_id: ProviderId::openrouter(),
            key: SecretString::from(FIXTURE_KEY.to_owned()),
            persist_on_success: false,
        })
        .await
        .expect("send connect command");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ApiKeyConnected { op_id: 2, .. })
    })
    .await;
    service
        .commands
        .send(ServiceCommand::RefreshCatalog {
            op_id: 3,
            provider_id: ProviderId::openrouter(),
        })
        .await
        .expect("send catalog command");
    let event = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::CatalogLoaded { op_id: 3, .. })
    })
    .await;
    let ServiceEvent::CatalogLoaded { catalog, .. } = event else {
        unreachable!()
    };
    assert_eq!(catalog.models.len(), 2);
    assert!(catalog.stale);
    let live = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::CatalogLoaded { op_id: 3, catalog, .. } if !catalog.stale)
    })
    .await;
    assert!(matches!(
        live,
        ServiceEvent::CatalogLoaded { ref catalog, .. } if catalog.models.len() == 2
    ));
    assert!(paths.catalog_cache().is_file());

    service
        .commands
        .send(ServiceCommand::SaveModelSettings {
            op_id: 4,
            provider_id: ProviderId::openrouter(),
            model_id: "black-forest-labs/flux-3-video".into(),
            settings_json: json!({"duration": 5, "resolution": "720p"}),
        })
        .await
        .expect("send settings command");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SettingsSaved { op_id: 4, .. })
    })
    .await;
    assert!(paths.model_settings().is_file());

    service
        .commands
        .send(ServiceCommand::LoadHistory {
            op_id: 5,
            limit: 100,
        })
        .await
        .expect("send history command");
    let event = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::HistoryLoaded { op_id: 5, .. })
    })
    .await;
    assert!(matches!(
        event,
        ServiceEvent::HistoryLoaded { records, .. } if records.is_empty()
    ));

    let requests = executor.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].authorized);
    assert!(requests[1].authorized);
    assert!(!requests[2].authorized);
    assert_eq!(requests[2].url.path(), "/api/v1/videos/models");
    shutdown(&mut service).await;
}

#[tokio::test]
async fn cached_catalog_is_immediate_and_live_refresh_does_not_block_actor() {
    let (_root, paths) = fixture_paths();
    fs::create_dir_all(&paths.cache_dir).expect("create cache directory");
    VideoCatalog::from_api(&fixture_json("catalog.json"))
        .expect("catalog fixture")
        .save(paths.catalog_cache())
        .expect("seed catalog cache");
    let live_started = Arc::new(Notify::new());
    let release_live = Arc::new(Notify::new());
    let executor = WorkflowExecutor::scripted([Reply::WaitJson {
        status: StatusCode::OK,
        value: fixture_json("catalog.json"),
        started: Arc::clone(&live_started),
        release: Arc::clone(&release_live),
    }]);
    let mut service = spawn_service_with_executor(paths, config(Duration::ZERO), executor.clone())
        .expect("spawn service");
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::Ready { .. }
    ));

    service
        .commands
        .send(ServiceCommand::RefreshCatalog {
            op_id: 6,
            provider_id: ProviderId::openrouter(),
        })
        .await
        .expect("refresh cached catalog");
    let cached = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::CatalogLoaded { op_id: 6, .. })
    })
    .await;
    assert!(matches!(
        cached,
        ServiceEvent::CatalogLoaded { catalog, .. }
            if catalog.stale && catalog.models.len() == 2
    ));
    tokio::time::timeout(Duration::from_secs(5), live_started.notified())
        .await
        .expect("live catalog request did not start");

    // The catalog request is deliberately blocked, but unrelated service
    // commands must continue to run.
    service
        .commands
        .send(ServiceCommand::LoadHistory {
            op_id: 7,
            limit: 10,
        })
        .await
        .expect("load history while catalog is blocked");
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::HistoryLoaded { op_id: 7, .. }
        ))
        .await,
        ServiceEvent::HistoryLoaded { .. }
    ));

    release_live.notify_one();
    assert!(matches!(
        event_matching(&mut service, |event| {
            matches!(event, ServiceEvent::CatalogLoaded { op_id: 6, catalog, .. } if !catalog.stale)
        })
        .await,
        ServiceEvent::CatalogLoaded { .. }
    ));
    assert_eq!(executor.requests().len(), 1);
    shutdown(&mut service).await;
}

#[tokio::test]
async fn invalid_fal_draft_fails_before_upload_review_or_paid_post() {
    let (root, paths) = fixture_paths();
    let media_path = root.path().join("reference.png");
    fs::write(&media_path, b"\x89PNG\r\n\x1a\nworkflow fixture").expect("write local reference");
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"models": []})),
        Reply::Json(
            StatusCode::OK,
            json!({"models": [fal_expanded_model()], "next_cursor": null, "has_more": false}),
        ),
    ]);
    let mut service = spawn_service_with_executor(paths, config(Duration::ZERO), executor.clone())
        .expect("spawn service");
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::Ready { .. }
    ));
    service
        .commands
        .send(ServiceCommand::ConnectApiKey {
            op_id: 8,
            provider_id: ProviderId::fal(),
            key: SecretString::from("fal-test-placeholder".to_owned()),
            persist_on_success: false,
        })
        .await
        .expect("connect fal fixture key");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ApiKeyConnected { op_id: 8, provider_id, .. } if *provider_id == ProviderId::fal())
    })
    .await;

    let mut draft = GenerationDraft::new(
        ProviderId::fal(),
        "fal-ai/fixture/text-to-video",
        "Dry validation workflow fixture",
    )
    .expect("draft");
    draft.duration = Some(4);
    draft.aspect_ratio = Some("16:9".into());
    draft.adapter_options = Some(json!({"prompt": "hidden override"}));
    draft
        .media
        .push(DraftMedia::local(media_path, MediaRole::Reference));
    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 9,
            draft,
            revision: 1,
        })
        .await
        .expect("prepare invalid fal draft");
    let mut preparation_started = false;
    loop {
        match next_event(&mut service).await {
            ServiceEvent::PreparationStarted { op_id: 9, .. } => preparation_started = true,
            ServiceEvent::MediaUploadStarted { op_id: 9, .. } => {
                panic!("invalid draft reached local upload")
            }
            ServiceEvent::ReviewReady { op_id: 9, .. } => {
                panic!("invalid draft reached Review")
            }
            ServiceEvent::Error {
                op_id: 9,
                scope: ServiceScope::Preparation,
                message,
                ..
            } => {
                assert!(message.contains("cannot override"));
                break;
            }
            _ => {}
        }
    }
    assert!(preparation_started);
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method == Method::GET)
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn generate_surfaces_id_then_persists_polls_and_downloads_with_one_post() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, pending_job("job-generated")),
        Reply::Json(
            StatusCode::OK,
            completed_job(
                "job-generated",
                "https://cdn.workflow.invalid/generated.mp4",
            ),
        ),
        Reply::Bytes(b"generated video bytes".to_vec()),
    ]);
    let mut service = spawn_service_with_executor(
        paths.clone(),
        config(Duration::from_millis(1)),
        executor.clone(),
    )
    .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;

    let mut request = VideoRequest::new(
        "black-forest-labs/flux-3-video",
        "A generated workflow fixture",
    )
    .expect("request");
    request.duration = Some(5);
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 10,
            provider_id: ProviderId::openrouter(),
            request,
        })
        .await
        .expect("send generate command");

    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionStarted { op_id: 10, .. })
    })
    .await;
    let accepted = next_event(&mut service).await;
    assert!(matches!(
        accepted,
        ServiceEvent::JobAccepted {
            op_id: 10,
            ref job,
            record: None,
            ..
        } if job.id == "job-generated"
    ));
    let recovery = next_event(&mut service).await;
    assert!(matches!(
        recovery,
        ServiceEvent::JobRecoverySaved {
            op_id: 10,
            ref key,
            store: video_harness::workflow::RecoveryStore::GuiState,
            ..
        } if key.remote_job_id == "job-generated"
    ));
    let resumable = GuiStateStore::new(paths.gui_state_db())
        .resumable_jobs()
        .expect("durable GUI recovery state");
    assert!(
        resumable
            .iter()
            .any(|job| job.key.remote_job_id == "job-generated")
    );
    let persisted = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::JobUpdated { op_id: 10, .. })
    })
    .await;
    assert!(matches!(
        persisted,
        ServiceEvent::JobUpdated {
            op_id: 10,
            ref job,
            ref record,
            ..
        } if job.status == JobStatus::Pending && record.status == "pending"
    ));
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::PollWaiting { op_id: 10, .. }
    ));
    let polled = next_event(&mut service).await;
    assert!(matches!(
        polled,
        ServiceEvent::JobUpdated { ref job, .. } if job.status == JobStatus::Completed
    ));
    let downloaded = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Downloaded { op_id: 10, .. })
    })
    .await;
    let ServiceEvent::Downloaded { path, record, .. } = downloaded else {
        unreachable!()
    };
    assert_eq!(path.parent(), Some(paths.videos_dir.as_path()));
    assert_eq!(
        fs::read(&path).expect("downloaded video"),
        b"generated video bytes"
    );
    assert_eq!(record.output_path.as_deref(), Some(path.as_path()));
    assert_eq!(
        HistoryStore::new(paths.history_db())
            .get("job-generated")
            .expect("read generated history")
            .expect("generated history row")
            .output_path
            .as_deref(),
        Some(path.as_path())
    );

    let requests = executor.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    assert_eq!(requests.len(), 4);
    shutdown(&mut service).await;
}

#[tokio::test]
async fn import_monitors_and_downloads_an_existing_job_without_posting() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(
            StatusCode::OK,
            completed_job("job-imported", "https://cdn.workflow.invalid/imported.mp4"),
        ),
        Reply::Bytes(b"imported video bytes".to_vec()),
    ]);
    let mut service =
        spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor.clone())
            .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    service
        .commands
        .send(ServiceCommand::Import {
            op_id: 20,
            provider_id: ProviderId::openrouter(),
            locator: JobLocator::OpenRouter {
                polling_url: "job-imported".into(),
            },
        })
        .await
        .expect("send import command");
    let imported = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Imported { op_id: 20, .. })
    })
    .await;
    assert!(matches!(
        imported,
        ServiceEvent::Imported { ref record, .. } if record.request.is_none()
    ));
    let downloaded = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Downloaded { op_id: 20, .. })
    })
    .await;
    assert!(matches!(
        downloaded,
        ServiceEvent::Downloaded { ref path, .. }
            if fs::read(path).expect("imported video") == b"imported video bytes"
    ));
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method == Method::GET)
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn duplicate_resume_is_rejected_and_monitor_can_pause_without_posting() {
    let (_root, paths) = fixture_paths();
    let request = VideoRequest::new("example/video", "A resumable fixture").expect("request");
    let pending = VideoJob::from_api(&pending_job("job-resume")).expect("pending job");
    HistoryStore::new(paths.history_db())
        .create_job(&request, &pending)
        .expect("seed pending history");
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, pending_job("job-resume")),
    ]);
    let mut service =
        spawn_service_with_executor(paths, config(Duration::from_secs(30)), executor.clone())
            .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    service
        .commands
        .send(ServiceCommand::Resume {
            op_id: 30,
            key: ProviderJobKey::new(ProviderId::openrouter(), "job-resume")
                .expect("provider job key"),
        })
        .await
        .expect("send resume command");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::PollWaiting { op_id: 30, .. })
    })
    .await;

    service
        .commands
        .send(ServiceCommand::Resume {
            op_id: 31,
            key: ProviderJobKey::new(ProviderId::openrouter(), "job-resume")
                .expect("provider job key"),
        })
        .await
        .expect("send duplicate resume");
    let active_error = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 31, .. })
    })
    .await;
    assert!(matches!(
        active_error,
        ServiceEvent::Error {
            scope: ServiceScope::Generation,
            ref message,
            ..
        } if message.contains("already being monitored")
    ));
    service
        .commands
        .send(ServiceCommand::CancelCurrent { op_id: 32 })
        .await
        .expect("send cancel command");
    let cancelled = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Cancelled { op_id: 30, .. })
    })
    .await;
    assert!(matches!(
        cancelled,
        ServiceEvent::Cancelled {
            job_id: Some(ref value),
            remote_continues: true,
            ..
        } if value == "job-resume"
    ));
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method == Method::GET)
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn accepted_job_id_survives_local_history_failure() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, pending_job("job-history-failure")),
    ]);
    let mut service =
        spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor.clone())
            .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;

    // Replace the initialized SQLite file with a directory so persistence fails deterministically.
    fs::remove_file(paths.history_db()).expect("remove temporary history database");
    fs::create_dir(paths.history_db()).expect("create invalid history path fixture");
    let request = VideoRequest::new("example/video", "History failure fixture").expect("request");
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 40,
            provider_id: ProviderId::openrouter(),
            request,
        })
        .await
        .expect("send generate command");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionStarted { op_id: 40, .. })
    })
    .await;
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::JobAccepted {
            op_id: 40,
            ref job,
            record: None,
            ..
        } if job.id == "job-history-failure"
    ));
    let error = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 40, .. })
    })
    .await;
    assert!(matches!(
        error,
        ServiceEvent::Error {
            scope: ServiceScope::History,
            job_id: Some(ref value),
            ..
        } if value == "job-history-failure"
    ));
    assert_eq!(
        executor
            .requests()
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn both_recovery_store_failures_emit_terminal_event_with_remote_id() {
    let (_root, paths) = fixture_paths();
    let post_started = Arc::new(Notify::new());
    let release_post = Arc::new(Notify::new());
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::WaitJson {
            status: StatusCode::OK,
            value: pending_job("job-no-local-recovery"),
            started: Arc::clone(&post_started),
            release: Arc::clone(&release_post),
        },
    ]);
    let mut service = spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor)
        .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;

    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 41,
            provider_id: ProviderId::openrouter(),
            request: VideoRequest::new("example/video", "No local recovery fixture")
                .expect("request"),
        })
        .await
        .expect("send generation");
    event_matching(&mut service, |event| {
        matches!(
            event,
            ServiceEvent::UncertainSubmissionSaved { op_id: 41, .. }
        )
    })
    .await;
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionStarted { op_id: 41, .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), post_started.notified())
        .await
        .expect("paid POST did not start");
    for database in [paths.gui_state_db(), paths.history_db()] {
        fs::remove_file(&database).expect("remove initialized database");
        fs::create_dir(&database).expect("replace database with invalid directory");
    }
    release_post.notify_one();
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::JobAccepted {
            op_id: 41,
            ref job,
            ..
        } if job.id == "job-no-local-recovery"
    ));
    let failed = next_event(&mut service).await;
    assert!(matches!(
        failed,
        ServiceEvent::JobRecoveryFailed {
            op_id: 41,
            ref key,
            ref message,
            ..
        } if key.remote_job_id == "job-no-local-recovery"
            && message.contains("GUI recovery state also failed")
    ));
    shutdown(&mut service).await;
}

#[tokio::test]
async fn ambiguous_paid_post_has_an_explicit_non_retryable_event() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": {"message": "upstream timed out after accepting work"}}),
        ),
    ]);
    let mut service = spawn_service_with_executor(paths, config(Duration::ZERO), executor.clone())
        .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 42,
            provider_id: ProviderId::openrouter(),
            request: VideoRequest::new("example/video", "Ambiguous submit fixture")
                .expect("request"),
        })
        .await
        .expect("send generation");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionStarted { op_id: 42, .. })
    })
    .await;
    let uncertain = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionUncertain { op_id: 42, .. })
    })
    .await;
    assert!(matches!(
        uncertain,
        ServiceEvent::SubmissionUncertain {
            op_id: 42,
            ref message,
            ..
        } if message.contains("upstream timed out")
    ));
    assert_eq!(
        executor
            .requests()
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn pre_submit_marker_survives_ambiguity_and_blocks_same_draft_after_restart() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"message": "ambiguous upstream response"}}),
        ),
    ]);
    let mut service =
        spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor.clone())
            .expect("spawn first service");
    connect_fixture_key(&mut service, 1).await;
    let request =
        VideoRequest::new("example/video", "  Persistent   ambiguity  ").expect("request");
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 43,
            provider_id: ProviderId::openrouter(),
            request,
        })
        .await
        .expect("send ambiguous generation");
    let saved = event_matching(&mut service, |event| {
        matches!(
            event,
            ServiceEvent::UncertainSubmissionSaved { op_id: 43, .. }
        )
    })
    .await;
    let ServiceEvent::UncertainSubmissionSaved { record, .. } = saved else {
        unreachable!()
    };
    let fingerprint = record.draft_fingerprint.clone();
    let uncertain = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionUncertain { op_id: 43, .. })
    })
    .await;
    assert!(matches!(
        uncertain,
        ServiceEvent::SubmissionUncertain {
            draft_fingerprint: Some(ref value),
            ..
        } if value == &fingerprint
    ));
    assert_eq!(
        GuiStateStore::new(paths.gui_state_db())
            .uncertain_submissions()
            .expect("persisted safety set"),
        vec![record.clone()]
    );
    shutdown(&mut service).await;

    let restart_executor = WorkflowExecutor::default();
    let mut restarted = spawn_service_with_executor(
        paths.clone(),
        config(Duration::ZERO),
        Arc::new(restart_executor),
    )
    .expect("restart service");
    assert!(matches!(
        next_event(&mut restarted).await,
        ServiceEvent::Ready { .. }
    ));
    let loaded = event_matching(&mut restarted, |event| {
        matches!(
            event,
            ServiceEvent::UncertainSubmissionsLoaded { op_id: 0, .. }
        )
    })
    .await;
    assert!(matches!(
        loaded,
        ServiceEvent::UncertainSubmissionsLoaded { ref records, .. }
            if records == &vec![record.clone()]
    ));

    // A materially different draft passes the safety barrier (then stops at
    // the intentionally absent credential), without deleting the old row.
    let distinct = GenerationDraft::new(
        ProviderId::openrouter(),
        "example/video",
        "Persistent ambiguity at sunset",
    )
    .expect("distinct draft");
    restarted
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 44,
            draft: distinct,
            revision: 2,
        })
        .await
        .expect("prepare distinct draft");
    assert!(matches!(
        event_matching(&mut restarted, |event| matches!(
            event,
            ServiceEvent::Error { op_id: 44, .. }
        ))
        .await,
        ServiceEvent::Error {
            scope: ServiceScope::Credential,
            ..
        }
    ));
    assert_eq!(
        GuiStateStore::new(paths.gui_state_db())
            .uncertain_submissions()
            .expect("old row retained"),
        vec![record.clone()]
    );

    // Whitespace edits and undo revision changes cannot bypass the digest.
    let same = GenerationDraft::new(
        ProviderId::openrouter(),
        " example/video ",
        "Persistent ambiguity",
    )
    .expect("same semantic draft");
    restarted
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 45,
            draft: same.clone(),
            revision: 99,
        })
        .await
        .expect("prepare blocked draft");
    assert!(matches!(
        event_matching(&mut restarted, |event| matches!(
            event,
            ServiceEvent::UncertainSubmissionBlocked { op_id: 45, .. }
        ))
        .await,
        ServiceEvent::UncertainSubmissionBlocked { ref record, .. }
            if record.draft_fingerprint == fingerprint
    ));

    restarted
        .commands
        .send(ServiceCommand::ClearUncertainSubmission {
            op_id: 46,
            provider_id: ProviderId::openrouter(),
            draft_fingerprint: fingerprint,
        })
        .await
        .expect("acknowledge dashboard check");
    assert!(matches!(
        event_matching(&mut restarted, |event| matches!(
            event,
            ServiceEvent::UncertainSubmissionCleared { op_id: 46, .. }
        ))
        .await,
        ServiceEvent::UncertainSubmissionCleared { removed: true, .. }
    ));
    restarted
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 47,
            draft: same,
            revision: 100,
        })
        .await
        .expect("prepare acknowledged draft");
    assert!(matches!(
        event_matching(&mut restarted, |event| matches!(
            event,
            ServiceEvent::Error { op_id: 47, .. }
        ))
        .await,
        ServiceEvent::Error {
            scope: ServiceScope::Credential,
            ..
        }
    ));
    shutdown(&mut restarted).await;
}

#[tokio::test]
async fn active_paid_post_rejects_marker_clear_until_recovery_is_durable() {
    let (_root, paths) = fixture_paths();
    let post_started = Arc::new(Notify::new());
    let release_post = Arc::new(Notify::new());
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::WaitJson {
            status: StatusCode::OK,
            value: pending_job("job-clear-race"),
            started: Arc::clone(&post_started),
            release: Arc::clone(&release_post),
        },
    ]);
    let mut service = spawn_service_with_executor(
        paths.clone(),
        config(Duration::from_secs(30)),
        executor.clone(),
    )
    .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 48,
            provider_id: ProviderId::openrouter(),
            request: VideoRequest::new("example/video", "Marker clear race").expect("request"),
        })
        .await
        .expect("start generation");
    let saved = event_matching(&mut service, |event| {
        matches!(
            event,
            ServiceEvent::UncertainSubmissionSaved { op_id: 48, .. }
        )
    })
    .await;
    let ServiceEvent::UncertainSubmissionSaved { record, .. } = saved else {
        unreachable!()
    };
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionStarted { op_id: 48, .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), post_started.notified())
        .await
        .expect("paid POST did not start");

    service
        .commands
        .send(ServiceCommand::ClearUncertainSubmission {
            op_id: 49,
            provider_id: record.provider_id.clone(),
            draft_fingerprint: record.draft_fingerprint.clone(),
        })
        .await
        .expect("attempt unsafe clear");
    let rejected = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 49, .. })
    })
    .await;
    assert!(matches!(
        rejected,
        ServiceEvent::Error {
            recoverable: false,
            ref message,
            ..
        } if message.contains("still in progress")
    ));
    assert_eq!(
        GuiStateStore::new(paths.gui_state_db())
            .uncertain_submissions()
            .expect("marker still present"),
        vec![record.clone()]
    );

    release_post.notify_one();
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::JobRecoverySaved { op_id: 48, .. })
    })
    .await;
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::UncertainSubmissionCleared { op_id: 48, .. }
        ))
        .await,
        ServiceEvent::UncertainSubmissionCleared { removed: true, .. }
    ));
    assert!(
        GuiStateStore::new(paths.gui_state_db())
            .uncertain_submissions()
            .expect("cleared after durable recovery")
            .is_empty()
    );
    assert_eq!(
        executor
            .requests()
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn paid_post_is_not_sent_when_pre_submit_marker_cannot_be_persisted() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([Reply::Json(
        StatusCode::OK,
        json!({"data": {"label": "fixture"}}),
    )]);
    let mut service =
        spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor.clone())
            .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;

    fs::remove_file(paths.gui_state_db()).expect("remove GUI state database");
    fs::create_dir(paths.gui_state_db()).expect("replace GUI database with invalid directory");
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 401,
            provider_id: ProviderId::openrouter(),
            request: VideoRequest::new("example/video", "Fail-closed outbox").expect("request"),
        })
        .await
        .expect("attempt generation");
    let error = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 401, .. })
    })
    .await;
    assert!(matches!(
        error,
        ServiceEvent::Error {
            scope: ServiceScope::Generation,
            ..
        }
    ));
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method != Method::POST)
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn prepared_review_is_fresh_one_time_and_submits_exactly_once() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
        Reply::Json(StatusCode::OK, pending_job("job-prepared")),
    ]);
    let mut service = spawn_service_with_executor(
        paths.clone(),
        config(Duration::from_secs(30)),
        executor.clone(),
    )
    .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;

    let mut draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "A prepared generation fixture",
    )
    .expect("draft");
    draft.duration = Some(4);
    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 50,
            draft,
            revision: 1,
        })
        .await
        .expect("send prepare command");
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::PreparationStarted { op_id: 50, .. }
        ))
        .await,
        ServiceEvent::PreparationStarted { media_count: 0, .. }
    ));
    let review = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ReviewReady { op_id: 50, .. })
    })
    .await;
    let ServiceEvent::ReviewReady {
        prepared_id,
        revision,
        expires_at,
        ..
    } = review
    else {
        unreachable!()
    };
    assert_eq!(revision, 1);
    assert!(expires_at > chrono::Utc::now());

    service
        .commands
        .send(ServiceCommand::SubmitPrepared {
            op_id: 51,
            prepared_id,
        })
        .await
        .expect("send prepared submission");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::JobAccepted { op_id: 51, .. })
    })
    .await;
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::JobUpdated { op_id: 51, .. })
    })
    .await;

    let resumable = GuiStateStore::new(paths.gui_state_db())
        .resumable_jobs()
        .expect("read resumable GUI jobs");
    assert_eq!(resumable.len(), 1);
    assert_eq!(resumable[0].key.remote_job_id, "job-prepared");

    service
        .commands
        .send(ServiceCommand::SubmitPrepared {
            op_id: 52,
            prepared_id,
        })
        .await
        .expect("send duplicate prepared submission");
    let duplicate = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 52, .. })
    })
    .await;
    assert!(matches!(
        duplicate,
        ServiceEvent::Error {
            scope: ServiceScope::Preparation,
            ref message,
            ..
        } if message.contains("no longer valid")
    ));
    assert_eq!(
        executor
            .requests()
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );

    service
        .commands
        .send(ServiceCommand::PauseMonitor {
            op_id: 53,
            key: ProviderJobKey::new(ProviderId::openrouter(), "job-prepared")
                .expect("prepared key"),
        })
        .await
        .expect("pause prepared monitor");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::MonitorPaused { op_id: 53, .. })
    })
    .await;
    shutdown(&mut service).await;
}

#[tokio::test]
async fn draft_edit_invalidates_review_and_autosaves_without_posting() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
    ]);
    let mut service = spawn_service_with_executor(paths, config(Duration::ZERO), executor.clone())
        .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    let original = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "Original prompt",
    )
    .expect("draft");
    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 60,
            draft: original.clone(),
            revision: 1,
        })
        .await
        .expect("prepare original");
    let review = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ReviewReady { op_id: 60, .. })
    })
    .await;
    let ServiceEvent::ReviewReady { prepared_id, .. } = review else {
        unreachable!()
    };

    let mut edited = original;
    edited.prompt = "Edited prompt".into();
    service
        .commands
        .send(ServiceCommand::SaveDraft {
            op_id: 61,
            draft: edited.clone(),
            editor_state: DraftEditorState::default(),
            revision: 2,
        })
        .await
        .expect("autosave edit");
    let invalidated = next_event(&mut service).await;
    assert!(matches!(
        invalidated,
        ServiceEvent::PreparedInvalidated {
            op_id: 61,
            prepared_id: Some(value),
            revision: 2,
        } if value == prepared_id
    ));
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::DraftSaved {
            op_id: 61,
            revision: 2
        }
    ));
    service
        .commands
        .send(ServiceCommand::SubmitPrepared {
            op_id: 62,
            prepared_id,
        })
        .await
        .expect("submit stale Review");
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::Error { op_id: 62, .. }
        ))
        .await,
        ServiceEvent::Error {
            scope: ServiceScope::Preparation,
            ..
        }
    ));
    service
        .commands
        .send(ServiceCommand::LoadDraft { op_id: 63 })
        .await
        .expect("load draft");
    let loaded = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::DraftLoaded { op_id: 63, .. })
    })
    .await;
    assert!(matches!(
        loaded,
        ServiceEvent::DraftLoaded {
            draft: Some(ref value),
            revision: Some(2),
            ..
        } if value.prompt == edited.prompt
    ));
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method != Method::POST)
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn raw_editor_state_survives_service_restart_exactly() {
    let (_root, paths) = fixture_paths();
    let draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "Raw autosave fixture",
    )
    .expect("draft");
    let editor_state = DraftEditorState {
        seed_text: "-".into(),
        advanced_json_text: "{\n  \"guidance_scale\": 1.\n".into(),
        schema_text: std::collections::BTreeMap::from([
            ("motion_strength".into(), "-.".into()),
            ("freeform_note".into(), "unfinished [ text".into()),
        ]),
    };
    let mut service = spawn_service_with_executor(
        paths.clone(),
        config(Duration::ZERO),
        WorkflowExecutor::scripted([]),
    )
    .expect("spawn service");
    service
        .commands
        .send(ServiceCommand::SaveDraft {
            op_id: 66,
            draft: draft.clone(),
            editor_state: editor_state.clone(),
            revision: 9,
        })
        .await
        .expect("save raw editor state");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::DraftSaved { op_id: 66, .. })
    })
    .await;
    shutdown(&mut service).await;

    let mut reopened = spawn_service_with_executor(
        paths,
        config(Duration::ZERO),
        WorkflowExecutor::scripted([]),
    )
    .expect("reopen service");
    reopened
        .commands
        .send(ServiceCommand::LoadDraft { op_id: 67 })
        .await
        .expect("load raw editor state");
    let loaded = event_matching(&mut reopened, |event| {
        matches!(event, ServiceEvent::DraftLoaded { op_id: 67, .. })
    })
    .await;
    assert!(matches!(
        loaded,
        ServiceEvent::DraftLoaded {
            draft: Some(ref loaded_draft),
            editor_state: Some(ref loaded_editor),
            revision: Some(9),
            ..
        } if loaded_draft == &draft && loaded_editor == &editor_state
    ));
    shutdown(&mut reopened).await;
}

#[tokio::test]
async fn active_api_key_in_raw_editor_state_is_never_persisted() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
    ]);
    let mut service = spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor)
        .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    let draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "Credential leak fixture",
    )
    .expect("draft");
    let editor_state = DraftEditorState {
        advanced_json_text: json!({"note": FIXTURE_KEY}).to_string(),
        ..DraftEditorState::default()
    };
    service
        .commands
        .send(ServiceCommand::SaveDraft {
            op_id: 68,
            draft,
            editor_state,
            revision: 1,
        })
        .await
        .expect("request rejected autosave");
    let rejected = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 68, .. })
    })
    .await;
    assert!(matches!(
        rejected,
        ServiceEvent::Error {
            scope: ServiceScope::Draft,
            ref message,
            ..
        } if !message.contains(FIXTURE_KEY)
    ));
    assert_eq!(
        GuiStateStore::new(paths.gui_state_db())
            .load_draft()
            .expect("load rejected draft"),
        None
    );
    let database = fs::read(paths.gui_state_db()).expect("read state database");
    assert!(
        !database
            .windows(FIXTURE_KEY.len())
            .any(|window| window == FIXTURE_KEY.as_bytes()),
        "active API key must not appear in persisted database bytes"
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn active_api_key_never_crosses_quote_prepare_or_direct_generate_boundaries() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
    ]);
    let mut service =
        spawn_service_with_executor(paths.clone(), config(Duration::ZERO), executor.clone())
            .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    let baseline_requests = executor.requests().len();

    let quote_request = VideoRequest::new(
        "black-forest-labs/flux-3-video",
        format!("Quote must reject {FIXTURE_KEY}"),
    )
    .expect("quote request");
    service
        .commands
        .send(ServiceCommand::Quote {
            op_id: 70,
            provider_id: ProviderId::openrouter(),
            request: quote_request,
        })
        .await
        .expect("send unsafe quote");
    let quote_error = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 70, .. })
    })
    .await;
    assert!(matches!(
        quote_error,
        ServiceEvent::Error {
            scope: ServiceScope::Quote,
            ref message,
            ..
        } if !message.contains(FIXTURE_KEY)
    ));

    let mut prepare_draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "Prepare adapter credential fixture",
    )
    .expect("prepare draft");
    prepare_draft.adapter_options = Some(json!({"innocent_note": FIXTURE_KEY}));
    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 71,
            draft: prepare_draft,
            revision: 1,
        })
        .await
        .expect("send unsafe preparation");
    let prepare_error = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 71, .. })
    })
    .await;
    assert!(matches!(
        prepare_error,
        ServiceEvent::Error {
            scope: ServiceScope::Preparation,
            ref message,
            ..
        } if !message.contains(FIXTURE_KEY)
    ));

    let mut direct_request = VideoRequest::new(
        "black-forest-labs/flux-3-video",
        format!("Direct generation must reject {FIXTURE_KEY}"),
    )
    .expect("direct request");
    direct_request.adapter_options = Some(json!({"innocent_note": FIXTURE_KEY}));
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 72,
            provider_id: ProviderId::openrouter(),
            request: direct_request,
        })
        .await
        .expect("send unsafe direct generation");
    let generate_error = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 72, .. })
    })
    .await;
    assert!(matches!(
        generate_error,
        ServiceEvent::Error {
            scope: ServiceScope::Generation,
            ref message,
            ..
        } if !message.contains(FIXTURE_KEY)
    ));

    let requests = executor.requests();
    assert_eq!(
        requests.len(),
        baseline_requests,
        "unsafe provider inputs must be rejected before any HTTP request"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.method != Method::POST),
        "no paid request may be sent"
    );
    assert!(
        GuiStateStore::new(paths.gui_state_db())
            .uncertain_submissions()
            .expect("load outbox")
            .is_empty(),
        "direct Generate must reject the key before writing its paid-submit outbox"
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn legacy_draft_rows_receive_typed_editor_fallback() {
    let (_root, paths) = fixture_paths();
    GuiStateStore::new(paths.gui_state_db())
        .save_draft(&StoredDraft {
            revision: 12,
            provider_id: ProviderId::openrouter(),
            model_id: "black-forest-labs/flux-3-video".into(),
            prompt: "Legacy autosave fixture".into(),
            settings: json!({
                "seed": 41,
                "adapter_options": {"guidance_scale": 7.5}
            }),
            editor_state: None,
            media: Vec::new(),
        })
        .expect("save legacy-shaped row");
    let mut service = spawn_service_with_executor(
        paths,
        config(Duration::ZERO),
        WorkflowExecutor::scripted([]),
    )
    .expect("spawn service");
    service
        .commands
        .send(ServiceCommand::LoadDraft { op_id: 69 })
        .await
        .expect("load legacy draft");
    let loaded = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::DraftLoaded { op_id: 69, .. })
    })
    .await;
    let ServiceEvent::DraftLoaded {
        editor_state: Some(editor_state),
        revision: Some(12),
        ..
    } = loaded
    else {
        panic!("legacy row did not receive an editor fallback")
    };
    assert_eq!(editor_state.seed_text, "41");
    assert_eq!(
        serde_json::from_str::<Value>(&editor_state.advanced_json_text)
            .expect("fallback advanced JSON"),
        json!({"guidance_scale": 7.5})
    );
    assert!(editor_state.schema_text.is_empty());
    shutdown(&mut service).await;
}

#[tokio::test]
async fn credential_changes_invalidate_a_review_before_any_paid_post() {
    let (_root, paths) = fixture_paths();
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "first account"}})),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
        Reply::Json(StatusCode::OK, json!({"data": {"label": "second account"}})),
        Reply::Json(StatusCode::OK, fixture_json("catalog.json")),
    ]);
    let mut service = spawn_service_with_executor(paths, config(Duration::ZERO), executor.clone())
        .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    let draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "Credential invalidation fixture",
    )
    .expect("draft");
    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 64,
            draft: draft.clone(),
            revision: 1,
        })
        .await
        .expect("prepare first review");
    let first_review = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ReviewReady { op_id: 64, .. })
    })
    .await;
    let ServiceEvent::ReviewReady {
        prepared_id: first_id,
        ..
    } = first_review
    else {
        unreachable!()
    };

    service
        .commands
        .send(ServiceCommand::ConnectApiKey {
            op_id: 65,
            provider_id: ProviderId::openrouter(),
            key: SecretString::from("sk-test-second-account".to_owned()),
            persist_on_success: false,
        })
        .await
        .expect("connect replacement credential");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ApiKeyConnected { op_id: 65, .. })
    })
    .await;
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::PreparedInvalidated { op_id: 65, .. }
        ))
        .await,
        ServiceEvent::PreparedInvalidated {
            prepared_id: Some(value),
            ..
        } if value == first_id
    ));
    service
        .commands
        .send(ServiceCommand::SubmitPrepared {
            op_id: 66,
            prepared_id: first_id,
        })
        .await
        .expect("try invalidated first review");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 66, .. })
    })
    .await;

    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 661,
            draft,
            revision: 2,
        })
        .await
        .expect("prepare second review");
    let second_review = event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ReviewReady { op_id: 661, .. })
    })
    .await;
    let ServiceEvent::ReviewReady {
        prepared_id: second_id,
        ..
    } = second_review
    else {
        unreachable!()
    };
    service
        .commands
        .send(ServiceCommand::ForgetApiKey {
            op_id: 662,
            provider_id: ProviderId::openrouter(),
        })
        .await
        .expect("forget credential");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ApiKeyForgotten { op_id: 662, .. })
    })
    .await;
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::PreparedInvalidated { op_id: 662, .. }
        ))
        .await,
        ServiceEvent::PreparedInvalidated {
            prepared_id: Some(value),
            ..
        } if value == second_id
    ));
    service
        .commands
        .send(ServiceCommand::SubmitPrepared {
            op_id: 663,
            prepared_id: second_id,
        })
        .await
        .expect("try invalidated second review");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 663, .. })
    })
    .await;
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method != Method::POST)
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn credential_swap_during_held_preparation_never_publishes_a_review() {
    let (_root, paths) = fixture_paths();
    let quote_started = Arc::new(Notify::new());
    let release_quote = Arc::new(Notify::new());
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "first account"}})),
        Reply::WaitJson {
            status: StatusCode::OK,
            value: fixture_json("catalog.json"),
            started: Arc::clone(&quote_started),
            release: Arc::clone(&release_quote),
        },
        Reply::Json(StatusCode::OK, json!({"data": {"label": "second account"}})),
    ]);
    let mut service = spawn_service_with_executor(paths, config(Duration::ZERO), executor.clone())
        .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    let draft = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "Held credential-swap fixture",
    )
    .expect("draft");
    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 73,
            draft,
            revision: 1,
        })
        .await
        .expect("start held preparation");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::PreparationStarted { op_id: 73, .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), quote_started.notified())
        .await
        .expect("old-account quote did not start");

    service
        .commands
        .send(ServiceCommand::ConnectApiKey {
            op_id: 74,
            provider_id: ProviderId::openrouter(),
            key: SecretString::from("sk-test-second-account".to_owned()),
            persist_on_success: false,
        })
        .await
        .expect("connect replacement credential");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::ApiKeyConnected { op_id: 74, .. })
    })
    .await;
    release_quote.notify_one();

    let mut saw_review = false;
    loop {
        match next_event(&mut service).await {
            ServiceEvent::ReviewReady { op_id: 73, .. } => saw_review = true,
            ServiceEvent::PreparedInvalidated {
                op_id: 73,
                prepared_id: None,
                ..
            } => break,
            ServiceEvent::Error { message, .. } => {
                panic!("unexpected workflow error: {message}")
            }
            _ => {}
        }
    }
    assert!(
        !saw_review,
        "an old-account preparation must not publish a Review after credentials change"
    );
    service
        .commands
        .send(ServiceCommand::SubmitPrepared {
            op_id: 75,
            prepared_id: PreparedGenerationId(1),
        })
        .await
        .expect("attempt stale submission");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Error { op_id: 75, .. })
    })
    .await;
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method != Method::POST),
        "credential swap must leave no submit-ready token"
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn edit_during_preparation_discards_the_stale_review_token() {
    let (_root, paths) = fixture_paths();
    let quote_started = Arc::new(Notify::new());
    let release_quote = Arc::new(Notify::new());
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::WaitJson {
            status: StatusCode::OK,
            value: fixture_json("catalog.json"),
            started: Arc::clone(&quote_started),
            release: Arc::clone(&release_quote),
        },
    ]);
    let mut service = spawn_service_with_executor(paths, config(Duration::ZERO), executor.clone())
        .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    let original = GenerationDraft::new(
        ProviderId::openrouter(),
        "black-forest-labs/flux-3-video",
        "Preparing prompt",
    )
    .expect("draft");
    service
        .commands
        .send(ServiceCommand::PrepareGeneration {
            op_id: 64,
            draft: original.clone(),
            revision: 10,
        })
        .await
        .expect("start preparation");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::PreparationStarted { op_id: 64, .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), quote_started.notified())
        .await
        .expect("quote request did not start");

    let mut edited = original;
    edited.prompt = "Changed while quote was loading".into();
    service
        .commands
        .send(ServiceCommand::SaveDraft {
            op_id: 65,
            draft: edited,
            editor_state: DraftEditorState::default(),
            revision: 11,
        })
        .await
        .expect("save edited draft");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::DraftSaved { op_id: 65, .. })
    })
    .await;
    release_quote.notify_one();
    let invalidated = event_matching(&mut service, |event| {
        matches!(
            event,
            ServiceEvent::PreparedInvalidated {
                op_id: 64,
                revision: 11,
                ..
            }
        )
    })
    .await;
    assert!(matches!(
        invalidated,
        ServiceEvent::PreparedInvalidated {
            prepared_id: None,
            ..
        }
    ));

    service
        .commands
        .send(ServiceCommand::SubmitPrepared {
            op_id: 66,
            prepared_id: PreparedGenerationId(1),
        })
        .await
        .expect("try stale token");
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::Error { op_id: 66, .. }
        ))
        .await,
        ServiceEvent::Error {
            scope: ServiceScope::Preparation,
            ..
        }
    ));
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method != Method::POST)
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn shutdown_is_blocked_until_a_paid_post_returns_a_remote_id() {
    let (_root, paths) = fixture_paths();
    let post_started = Arc::new(Notify::new());
    let release_post = Arc::new(Notify::new());
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::WaitJson {
            status: StatusCode::OK,
            value: pending_job("job-close-guard"),
            started: Arc::clone(&post_started),
            release: Arc::clone(&release_post),
        },
    ]);
    let mut service =
        spawn_service_with_executor(paths, config(Duration::from_secs(30)), executor.clone())
            .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    service
        .commands
        .send(ServiceCommand::Generate {
            op_id: 67,
            provider_id: ProviderId::openrouter(),
            request: VideoRequest::new("example/video", "Close guard fixture").expect("request"),
        })
        .await
        .expect("start paid submission");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::SubmissionStarted { op_id: 67, .. })
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), post_started.notified())
        .await
        .expect("paid POST did not start");
    service
        .commands
        .send(ServiceCommand::Shutdown)
        .await
        .expect("request shutdown during POST");
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::ShutdownBlocked { .. }
    ));

    release_post.notify_one();
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::JobAccepted { op_id: 67, .. })
    })
    .await;
    service
        .commands
        .send(ServiceCommand::CancelCurrent { op_id: 68 })
        .await
        .expect("pause accepted job");
    event_matching(&mut service, |event| {
        matches!(event, ServiceEvent::Cancelled { op_id: 67, .. })
    })
    .await;
    assert_eq!(
        executor
            .requests()
            .iter()
            .filter(|request| request.method == Method::POST)
            .count(),
        1
    );
    shutdown(&mut service).await;
}

#[tokio::test]
async fn two_saved_jobs_monitor_concurrently_and_pause_together() {
    let (_root, paths) = fixture_paths();
    let request = VideoRequest::new("example/video", "Concurrent fixture").expect("request");
    let history = HistoryStore::new(paths.history_db());
    for id in ["job-concurrent-a", "job-concurrent-b"] {
        history
            .create_job(
                &request,
                &VideoJob::from_api(&pending_job(id)).expect("pending job"),
            )
            .expect("seed history");
    }
    let executor = WorkflowExecutor::scripted([
        Reply::Json(StatusCode::OK, json!({"data": {"label": "fixture"}})),
        Reply::Json(StatusCode::OK, pending_job("job-concurrent-a")),
        Reply::Json(StatusCode::OK, pending_job("job-concurrent-b")),
    ]);
    let mut service =
        spawn_service_with_executor(paths, config(Duration::from_secs(30)), executor.clone())
            .expect("spawn service");
    connect_fixture_key(&mut service, 1).await;
    for (op_id, id) in [(70, "job-concurrent-a"), (71, "job-concurrent-b")] {
        service
            .commands
            .send(ServiceCommand::Resume {
                op_id,
                key: ProviderJobKey::new(ProviderId::openrouter(), id).expect("job key"),
            })
            .await
            .expect("resume job");
        event_matching(&mut service, |event| {
            matches!(event, ServiceEvent::PollWaiting { op_id: value, .. } if *value == op_id)
        })
        .await;
    }
    service
        .commands
        .send(ServiceCommand::PauseAll { op_id: 72 })
        .await
        .expect("pause all");
    assert!(matches!(
        event_matching(&mut service, |event| matches!(
            event,
            ServiceEvent::MonitorsPaused { op_id: 72, .. }
        ))
        .await,
        ServiceEvent::MonitorsPaused {
            count: 2,
            remote_continue: true,
            ..
        }
    ));
    assert!(
        executor
            .requests()
            .iter()
            .all(|request| request.method == Method::GET)
    );
    shutdown(&mut service).await;
}
