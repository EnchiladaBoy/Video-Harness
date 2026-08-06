use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use openrouter_video_studio::AppPaths;
use openrouter_video_studio::api::{
    ClientOptions, HttpExecutor, HttpRequest, HttpResponse, TransportError,
};
use openrouter_video_studio::domain::{
    JobLocator, JobStatus, ProviderId, ProviderJobKey, VideoJob, VideoRequest,
};
use openrouter_video_studio::history::HistoryStore;
use openrouter_video_studio::workflow::{
    ServiceCommand, ServiceConfig, ServiceEvent, ServiceHandle, ServiceScope,
    spawn_service_with_executor,
};
use reqwest::header::{AUTHORIZATION, HeaderMap};
use reqwest::{Method, StatusCode};
use secrecy::SecretString;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use url::Url;

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
        match self.replies.lock().expect("reply lock").pop_front() {
            Some(Reply::Json(status, value)) => {
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
    service
        .commands
        .send(ServiceCommand::Shutdown)
        .await
        .expect("send shutdown command");
    event_matching(service, |event| {
        matches!(event, ServiceEvent::ShutdownComplete)
    })
    .await;
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

    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::SubmissionStarted { op_id: 10, .. }
    ));
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
    let persisted = next_event(&mut service).await;
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
async fn resume_enforces_one_active_operation_and_cancels_without_posting() {
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
        .send(ServiceCommand::Generate {
            op_id: 31,
            provider_id: ProviderId::openrouter(),
            request: request.clone(),
        })
        .await
        .expect("send competing generate");
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
        } if message.contains("already active")
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
    assert!(matches!(
        next_event(&mut service).await,
        ServiceEvent::SubmissionStarted { op_id: 40, .. }
    ));
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
