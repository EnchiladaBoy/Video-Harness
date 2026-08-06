//! Provider-aware long-running service bridge between the reducer and backend I/O.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::api::{ClientOptions, DownloadProgress, HttpExecutor, OpenRouterClient};
use crate::config::{
    AppPaths, AppSettings, ConfigError, ModelSettingsMap, load_app_settings, load_model_settings,
    make_output_path, partial_path, save_app_settings, save_model_settings,
};
use crate::credentials::{CredentialStatus, CredentialStore};
use crate::domain::{
    CostQuote, DraftMedia, GenerationDraft, InputReferenceKind, JobLocator, JobStatus, MediaRole,
    MediaSource, ProviderDescriptor, ProviderId, ProviderJobKey, StagedMedia, UploadReceipt,
    VideoCatalog, VideoJob, VideoRequest,
};
use crate::gui_state::{
    BeginUncertainSubmission, DraftEditorState, GenerationMediaAssociation, GuiStateError,
    GuiStateStore, ResumableJob, StoredDraft, StoredDraftMedia, StoredMediaSource,
    StoredUploadReceipt, UncertainSubmissionRecord, generation_draft_fingerprint_candidates,
};
use crate::history::{HistoryError, HistoryStore, JobRecord};
use crate::providers::fal::{FalOptions, FalProvider};
use crate::providers::openrouter::OpenRouterProvider;
use crate::providers::{
    ProviderAccount, ProviderError, ProviderErrorKind, UploadProgress, VideoProvider,
};

/// A Review remains billable-action-ready for only a short window. Any edit
/// invalidates it immediately, and submission consumes it exactly once.
pub const PREPARED_REVIEW_LIFETIME: Duration = Duration::from_secs(5 * 60);
pub const STAGED_MEDIA_EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PreparedGenerationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStore {
    /// The GUI sidecar contains the accepted provider locator.
    GuiState,
    /// Compatible generation history contains the accepted provider locator.
    History,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub poll_interval: Duration,
    pub max_poll_attempts: usize,
    pub client_options: ClientOptions,
    /// Disable in tests to guarantee no process touches the user's real keyring.
    pub use_system_credentials: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            max_poll_attempts: 60,
            client_options: ClientOptions::default(),
            use_system_credentials: true,
        }
    }
}

#[derive(Debug)]
pub enum ServiceCommand {
    ConnectApiKey {
        op_id: u64,
        provider_id: ProviderId,
        key: SecretString,
        persist_on_success: bool,
    },
    ForgetApiKey {
        op_id: u64,
        provider_id: ProviderId,
    },
    RefreshCatalog {
        op_id: u64,
        provider_id: ProviderId,
    },
    Quote {
        op_id: u64,
        provider_id: ProviderId,
        request: VideoRequest,
    },
    /// Validate and stage media, then obtain the fresh quote shown by Review.
    /// This never performs a billable generation submission.
    PrepareGeneration {
        op_id: u64,
        draft: GenerationDraft,
        revision: u64,
    },
    /// Consume a still-fresh Review and perform exactly one paid submission.
    SubmitPrepared {
        op_id: u64,
        prepared_id: PreparedGenerationId,
    },
    /// Explicit edit notification used to revoke a visible Review immediately.
    InvalidatePrepared {
        op_id: u64,
        revision: u64,
    },
    SaveDraft {
        op_id: u64,
        draft: GenerationDraft,
        editor_state: DraftEditorState,
        revision: u64,
    },
    LoadDraft {
        op_id: u64,
    },
    /// Explicitly persist a credential-free safety barrier. Normal paid
    /// submissions do this automatically before the provider call.
    SaveUncertainSubmission {
        op_id: u64,
        provider_id: ProviderId,
        draft_fingerprint: String,
    },
    LoadUncertainSubmissions {
        op_id: u64,
    },
    /// Explicit acknowledgement after the user checks the provider dashboard.
    ClearUncertainSubmission {
        op_id: u64,
        provider_id: ProviderId,
        draft_fingerprint: String,
    },
    SaveModelSettings {
        op_id: u64,
        provider_id: ProviderId,
        model_id: String,
        settings_json: Value,
    },
    SaveDefaultProvider {
        op_id: u64,
        provider_id: ProviderId,
    },
    Generate {
        op_id: u64,
        provider_id: ProviderId,
        request: VideoRequest,
    },
    Resume {
        op_id: u64,
        key: ProviderJobKey,
    },
    ResumeAll {
        op_id: u64,
    },
    Import {
        op_id: u64,
        provider_id: ProviderId,
        locator: JobLocator,
    },
    CancelCurrent {
        op_id: u64,
    },
    PauseMonitor {
        op_id: u64,
        key: ProviderJobKey,
    },
    PauseAll {
        op_id: u64,
    },
    LoadHistory {
        op_id: u64,
        limit: usize,
    },
    OpenVideo {
        op_id: u64,
        path: PathBuf,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceScope {
    Startup,
    Credential,
    Catalog,
    Quote,
    Preparation,
    Draft,
    Settings,
    Generation,
    Import,
    History,
    OpenVideo,
}

#[derive(Debug, Clone)]
pub struct ProviderConnection {
    pub descriptor: ProviderDescriptor,
    pub connected: bool,
    pub credential_status: CredentialStatus,
}

#[derive(Debug, Clone)]
pub enum ServiceEvent {
    Ready {
        providers: Vec<ProviderConnection>,
        default_provider: ProviderId,
    },
    ApiKeyConnected {
        op_id: u64,
        provider_id: ProviderId,
        info: ProviderAccount,
        credential_status: CredentialStatus,
    },
    ApiKeyForgotten {
        op_id: u64,
        provider_id: ProviderId,
        credential_status: CredentialStatus,
    },
    CatalogLoaded {
        op_id: u64,
        provider_id: ProviderId,
        catalog: VideoCatalog,
        remembered_settings: ModelSettingsMap,
    },
    QuoteReady {
        op_id: u64,
        provider_id: ProviderId,
        model_id: String,
        quote: CostQuote,
    },
    PreparationStarted {
        op_id: u64,
        provider_id: ProviderId,
        media_count: usize,
    },
    MediaUploadStarted {
        op_id: u64,
        provider_id: ProviderId,
        media_index: usize,
        path: PathBuf,
    },
    MediaUploadProgress {
        op_id: u64,
        provider_id: ProviderId,
        media_index: usize,
        sent: u64,
        total: u64,
    },
    MediaUploadCompleted {
        op_id: u64,
        provider_id: ProviderId,
        media_index: usize,
        public_url: String,
        reused: bool,
        expires_at: Option<DateTime<Utc>>,
    },
    ReviewReady {
        op_id: u64,
        prepared_id: PreparedGenerationId,
        revision: u64,
        provider_id: ProviderId,
        request: VideoRequest,
        quote: CostQuote,
        expires_at: DateTime<Utc>,
        draft_fingerprint: String,
    },
    PreparedInvalidated {
        op_id: u64,
        prepared_id: Option<PreparedGenerationId>,
        revision: u64,
    },
    DraftSaved {
        op_id: u64,
        revision: u64,
    },
    DraftLoaded {
        op_id: u64,
        draft: Option<GenerationDraft>,
        editor_state: Option<DraftEditorState>,
        revision: Option<u64>,
    },
    UncertainSubmissionSaved {
        op_id: u64,
        record: UncertainSubmissionRecord,
    },
    UncertainSubmissionCleared {
        op_id: u64,
        provider_id: ProviderId,
        draft_fingerprint: String,
        removed: bool,
    },
    UncertainSubmissionsLoaded {
        op_id: u64,
        records: Vec<UncertainSubmissionRecord>,
    },
    /// An unresolved pre-submit intent already exists for this exact draft.
    /// Review/submission is blocked until explicit acknowledgement.
    UncertainSubmissionBlocked {
        op_id: u64,
        record: UncertainSubmissionRecord,
    },
    SettingsSaved {
        op_id: u64,
        provider_id: ProviderId,
        model_id: String,
    },
    DefaultProviderSaved {
        op_id: u64,
        provider_id: ProviderId,
    },
    SubmissionStarted {
        op_id: u64,
        provider_id: ProviderId,
    },
    JobAccepted {
        op_id: u64,
        provider_id: ProviderId,
        job: VideoJob,
        /// Emitted as `None` immediately after the paid POST. This makes the
        /// remote id recoverable even if the following local write fails.
        record: Option<JobRecord>,
    },
    /// Emitted only after at least one local database durably contains the
    /// accepted provider locator. `JobAccepted` always precedes this event.
    JobRecoverySaved {
        op_id: u64,
        provider_id: ProviderId,
        key: ProviderJobKey,
        store: RecoveryStore,
    },
    /// Neither local recovery database could save an already accepted job.
    /// This is terminal locally; the remote provider job may still continue.
    JobRecoveryFailed {
        op_id: u64,
        provider_id: ProviderId,
        key: ProviderJobKey,
        message: String,
    },
    /// Recovery is durable, but optional local metadata could not be saved.
    JobRecoveryWarning {
        op_id: u64,
        provider_id: ProviderId,
        key: ProviderJobKey,
        message: String,
    },
    /// The paid POST may have reached the provider, but no remote id was
    /// returned. Retrying could create a duplicate billable generation.
    SubmissionUncertain {
        op_id: u64,
        provider_id: ProviderId,
        message: String,
        draft_fingerprint: Option<String>,
    },
    JobUpdated {
        op_id: u64,
        provider_id: ProviderId,
        job: VideoJob,
        record: JobRecord,
    },
    PollWaiting {
        op_id: u64,
        provider_id: ProviderId,
        job_id: String,
        attempt: usize,
        next_in: Duration,
    },
    DownloadProgress {
        op_id: u64,
        provider_id: ProviderId,
        job_id: String,
        written: u64,
        total: Option<u64>,
    },
    Downloaded {
        op_id: u64,
        provider_id: ProviderId,
        job: VideoJob,
        record: JobRecord,
        path: PathBuf,
    },
    HistoryLoaded {
        op_id: u64,
        records: Vec<JobRecord>,
    },
    Imported {
        op_id: u64,
        provider_id: ProviderId,
        job: VideoJob,
        record: JobRecord,
    },
    MonitorPaused {
        op_id: u64,
        key: ProviderJobKey,
        remote_continues: bool,
    },
    MonitorsPaused {
        op_id: u64,
        count: usize,
        remote_continue: bool,
    },
    ResumeAllStarted {
        op_id: u64,
        started: usize,
        skipped: usize,
    },
    ResumableJobsLoaded {
        op_id: u64,
        jobs: Vec<ResumableJob>,
    },
    Cancelled {
        op_id: u64,
        provider_id: Option<ProviderId>,
        job_id: Option<String>,
        remote_continues: bool,
    },
    VideoOpened {
        op_id: u64,
        path: PathBuf,
    },
    Error {
        op_id: u64,
        provider_id: Option<ProviderId>,
        scope: ServiceScope,
        message: String,
        recoverable: bool,
        job_id: Option<String>,
    },
    ShutdownBlocked {
        reason: String,
    },
    ShutdownComplete,
}

pub struct ServiceHandle {
    pub commands: mpsc::Sender<ServiceCommand>,
    pub events: mpsc::UnboundedReceiver<ServiceEvent>,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
}

struct ActiveOperation {
    task_id: u64,
    op_id: u64,
    cancel: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
enum OperationNotice {
    Monitoring { task_id: u64, key: ProviderJobKey },
    Finished { task_id: u64 },
}

struct PreparedGeneration {
    id: PreparedGenerationId,
    revision: u64,
    draft: GenerationDraft,
    request: VideoRequest,
    staged_media: Vec<StagedMedia>,
    quote: CostQuote,
    expires_at: DateTime<Utc>,
    deadline: Instant,
    draft_fingerprint: String,
    draft_fingerprint_candidates: Vec<String>,
}

struct PreparedPayload {
    revision: u64,
    draft: GenerationDraft,
    request: VideoRequest,
    staged_media: Vec<StagedMedia>,
    quote: CostQuote,
    draft_fingerprint: String,
    draft_fingerprint_candidates: Vec<String>,
}

struct PreparationOutcome {
    op_id: u64,
    result: Result<PreparedPayload, TaskFailure>,
}

#[derive(Debug, Clone)]
struct PreparedMediaAssociation {
    position: usize,
    draft_media_id: String,
    role: String,
    source: StoredMediaSource,
    resolved_url: String,
}

struct ProviderSession {
    store: Arc<Mutex<CredentialStore>>,
    key: Option<SecretString>,
}

pub fn spawn_service(
    paths: AppPaths,
    config: ServiceConfig,
) -> Result<ServiceHandle, ServiceError> {
    spawn_service_inner(paths, config, None)
}

pub fn spawn_service_with_executor(
    paths: AppPaths,
    config: ServiceConfig,
    executor: Arc<dyn HttpExecutor>,
) -> Result<ServiceHandle, ServiceError> {
    spawn_service_inner(paths, config, Some(executor))
}

fn spawn_service_inner(
    paths: AppPaths,
    config: ServiceConfig,
    executor: Option<Arc<dyn HttpExecutor>>,
) -> Result<ServiceHandle, ServiceError> {
    paths.ensure_dirs()?;
    let (commands, command_rx) = mpsc::channel(32);
    let (event_tx, events) = mpsc::unbounded_channel();
    tokio::spawn(run_service(paths, config, executor, command_rx, event_tx));
    Ok(ServiceHandle { commands, events })
}

async fn run_service(
    paths: AppPaths,
    config: ServiceConfig,
    executor: Option<Arc<dyn HttpExecutor>>,
    mut commands: mpsc::Receiver<ServiceCommand>,
    events: mpsc::UnboundedSender<ServiceEvent>,
) {
    let history = HistoryStore::new(paths.history_db());
    let gui_state = GuiStateStore::new(paths.gui_state_db());
    let init_history = history.clone();
    let init_gui_state = gui_state.clone();
    let initialized = tokio::task::spawn_blocking(move || {
        init_history
            .initialize()
            .map_err(|error| error.to_string())?;
        init_gui_state
            .initialize()
            .map_err(|error| error.to_string())
    })
    .await;
    match initialized {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            emit_error(
                &events,
                0,
                None,
                ServiceScope::Startup,
                message,
                false,
                None,
            );
            return;
        }
        Err(_) => {
            emit_error(
                &events,
                0,
                None,
                ServiceScope::Startup,
                "Application database initialization failed".into(),
                false,
                None,
            );
            return;
        }
    }

    let mut sessions = BTreeMap::new();
    for provider_id in [ProviderId::openrouter(), ProviderId::fal()] {
        let use_system_credentials = config.use_system_credentials;
        let id = provider_id.clone();
        let (store, key) = tokio::task::spawn_blocking(move || {
            let mut store = if use_system_credentials {
                CredentialStore::for_provider(&id)
            } else {
                CredentialStore::memory_only_for_provider(&id)
            };
            let key = store.get();
            (store, key)
        })
        .await
        .unwrap_or_else(|_| {
            let store = CredentialStore::memory_only_for_provider(&provider_id);
            (store, None)
        });
        sessions.insert(
            provider_id,
            ProviderSession {
                store: Arc::new(Mutex::new(store)),
                key,
            },
        );
    }
    let app_settings_path = paths.app_settings();
    let default_provider = tokio::task::spawn_blocking(move || {
        load_app_settings(&app_settings_path)
            .unwrap_or_default()
            .default_provider
    })
    .await
    .unwrap_or_else(|_| ProviderId::openrouter());
    let provider_states = sessions
        .iter()
        .map(|(provider_id, session)| connection(provider_id, session))
        .collect();
    let _ = events.send(ServiceEvent::Ready {
        providers: provider_states,
        default_provider,
    });
    let startup_state = gui_state.clone();
    if let Ok(Ok(jobs)) = tokio::task::spawn_blocking(move || startup_state.resumable_jobs()).await
    {
        let _ = events.send(ServiceEvent::ResumableJobsLoaded { op_id: 0, jobs });
    }
    let startup_state = gui_state.clone();
    match tokio::task::spawn_blocking(move || startup_state.uncertain_submissions()).await {
        Ok(Ok(records)) => {
            let _ = events.send(ServiceEvent::UncertainSubmissionsLoaded { op_id: 0, records });
        }
        Ok(Err(error)) => emit_gui_state_error(&events, 0, ServiceScope::Startup, error),
        Err(_) => emit_error(
            &events,
            0,
            None,
            ServiceScope::Startup,
            "Uncertain-submission safety state could not be loaded".into(),
            false,
            None,
        ),
    }

    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<OperationNotice>();
    let (preparation_tx, mut preparation_rx) = mpsc::unbounded_channel::<PreparationOutcome>();
    let (catalog_finished_tx, mut catalog_finished_rx) = mpsc::unbounded_channel::<u64>();
    let mut catalog_tasks = BTreeMap::<u64, tokio::task::JoinHandle<()>>::new();
    let mut operations = BTreeMap::<u64, ActiveOperation>::new();
    let mut monitors = BTreeMap::<ProviderJobKey, u64>::new();
    let mut preparation: Option<ActiveOperation> = None;
    let mut preparation_invalidated = false;
    let mut prepared: Option<PreparedGeneration> = None;
    let mut current_revision: Option<u64> = None;
    let mut submission_task: Option<u64> = None;
    let mut next_task_id = 1u64;
    let mut next_prepared_id = 1u64;
    let mut shutting_down = false;

    loop {
        tokio::select! {
            Some(task_id) = catalog_finished_rx.recv() => {
                if let Some(task) = catalog_tasks.remove(&task_id) {
                    let _ = task.await;
                }
            }
            Some(outcome) = preparation_rx.recv() => {
                if preparation.as_ref().is_some_and(|operation| operation.op_id == outcome.op_id)
                    && let Some(operation) = preparation.take()
                {
                    let _ = operation.task.await;
                }
                match outcome.result {
                    Ok(payload) => {
                        if preparation_invalidated
                            || current_revision.is_some_and(|revision| revision != payload.revision)
                        {
                            let _ = events.send(ServiceEvent::PreparedInvalidated {
                                op_id: outcome.op_id,
                                prepared_id: None,
                                revision: current_revision.unwrap_or(payload.revision),
                            });
                        } else {
                            let now = Utc::now();
                            if let Some((expires_at, lifetime)) =
                                prepared_review_window(&payload.staged_media, now)
                            {
                                let id = PreparedGenerationId(next_prepared_id);
                                next_prepared_id = next_prepared_id.saturating_add(1);
                                let item = PreparedGeneration {
                                    id,
                                    revision: payload.revision,
                                    draft: payload.draft,
                                    request: payload.request.clone(),
                                    staged_media: payload.staged_media,
                                    quote: payload.quote.clone(),
                                    expires_at,
                                    deadline: Instant::now() + lifetime,
                                    draft_fingerprint: payload.draft_fingerprint,
                                    draft_fingerprint_candidates: payload.draft_fingerprint_candidates,
                                };
                                let _ = events.send(ServiceEvent::ReviewReady {
                                    op_id: outcome.op_id,
                                    prepared_id: id,
                                    revision: item.revision,
                                    provider_id: item.request.provider_id.clone(),
                                    request: item.request.clone(),
                                    quote: item.quote.clone(),
                                    expires_at,
                                    draft_fingerprint: item.draft_fingerprint.clone(),
                                });
                                prepared = Some(item);
                            } else {
                                emit_error(
                                    &events,
                                    outcome.op_id,
                                    Some(payload.request.provider_id),
                                    ServiceScope::Preparation,
                                    "Staged input media expires too soon to submit safely; Review again to upload a fresh copy".into(),
                                    true,
                                    None,
                                );
                            }
                        }
                        preparation_invalidated = false;
                    }
                    Err(error) => {
                        if preparation_invalidated {
                            let _ = events.send(ServiceEvent::PreparedInvalidated {
                                op_id: outcome.op_id,
                                prepared_id: None,
                                revision: current_revision.unwrap_or_default(),
                            });
                            preparation_invalidated = false;
                        } else {
                            emit_task_failure(&events, outcome.op_id, error, None);
                        }
                    }
                }
                if shutting_down && operations.is_empty() && preparation.is_none() {
                    let _ = events.send(ServiceEvent::ShutdownComplete);
                    break;
                }
            }
            Some(notice) = notice_rx.recv() => {
                match notice {
                    OperationNotice::Monitoring { task_id, key } => {
                        if submission_task == Some(task_id) {
                            submission_task = None;
                        }
                        if operations.contains_key(&task_id) {
                            monitors.insert(key, task_id);
                        }
                    }
                    OperationNotice::Finished { task_id } => {
                        if submission_task == Some(task_id) {
                            submission_task = None;
                        }
                        monitors.retain(|_, value| *value != task_id);
                        if let Some(operation) = operations.remove(&task_id) {
                            let _ = operation.task.await;
                        }
                    }
                }
                if shutting_down && operations.is_empty() && preparation.is_none() {
                    let _ = events.send(ServiceEvent::ShutdownComplete);
                    break;
                }
            }
            command = commands.recv(), if !shutting_down => {
                let Some(command) = command else {
                    shutting_down = true;
                    if let Some(operation) = &preparation {
                        preparation_invalidated = true;
                        operation.cancel.store(true, Ordering::Release);
                    }
                    for operation in operations.values() {
                        operation.cancel.store(true, Ordering::Release);
                    }
                    for task in catalog_tasks.values() {
                        task.abort();
                    }
                    catalog_tasks.clear();
                    if operations.is_empty() && preparation.is_none() {
                        let _ = events.send(ServiceEvent::ShutdownComplete);
                        break;
                    }
                    continue;
                };
                match command {
                    ServiceCommand::ConnectApiKey { op_id, provider_id, key, persist_on_success } => {
                        let key = SecretString::from(key.expose_secret().trim().to_owned());
                        match make_provider(&provider_id, &key, &config, executor.clone()) {
                            Ok(provider) => match provider.validate_credentials().await {
                                Ok(info) => {
                                    let Some(session) = sessions.get_mut(&provider_id) else {
                                        emit_unknown_provider(&events, op_id, &provider_id, ServiceScope::Credential);
                                        continue;
                                    };
                                    let credential_status = if persist_on_success {
                                        let store = Arc::clone(&session.store);
                                        let stored_key = key.clone();
                                        tokio::task::spawn_blocking(move || {
                                            let mut store = store.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                                            let _ = store.set(stored_key);
                                            store.status()
                                        }).await.unwrap_or_else(|_| memory_status())
                                    } else {
                                        memory_status()
                                    };
                                    session.key = Some(key);
                                    let _ = events.send(ServiceEvent::ApiKeyConnected {
                                        op_id,
                                        provider_id: provider_id.clone(),
                                        info,
                                        credential_status,
                                    });
                                    invalidate_prepared_for_provider(
                                        &mut prepared,
                                        &provider_id,
                                        op_id,
                                        &events,
                                    );
                                    invalidate_active_preparation(
                                        preparation.as_ref(),
                                        &mut preparation_invalidated,
                                    );
                                }
                                Err(error) => emit_provider_error(&events, op_id, ServiceScope::Credential, error, None),
                            },
                            Err(error) => emit_provider_error(&events, op_id, ServiceScope::Credential, error, None),
                        }
                    }
                    ServiceCommand::ForgetApiKey { op_id, provider_id } => {
                        let Some(session) = sessions.get_mut(&provider_id) else {
                            emit_unknown_provider(&events, op_id, &provider_id, ServiceScope::Credential);
                            continue;
                        };
                        session.key = None;
                        let store = Arc::clone(&session.store);
                        let credential_status = tokio::task::spawn_blocking(move || {
                            let mut store = store.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                            store.delete();
                            store.status()
                        }).await.unwrap_or_else(|_| memory_status());
                        let _ = events.send(ServiceEvent::ApiKeyForgotten {
                            op_id,
                            provider_id: provider_id.clone(),
                            credential_status,
                        });
                        invalidate_prepared_for_provider(
                            &mut prepared,
                            &provider_id,
                            op_id,
                            &events,
                        );
                        invalidate_active_preparation(
                            preparation.as_ref(),
                            &mut preparation_invalidated,
                        );
                    }
                    ServiceCommand::RefreshCatalog { op_id, provider_id } => {
                        let key = sessions.get(&provider_id).and_then(|session| session.key.clone());
                        let task_id = next_task_id;
                        next_task_id = next_task_id.saturating_add(1);
                        let finished = catalog_finished_tx.clone();
                        let refresh_paths = paths.clone();
                        let refresh_config = config.clone();
                        let refresh_executor = executor.clone();
                        let refresh_events = events.clone();
                        let task = tokio::spawn(async move {
                            refresh_catalog(
                                op_id,
                                provider_id,
                                key,
                                refresh_paths,
                                refresh_config,
                                refresh_executor,
                                refresh_events,
                            ).await;
                            let _ = finished.send(task_id);
                        });
                        catalog_tasks.insert(task_id, task);
                    }
                    ServiceCommand::Quote { op_id, provider_id, request } => {
                        if video_request_contains_active_key(&request, &sessions) {
                            emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Quote,
                                GuiStateError::CredentialInDraft,
                            );
                            continue;
                        }
                        let Some(key) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else {
                            emit_missing_key(&events, op_id, &provider_id, ServiceScope::Quote, None);
                            continue;
                        };
                        match make_provider(&provider_id, &key, &config, executor.clone()) {
                            Ok(provider) => match provider.quote(&request).await {
                                Ok(quote) => { let _ = events.send(ServiceEvent::QuoteReady {
                                    op_id,
                                    provider_id,
                                    model_id: request.model,
                                    quote,
                                }); }
                                Err(error) => emit_provider_error(&events, op_id, ServiceScope::Quote, error, None),
                            },
                            Err(error) => emit_provider_error(&events, op_id, ServiceScope::Quote, error, None),
                        }
                    }
                    ServiceCommand::PrepareGeneration { op_id, draft, revision } => {
                        let provider_id = draft.provider_id.clone();
                        if preparation.is_some() || submission_task.is_some() {
                            emit_error(&events, op_id, Some(provider_id), ServiceScope::Preparation, "Another Review preparation or paid submission is in progress".into(), true, None);
                            continue;
                        }
                        if let Some(previous) = prepared.take() {
                            let _ = events.send(ServiceEvent::PreparedInvalidated {
                                op_id,
                                prepared_id: Some(previous.id),
                                revision,
                            });
                        }
                        current_revision = Some(revision);
                        preparation_invalidated = false;
                        if generation_draft_contains_active_key(&draft, &sessions) {
                            emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Preparation,
                                GuiStateError::CredentialInDraft,
                            );
                            continue;
                        }
                        if let Err(error) = draft.validate() {
                            emit_error(&events, op_id, Some(provider_id), ServiceScope::Preparation, error.to_string(), true, None);
                            continue;
                        }
                        let draft_fingerprints = match generation_draft_fingerprint_candidates(&draft) {
                            Ok(fingerprints) => fingerprints,
                            Err(error) => {
                                emit_gui_state_error(&events, op_id, ServiceScope::Preparation, error);
                                continue;
                            }
                        };
                        let draft_fingerprint = draft_fingerprints[0].clone();
                        let safety_state = gui_state.clone();
                        let safety_provider = provider_id.clone();
                        let safety_fingerprints = draft_fingerprints.clone();
                        match tokio::task::spawn_blocking(move || {
                            safety_state.uncertain_submission_for_fingerprints(
                                &safety_provider,
                                &safety_fingerprints,
                            )
                        }).await {
                            Ok(Ok(Some(record))) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionBlocked {
                                    op_id,
                                    record,
                                });
                                continue;
                            }
                            Ok(Ok(None)) => {}
                            Ok(Err(error)) => {
                                emit_gui_state_error(&events, op_id, ServiceScope::Preparation, error);
                                continue;
                            }
                            Err(_) => {
                                emit_error(&events, op_id, Some(provider_id), ServiceScope::Preparation, "Could not verify uncertain-submission safety state".into(), false, None);
                                continue;
                            }
                        }
                        let Some(key) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else {
                            emit_missing_key(&events, op_id, &provider_id, ServiceScope::Credential, None);
                            continue;
                        };
                        let provider = match make_provider(&provider_id, &key, &config, executor.clone()) {
                            Ok(provider) => provider,
                            Err(error) => {
                                emit_provider_error(&events, op_id, ServiceScope::Preparation, error, None);
                                continue;
                            }
                        };
                        let _ = events.send(ServiceEvent::PreparationStarted {
                            op_id,
                            provider_id: provider_id.clone(),
                            media_count: draft.media.len(),
                        });
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task_id = next_task_id;
                        next_task_id = next_task_id.saturating_add(1);
                        let task = tokio::spawn(run_prepare_generation(
                            op_id,
                            revision,
                            draft,
                            draft_fingerprint,
                            draft_fingerprints,
                            provider,
                            gui_state.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            preparation_tx.clone(),
                        ));
                        preparation = Some(ActiveOperation { task_id, op_id, cancel, task });
                    }
                    ServiceCommand::SubmitPrepared { op_id, prepared_id } => {
                        if submission_task.is_some() {
                            emit_error(&events, op_id, None, ServiceScope::Generation, "A paid submission is already in progress".into(), true, None);
                            continue;
                        }
                        let Some(current) = prepared.as_ref() else {
                            emit_error(&events, op_id, None, ServiceScope::Preparation, "Review is no longer valid; review the generation again".into(), true, None);
                            continue;
                        };
                        if current.id != prepared_id {
                            emit_error(&events, op_id, Some(current.request.provider_id.clone()), ServiceScope::Preparation, "This Review was replaced by a newer draft".into(), true, None);
                            continue;
                        }
                        if !staged_media_valid_for_submission(&current.staged_media, Utc::now()) {
                            let expired = prepared.take().expect("checked prepared Review");
                            let _ = events.send(ServiceEvent::PreparedInvalidated {
                                op_id,
                                prepared_id: Some(expired.id),
                                revision: expired.revision,
                            });
                            emit_error(&events, op_id, Some(expired.request.provider_id), ServiceScope::Preparation, "A staged input is too close to expiring; Review again before generating".into(), true, None);
                            continue;
                        }
                        if Instant::now() >= current.deadline {
                            let expired = prepared.take().expect("checked prepared Review");
                            let _ = events.send(ServiceEvent::PreparedInvalidated {
                                op_id,
                                prepared_id: Some(expired.id),
                                revision: expired.revision,
                            });
                            emit_error(&events, op_id, Some(expired.request.provider_id), ServiceScope::Preparation, "Review expired; refresh the quote before generating".into(), true, None);
                            continue;
                        }
                        if video_request_contains_active_key(&current.request, &sessions) {
                            let rejected = prepared.take().expect("checked prepared Review");
                            let _ = events.send(ServiceEvent::PreparedInvalidated {
                                op_id,
                                prepared_id: Some(rejected.id),
                                revision: rejected.revision,
                            });
                            emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Generation,
                                GuiStateError::CredentialInDraft,
                            );
                            continue;
                        }
                        let item = prepared.take().expect("checked prepared Review");
                        let provider_id = item.request.provider_id.clone();
                        let Some(key) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else {
                            emit_missing_key(&events, op_id, &provider_id, ServiceScope::Credential, None);
                            continue;
                        };
                        let provider = match make_provider(&provider_id, &key, &config, executor.clone()) {
                            Ok(provider) => provider,
                            Err(error) => {
                                emit_provider_error(&events, op_id, ServiceScope::Generation, error, None);
                                continue;
                            }
                        };
                        let safety_record = UncertainSubmissionRecord::new(
                            provider_id.clone(),
                            item.draft_fingerprint.clone(),
                            Utc::now(),
                        );
                        let safety_state = gui_state.clone();
                        let safety_candidate = safety_record.clone();
                        let safety_fingerprints = item.draft_fingerprint_candidates.clone();
                        match tokio::task::spawn_blocking(move || {
                            safety_state.begin_uncertain_submission_with_aliases(
                                &safety_candidate,
                                &safety_fingerprints,
                            )
                        }).await {
                            Ok(Ok(BeginUncertainSubmission::Inserted(record))) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionSaved {
                                    op_id,
                                    record,
                                });
                            }
                            Ok(Ok(BeginUncertainSubmission::Existing(record))) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionBlocked {
                                    op_id,
                                    record,
                                });
                                let _ = events.send(ServiceEvent::PreparedInvalidated {
                                    op_id,
                                    prepared_id: Some(item.id),
                                    revision: item.revision,
                                });
                                continue;
                            }
                            Ok(Err(error)) => {
                                emit_gui_state_error(&events, op_id, ServiceScope::Generation, error);
                                continue;
                            }
                            Err(_) => {
                                emit_error(&events, op_id, Some(provider_id), ServiceScope::Generation, "Could not durably save the pre-submit safety record; no paid request was sent".into(), false, None);
                                continue;
                            }
                        }
                        let associations = generation_media_for_prepared(&item);
                        let submit_before = staged_media_submit_before(&item.staged_media)
                            .map_or(item.expires_at, |media_deadline| {
                                media_deadline.min(item.expires_at)
                            });
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task_id = next_task_id;
                        next_task_id = next_task_id.saturating_add(1);
                        let task = tokio::spawn(run_generate(
                            task_id,
                            op_id,
                            provider_id.clone(),
                            item.request,
                            provider,
                            history.clone(),
                            gui_state.clone(),
                            safety_record,
                            associations,
                            Some(submit_before),
                            paths.clone(),
                            config.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            notice_tx.clone(),
                        ));
                        operations.insert(task_id, ActiveOperation { task_id, op_id, cancel, task });
                        submission_task = Some(task_id);
                    }
                    ServiceCommand::InvalidatePrepared { op_id, revision } => {
                        current_revision = Some(revision);
                        if let Some(operation) = &preparation {
                            preparation_invalidated = true;
                            operation.cancel.store(true, Ordering::Release);
                        }
                        let invalidated = prepared.take().map(|item| item.id);
                        let _ = events.send(ServiceEvent::PreparedInvalidated {
                            op_id,
                            prepared_id: invalidated,
                            revision,
                        });
                    }
                    ServiceCommand::SaveDraft { op_id, draft, editor_state, revision } => {
                        current_revision = Some(revision);
                        if let Some(operation) = &preparation {
                            preparation_invalidated = true;
                            operation.cancel.store(true, Ordering::Release);
                        }
                        if prepared.as_ref().is_some_and(|item| {
                            item.revision != revision || item.draft != draft
                        })
                            && let Some(previous) = prepared.take()
                        {
                            let _ = events.send(ServiceEvent::PreparedInvalidated {
                                op_id,
                                prepared_id: Some(previous.id),
                                revision,
                            });
                        }
                        let stored = stored_draft_from_generation(&draft, editor_state, revision);
                        if stored_draft_contains_active_key(&stored, &sessions) {
                            emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Draft,
                                GuiStateError::CredentialInDraft,
                            );
                            continue;
                        }
                        let state = gui_state.clone();
                        match tokio::task::spawn_blocking(move || state.save_draft(&stored)).await {
                            Ok(Ok(())) => { let _ = events.send(ServiceEvent::DraftSaved { op_id, revision }); }
                            Ok(Err(error)) => emit_gui_state_error(&events, op_id, ServiceScope::Draft, error),
                            Err(_) => emit_error(&events, op_id, None, ServiceScope::Draft, "Draft autosave task failed".into(), true, None),
                        }
                    }
                    ServiceCommand::LoadDraft { op_id } => {
                        let state = gui_state.clone();
                        match tokio::task::spawn_blocking(move || state.load_draft()).await {
                            Ok(Ok(Some(stored))) => match generation_from_stored_draft(&stored) {
                                Ok(draft) => {
                                    let editor_state = stored.editor_state.clone().unwrap_or_else(|| {
                                        legacy_editor_state(&draft)
                                    });
                                    current_revision = Some(stored.revision);
                                    let _ = events.send(ServiceEvent::DraftLoaded {
                                        op_id,
                                        draft: Some(draft),
                                        editor_state: Some(editor_state),
                                        revision: Some(stored.revision),
                                    });
                                }
                                Err(error) => emit_gui_state_error(&events, op_id, ServiceScope::Draft, error),
                            },
                            Ok(Ok(None)) => {
                                let _ = events.send(ServiceEvent::DraftLoaded {
                                    op_id,
                                    draft: None,
                                    editor_state: None,
                                    revision: None,
                                });
                            }
                            Ok(Err(error)) => emit_gui_state_error(&events, op_id, ServiceScope::Draft, error),
                            Err(_) => emit_error(&events, op_id, None, ServiceScope::Draft, "Draft load task failed".into(), true, None),
                        }
                    }
                    ServiceCommand::SaveUncertainSubmission { op_id, provider_id, draft_fingerprint } => {
                        let record = UncertainSubmissionRecord::new(
                            provider_id.clone(),
                            draft_fingerprint,
                            Utc::now(),
                        );
                        let state = gui_state.clone();
                        let candidate = record.clone();
                        match tokio::task::spawn_blocking(move || {
                            state.begin_uncertain_submission(&candidate)
                        }).await {
                            Ok(Ok(BeginUncertainSubmission::Inserted(saved)
                                | BeginUncertainSubmission::Existing(saved))) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionSaved {
                                    op_id,
                                    record: saved,
                                });
                            }
                            Ok(Err(error)) => emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Generation,
                                error,
                            ),
                            Err(_) => emit_error(&events, op_id, Some(provider_id), ServiceScope::Generation, "Could not save uncertain-submission safety state".into(), false, None),
                        }
                    }
                    ServiceCommand::LoadUncertainSubmissions { op_id } => {
                        let state = gui_state.clone();
                        match tokio::task::spawn_blocking(move || state.uncertain_submissions()).await {
                            Ok(Ok(records)) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionsLoaded {
                                    op_id,
                                    records,
                                });
                            }
                            Ok(Err(error)) => emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Generation,
                                error,
                            ),
                            Err(_) => emit_error(&events, op_id, None, ServiceScope::Generation, "Could not load uncertain-submission safety state".into(), false, None),
                        }
                    }
                    ServiceCommand::ClearUncertainSubmission { op_id, provider_id, draft_fingerprint } => {
                        if submission_task.is_some() {
                            emit_error(
                                &events,
                                op_id,
                                Some(provider_id),
                                ServiceScope::Generation,
                                "Cannot clear a submission safety barrier while a paid request is still in progress".into(),
                                false,
                                None,
                            );
                            continue;
                        }
                        let state = gui_state.clone();
                        let clear_provider = provider_id.clone();
                        let clear_fingerprint = draft_fingerprint.clone();
                        match tokio::task::spawn_blocking(move || {
                            state.clear_uncertain_submission(
                                &clear_provider,
                                &clear_fingerprint,
                            )
                        }).await {
                            Ok(Ok(removed)) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionCleared {
                                    op_id,
                                    provider_id,
                                    draft_fingerprint,
                                    removed,
                                });
                            }
                            Ok(Err(error)) => emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Generation,
                                error,
                            ),
                            Err(_) => emit_error(&events, op_id, Some(provider_id), ServiceScope::Generation, "Could not clear uncertain-submission safety state".into(), false, None),
                        }
                    }
                    ServiceCommand::SaveModelSettings { op_id, provider_id, model_id, settings_json } => {
                        let path = match paths.provider_model_settings(&provider_id) {
                            Ok(path) => path,
                            Err(error) => {
                                emit_error(&events, op_id, Some(provider_id), ServiceScope::Settings, error.to_string(), false, None);
                                continue;
                            }
                        };
                        let saved_model = model_id.clone();
                        let saved_provider = provider_id.clone();
                        match tokio::task::spawn_blocking(move || save_model_settings(&path, &model_id, settings_json)).await {
                            Ok(Ok(())) => { let _ = events.send(ServiceEvent::SettingsSaved {
                                op_id,
                                provider_id: saved_provider,
                                model_id: saved_model,
                            }); }
                            Ok(Err(error)) => emit_error(&events, op_id, Some(provider_id), ServiceScope::Settings, error.to_string(), true, None),
                            Err(_) => emit_error(&events, op_id, Some(provider_id), ServiceScope::Settings, "Model settings task failed".into(), true, None),
                        }
                    }
                    ServiceCommand::SaveDefaultProvider { op_id, provider_id } => {
                        if !sessions.contains_key(&provider_id) {
                            emit_unknown_provider(&events, op_id, &provider_id, ServiceScope::Settings);
                            continue;
                        }
                        let path = paths.app_settings();
                        let saved_provider = provider_id.clone();
                        let settings = AppSettings {
                            default_provider: provider_id.clone(),
                            ..AppSettings::default()
                        };
                        match tokio::task::spawn_blocking(move || save_app_settings(&path, &settings)).await {
                            Ok(Ok(())) => { let _ = events.send(ServiceEvent::DefaultProviderSaved {
                                op_id,
                                provider_id: saved_provider,
                            }); }
                            Ok(Err(error)) => emit_error(&events, op_id, Some(provider_id), ServiceScope::Settings, error.to_string(), true, None),
                            Err(_) => emit_error(&events, op_id, Some(provider_id), ServiceScope::Settings, "App settings task failed".into(), true, None),
                        }
                    }
                    ServiceCommand::LoadHistory { op_id, limit } => {
                        let history = history.clone();
                        match tokio::task::spawn_blocking(move || history.list_generations(limit)).await {
                            Ok(Ok(records)) => { let _ = events.send(ServiceEvent::HistoryLoaded { op_id, records }); }
                            Ok(Err(error)) => emit_error(&events, op_id, None, ServiceScope::History, error.to_string(), true, None),
                            Err(_) => emit_error(&events, op_id, None, ServiceScope::History, "History task failed".into(), true, None),
                        }
                    }
                    ServiceCommand::OpenVideo { op_id, path } => open_video(op_id, path, &events).await,
                    ServiceCommand::CancelCurrent { op_id } => {
                        if let Some(operation) = &preparation {
                            preparation_invalidated = true;
                            operation.cancel.store(true, Ordering::Release);
                        } else if let Some(task_id) = submission_task
                            && let Some(operation) = operations.get(&task_id)
                        {
                            // Cancellation is intentionally observed only after
                            // the provider returns an accepted remote id.
                            operation.cancel.store(true, Ordering::Release);
                        } else if let Some(operation) = operations.values().max_by_key(|operation| operation.task_id) {
                            operation.cancel.store(true, Ordering::Release);
                        } else {
                            let _ = events.send(ServiceEvent::Cancelled {
                                op_id,
                                provider_id: None,
                                job_id: None,
                                remote_continues: false,
                            });
                        }
                    }
                    ServiceCommand::PauseMonitor { op_id, key } => {
                        let Some(task_id) = monitors.get(&key).copied() else {
                            emit_error(&events, op_id, Some(key.provider_id.clone()), ServiceScope::Generation, "That job is not currently being monitored".into(), true, Some(key.remote_job_id));
                            continue;
                        };
                        if let Some(operation) = operations.get(&task_id) {
                            operation.cancel.store(true, Ordering::Release);
                        }
                        let saved_state = gui_state.clone();
                        let saved_key = key.clone();
                        let _ = tokio::task::spawn_blocking(move || saved_state.set_monitoring_paused(&saved_key, true)).await;
                        let _ = events.send(ServiceEvent::MonitorPaused { op_id, key, remote_continues: true });
                    }
                    ServiceCommand::PauseAll { op_id } => {
                        let keys: Vec<_> = monitors.keys().cloned().collect();
                        for task_id in monitors.values() {
                            if let Some(operation) = operations.get(task_id) {
                                operation.cancel.store(true, Ordering::Release);
                            }
                        }
                        let saved_state = gui_state.clone();
                        let saved_keys = keys.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            for key in saved_keys {
                                let _ = saved_state.set_monitoring_paused(&key, true);
                            }
                        }).await;
                        let _ = events.send(ServiceEvent::MonitorsPaused { op_id, count: keys.len(), remote_continue: true });
                    }
                    ServiceCommand::Generate { op_id, provider_id, mut request } => {
                        if submission_task.is_some() || preparation.is_some() {
                            emit_error(&events, op_id, Some(provider_id), ServiceScope::Generation, "A Review preparation or paid submission is already in progress".into(), true, None);
                            continue;
                        }
                        request.provider_id = provider_id.clone();
                        if video_request_contains_active_key(&request, &sessions) {
                            emit_gui_state_error(
                                &events,
                                op_id,
                                ServiceScope::Generation,
                                GuiStateError::CredentialInDraft,
                            );
                            continue;
                        }
                        let Some(key) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else {
                            emit_missing_key(&events, op_id, &provider_id, ServiceScope::Credential, None);
                            continue;
                        };
                        let provider = match make_provider(&provider_id, &key, &config, executor.clone()) {
                            Ok(provider) => provider,
                            Err(error) => {
                                emit_provider_error(&events, op_id, ServiceScope::Generation, error, None);
                                continue;
                            }
                        };
                        // The legacy direct command remains available for
                        // integrations, but media-bearing requests must pass
                        // the same current catalog/schema checks as Review
                        // before a durable paid-submission safety marker is
                        // written.
                        if (!request.frame_images.is_empty()
                            || !request.input_references.is_empty())
                            && let Err(error) = provider.validate_request(&request).await
                        {
                            emit_provider_error(
                                &events,
                                op_id,
                                ServiceScope::Generation,
                                error,
                                None,
                            );
                            continue;
                        }
                        let draft_fingerprints = match video_request_fingerprint_candidates(&request) {
                            Ok(fingerprints) => fingerprints,
                            Err(error) => {
                                emit_gui_state_error(&events, op_id, ServiceScope::Generation, error);
                                continue;
                            }
                        };
                        let draft_fingerprint = draft_fingerprints[0].clone();
                        let safety_record = UncertainSubmissionRecord::new(
                            provider_id.clone(),
                            draft_fingerprint,
                            Utc::now(),
                        );
                        let safety_state = gui_state.clone();
                        let safety_candidate = safety_record.clone();
                        let safety_fingerprints = draft_fingerprints.clone();
                        match tokio::task::spawn_blocking(move || {
                            safety_state.begin_uncertain_submission_with_aliases(
                                &safety_candidate,
                                &safety_fingerprints,
                            )
                        }).await {
                            Ok(Ok(BeginUncertainSubmission::Inserted(record))) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionSaved {
                                    op_id,
                                    record,
                                });
                            }
                            Ok(Ok(BeginUncertainSubmission::Existing(record))) => {
                                let _ = events.send(ServiceEvent::UncertainSubmissionBlocked {
                                    op_id,
                                    record,
                                });
                                continue;
                            }
                            Ok(Err(error)) => {
                                emit_gui_state_error(&events, op_id, ServiceScope::Generation, error);
                                continue;
                            }
                            Err(_) => {
                                emit_error(&events, op_id, Some(provider_id), ServiceScope::Generation, "Could not durably save the pre-submit safety record; no paid request was sent".into(), false, None);
                                continue;
                            }
                        }
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task_id = next_task_id;
                        next_task_id = next_task_id.saturating_add(1);
                        let task = tokio::spawn(run_generate(
                            task_id,
                            op_id,
                            provider_id.clone(),
                            request,
                            provider,
                            history.clone(),
                            gui_state.clone(),
                            safety_record,
                            Vec::new(),
                            None,
                            paths.clone(),
                            config.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            notice_tx.clone(),
                        ));
                        operations.insert(task_id, ActiveOperation { task_id, op_id, cancel, task });
                        submission_task = Some(task_id);
                    }
                    ServiceCommand::Resume { op_id, key } => {
                        if monitors.contains_key(&key) {
                            emit_error(&events, op_id, Some(key.provider_id.clone()), ServiceScope::Generation, "That job is already being monitored".into(), true, Some(key.remote_job_id.clone()));
                            continue;
                        }
                        let provider_id = key.provider_id.clone();
                        let Some(secret) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else {
                            emit_missing_key(&events, op_id, &provider_id, ServiceScope::Credential, Some(key.remote_job_id.clone()));
                            continue;
                        };
                        let provider = match make_provider(&provider_id, &secret, &config, executor.clone()) {
                            Ok(provider) => provider,
                            Err(error) => {
                                emit_provider_error(&events, op_id, ServiceScope::Generation, error, Some(key.remote_job_id.clone()));
                                continue;
                            }
                        };
                        let monitor_key = key.clone();
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task_id = next_task_id;
                        next_task_id = next_task_id.saturating_add(1);
                        let task = tokio::spawn(run_existing(
                            task_id,
                            op_id,
                            provider_id.clone(),
                            ExistingJob::Resume(key),
                            provider,
                            history.clone(),
                            gui_state.clone(),
                            paths.clone(),
                            config.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            notice_tx.clone(),
                        ));
                        operations.insert(task_id, ActiveOperation { task_id, op_id, cancel, task });
                        monitors.insert(monitor_key, task_id);
                    }
                    ServiceCommand::ResumeAll { op_id } => {
                        let saved_state = gui_state.clone();
                        let jobs = match tokio::task::spawn_blocking(move || saved_state.resumable_jobs()).await {
                            Ok(Ok(jobs)) => jobs,
                            Ok(Err(error)) => {
                                emit_gui_state_error(&events, op_id, ServiceScope::Generation, error);
                                continue;
                            }
                            Err(_) => {
                                emit_error(&events, op_id, None, ServiceScope::Generation, "Resume All state lookup failed".into(), true, None);
                                continue;
                            }
                        };
                        let total = jobs.len();
                        let mut started = 0usize;
                        for job in jobs {
                            if monitors.contains_key(&job.key) {
                                continue;
                            }
                            let provider_id = job.key.provider_id.clone();
                            let Some(secret) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else { continue; };
                            let Ok(provider) = make_provider(&provider_id, &secret, &config, executor.clone()) else { continue; };
                            let task_id = next_task_id;
                            next_task_id = next_task_id.saturating_add(1);
                            let cancel = Arc::new(AtomicBool::new(false));
                            let task_key = job.key.clone();
                            let task = tokio::spawn(run_existing(
                                task_id,
                                op_id,
                                provider_id,
                                ExistingJob::Resume(job.key.clone()),
                                provider,
                                history.clone(),
                                gui_state.clone(),
                                paths.clone(),
                                config.clone(),
                                Arc::clone(&cancel),
                                events.clone(),
                                notice_tx.clone(),
                            ));
                            operations.insert(task_id, ActiveOperation { task_id, op_id, cancel, task });
                            monitors.insert(task_key.clone(), task_id);
                            let state = gui_state.clone();
                            let _ = tokio::task::spawn_blocking(move || state.set_monitoring_paused(&task_key, false)).await;
                            started += 1;
                        }
                        let _ = events.send(ServiceEvent::ResumeAllStarted { op_id, started, skipped: total.saturating_sub(started) });
                    }
                    ServiceCommand::Import { op_id, provider_id, locator } => {
                        if locator.provider_id() != provider_id {
                            emit_error(&events, op_id, Some(provider_id), ServiceScope::Import, "Import locator belongs to a different provider".into(), false, Some(locator.remote_job_id().into()));
                            continue;
                        }
                        let Some(secret) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else {
                            emit_missing_key(&events, op_id, &provider_id, ServiceScope::Credential, Some(locator.remote_job_id().into()));
                            continue;
                        };
                        let provider = match make_provider(&provider_id, &secret, &config, executor.clone()) {
                            Ok(provider) => provider,
                            Err(error) => {
                                emit_provider_error(&events, op_id, ServiceScope::Import, error, Some(locator.remote_job_id().into()));
                                continue;
                            }
                        };
                        let monitor_key = ProviderJobKey {
                            provider_id: provider_id.clone(),
                            remote_job_id: locator.remote_job_id().to_owned(),
                        };
                        if monitors.contains_key(&monitor_key) {
                            emit_error(&events, op_id, Some(provider_id), ServiceScope::Import, "That job is already being monitored".into(), true, Some(locator.remote_job_id().into()));
                            continue;
                        }
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task_id = next_task_id;
                        next_task_id = next_task_id.saturating_add(1);
                        let task = tokio::spawn(run_existing(
                            task_id,
                            op_id,
                            provider_id.clone(),
                            ExistingJob::Import(locator),
                            provider,
                            history.clone(),
                            gui_state.clone(),
                            paths.clone(),
                            config.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            notice_tx.clone(),
                        ));
                        operations.insert(task_id, ActiveOperation { task_id, op_id, cancel, task });
                        monitors.insert(monitor_key, task_id);
                    }
                    ServiceCommand::Shutdown => {
                        if submission_task.is_some() {
                            let _ = events.send(ServiceEvent::ShutdownBlocked {
                                reason: "Waiting for the provider to return the paid submission's remote job id".into(),
                            });
                            continue;
                        }
                        shutting_down = true;
                        prepared = None;
                        if let Some(operation) = &preparation {
                            preparation_invalidated = true;
                            operation.cancel.store(true, Ordering::Release);
                        }
                        for operation in operations.values() {
                            operation.cancel.store(true, Ordering::Release);
                        }
                        for task in catalog_tasks.values() {
                            task.abort();
                        }
                        catalog_tasks.clear();
                        let keys: Vec<_> = monitors.keys().cloned().collect();
                        let state = gui_state.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            for key in keys {
                                let _ = state.set_monitoring_paused(&key, true);
                            }
                        }).await;
                        if operations.is_empty() && preparation.is_none() {
                            let _ = events.send(ServiceEvent::ShutdownComplete);
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn connection(provider_id: &ProviderId, session: &ProviderSession) -> ProviderConnection {
    let credential_status = session
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .status();
    ProviderConnection {
        descriptor: descriptor(provider_id),
        connected: session.key.is_some(),
        credential_status,
    }
}

fn invalidate_prepared_for_provider(
    prepared: &mut Option<PreparedGeneration>,
    provider_id: &ProviderId,
    op_id: u64,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) {
    if !prepared
        .as_ref()
        .is_some_and(|item| &item.request.provider_id == provider_id)
    {
        return;
    }
    let invalidated = prepared.take().expect("checked prepared provider");
    let _ = events.send(ServiceEvent::PreparedInvalidated {
        op_id,
        prepared_id: Some(invalidated.id),
        revision: invalidated.revision,
    });
}

fn invalidate_active_preparation(
    preparation: Option<&ActiveOperation>,
    preparation_invalidated: &mut bool,
) {
    if let Some(operation) = preparation {
        *preparation_invalidated = true;
        operation.cancel.store(true, Ordering::Release);
    }
}

fn descriptor(provider_id: &ProviderId) -> ProviderDescriptor {
    match provider_id.as_str() {
        "fal" => ProviderDescriptor {
            id: ProviderId::fal(),
            display_name: "fal.ai".into(),
            website: "https://fal.ai".into(),
        },
        _ => ProviderDescriptor {
            id: ProviderId::openrouter(),
            display_name: "OpenRouter".into(),
            website: "https://openrouter.ai".into(),
        },
    }
}

fn make_provider(
    provider_id: &ProviderId,
    key: &SecretString,
    config: &ServiceConfig,
    executor: Option<Arc<dyn HttpExecutor>>,
) -> Result<Arc<dyn VideoProvider>, ProviderError> {
    match provider_id.as_str() {
        "openrouter" => {
            let client = match executor {
                Some(executor) => OpenRouterClient::with_executor(
                    key.clone(),
                    config.client_options.clone(),
                    executor,
                ),
                None => OpenRouterClient::with_options(key.clone(), config.client_options.clone()),
            }
            .map_err(|error| ProviderError {
                provider_id: provider_id.clone(),
                kind: ProviderErrorKind::Configuration,
                message: error.message,
                status_code: error.status_code,
                code: error.code,
                details: error.details,
                retry_after: error.retry_after,
            })?;
            Ok(Arc::new(OpenRouterProvider::new(client)))
        }
        "fal" => {
            let provider = match executor {
                Some(executor) => {
                    FalProvider::with_executor(key.clone(), FalOptions::default(), executor)
                }
                None => FalProvider::new(key.clone()),
            }?;
            Ok(Arc::new(provider))
        }
        _ => Err(ProviderError::new(
            provider_id.clone(),
            ProviderErrorKind::Configuration,
            format!("Unsupported video provider {provider_id}"),
        )),
    }
}

fn memory_status() -> CredentialStatus {
    CredentialStatus {
        backend: "memory".into(),
        available: false,
        persistent: false,
        message: "API key is kept in memory for this session only".into(),
    }
}

async fn refresh_catalog(
    op_id: u64,
    provider_id: ProviderId,
    key: Option<SecretString>,
    paths: AppPaths,
    config: ServiceConfig,
    executor: Option<Arc<dyn HttpExecutor>>,
    events: mpsc::UnboundedSender<ServiceEvent>,
) {
    let cache_path = match paths.provider_catalog_cache(&provider_id) {
        Ok(path) => path,
        Err(error) => {
            emit_error(
                &events,
                op_id,
                Some(provider_id),
                ServiceScope::Catalog,
                error.to_string(),
                false,
                None,
            );
            return;
        }
    };
    let settings_path = match paths.provider_model_settings(&provider_id) {
        Ok(path) => path,
        Err(error) => {
            emit_error(
                &events,
                op_id,
                Some(provider_id),
                ServiceScope::Settings,
                error.to_string(),
                false,
                None,
            );
            return;
        }
    };
    let remembered_settings = tokio::task::spawn_blocking(move || {
        load_model_settings(&settings_path).unwrap_or_default()
    })
    .await
    .unwrap_or_default();

    // Disk wins the startup race: a valid provider-scoped cache is useful
    // immediately even when the network is offline or slow.
    let load_path = cache_path.clone();
    let cached = tokio::task::spawn_blocking(move || VideoCatalog::load(load_path)).await;
    let mut emitted_cached = false;
    if let Ok(Ok(mut catalog)) = cached
        && catalog.provider_id == provider_id
    {
        catalog.stale = true;
        emitted_cached = true;
        let _ = events.send(ServiceEvent::CatalogLoaded {
            op_id,
            provider_id: provider_id.clone(),
            catalog,
            remembered_settings: remembered_settings.clone(),
        });
    }

    // Both catalog APIs are public. A placeholder only satisfies adapter
    // construction and is never attached to catalog HTTP requests.
    let catalog_only_key = SecretString::from("catalog-only-placeholder".to_owned());
    let provider = match make_provider(
        &provider_id,
        key.as_ref().unwrap_or(&catalog_only_key),
        &config,
        executor,
    ) {
        Ok(provider) => provider,
        Err(error) => {
            if !emitted_cached {
                emit_provider_error(&events, op_id, ServiceScope::Catalog, error, None);
            }
            return;
        }
    };
    match provider.load_catalog().await {
        Ok(mut catalog) => {
            catalog.stale = false;
            let cached = catalog.clone();
            let save_path = cache_path;
            let _ = tokio::task::spawn_blocking(move || cached.save(save_path)).await;
            let _ = events.send(ServiceEvent::CatalogLoaded {
                op_id,
                provider_id,
                catalog,
                remembered_settings,
            });
        }
        Err(error) if !emitted_cached => {
            emit_provider_error(&events, op_id, ServiceScope::Catalog, error, None)
        }
        Err(_) => {}
    }
}

async fn open_video(op_id: u64, path: PathBuf, events: &mpsc::UnboundedSender<ServiceEvent>) {
    let resolved = match tokio::fs::canonicalize(&path).await {
        Ok(path) if path.is_file() => path,
        _ => {
            emit_error(
                events,
                op_id,
                None,
                ServiceScope::OpenVideo,
                format!("Video file no longer exists: {}", path.display()),
                true,
                None,
            );
            return;
        }
    };
    match std::process::Command::new("xdg-open")
        .arg(&resolved)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {
            let _ = events.send(ServiceEvent::VideoOpened {
                op_id,
                path: resolved,
            });
        }
        Err(error) => emit_error(
            events,
            op_id,
            None,
            ServiceScope::OpenVideo,
            format!("Could not start the default video player: {error}"),
            true,
            None,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_prepare_generation(
    op_id: u64,
    revision: u64,
    draft: GenerationDraft,
    draft_fingerprint: String,
    draft_fingerprint_candidates: Vec<String>,
    provider: Arc<dyn VideoProvider>,
    gui_state: GuiStateStore,
    cancel: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    completed: mpsc::UnboundedSender<PreparationOutcome>,
) {
    let result = prepare_generation_inner(
        op_id,
        revision,
        draft,
        draft_fingerprint,
        draft_fingerprint_candidates,
        provider,
        gui_state,
        cancel,
        &events,
    )
    .await;
    let _ = completed.send(PreparationOutcome { op_id, result });
}

#[allow(clippy::too_many_arguments)]
async fn prepare_generation_inner(
    op_id: u64,
    revision: u64,
    draft: GenerationDraft,
    draft_fingerprint: String,
    draft_fingerprint_candidates: Vec<String>,
    provider: Arc<dyn VideoProvider>,
    gui_state: GuiStateStore,
    cancel: Arc<AtomicBool>,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) -> Result<PreparedPayload, TaskFailure> {
    let provider_id = draft.provider_id.clone();
    draft.validate().map_err(|error| TaskFailure {
        provider_id: provider_id.clone(),
        scope: ServiceScope::Preparation,
        message: error.to_string(),
        recoverable: true,
        job_id: None,
        kind: TaskFailureKind::Ordinary,
    })?;
    provider
        .validate_draft(&draft)
        .await
        .map_err(|error| TaskFailure::provider(ServiceScope::Preparation, error, None))?;
    let mut staged_media = Vec::with_capacity(draft.media.len());
    for (media_index, media) in draft.media.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Err(preparation_cancelled(provider_id));
        }
        let local_path = media.source.local_path().map(Path::to_path_buf);
        let cached = if let Some(path) = &local_path {
            let lookup = gui_state.clone();
            let lookup_provider = provider_id.clone();
            let lookup_path = path.clone();
            match tokio::task::spawn_blocking(move || {
                lookup.usable_upload_receipt_for_path(&lookup_provider, &lookup_path, Utc::now())
            })
            .await
            {
                Ok(Ok(Some(receipt))) => stored_receipt_to_domain(&receipt)
                    .ok()
                    .filter(|receipt| upload_receipt_covers_review(receipt, Utc::now())),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    emit_gui_state_error(events, op_id, ServiceScope::Preparation, error);
                    None
                }
                Err(_) => None,
            }
        } else {
            None
        };
        if let Some(path) = &local_path {
            let _ = events.send(ServiceEvent::MediaUploadStarted {
                op_id,
                provider_id: provider_id.clone(),
                media_index,
                path: path.clone(),
            });
        }
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<UploadProgress>();
        let staged = {
            let stage = provider.stage_media(media, cached.as_ref(), Some(progress_tx));
            tokio::pin!(stage);
            loop {
                tokio::select! {
                    result = &mut stage => {
                        break result.map_err(|error| {
                            TaskFailure::provider(ServiceScope::Preparation, error, None)
                        })?;
                    }
                    progress = progress_rx.recv() => {
                        if let Some(progress) = progress {
                            let _ = events.send(ServiceEvent::MediaUploadProgress {
                                op_id,
                                provider_id: provider_id.clone(),
                                media_index,
                                sent: progress.sent,
                                total: progress.total,
                            });
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if cancel.load(Ordering::Acquire) {
                            return Err(preparation_cancelled(provider_id));
                        }
                    }
                }
            }
        };
        let reused = cached.as_ref().is_some_and(|cached| {
            staged.receipt.as_ref().is_some_and(|receipt| {
                receipt.sha256 == cached.sha256 && receipt.public_url == cached.public_url
            })
        });
        if let (Some(path), Some(receipt)) = (&local_path, &staged.receipt) {
            let stored = domain_receipt_to_stored(receipt, path.clone());
            let save = gui_state.clone();
            match tokio::task::spawn_blocking(move || save.save_upload_receipt(&stored)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    emit_gui_state_error(events, op_id, ServiceScope::Preparation, error)
                }
                Err(_) => emit_error(
                    events,
                    op_id,
                    Some(provider_id.clone()),
                    ServiceScope::Preparation,
                    "Could not cache the provider upload receipt".into(),
                    true,
                    None,
                ),
            }
        }
        let _ = events.send(ServiceEvent::MediaUploadCompleted {
            op_id,
            provider_id: provider_id.clone(),
            media_index,
            public_url: staged.public_url.clone(),
            reused,
            expires_at: staged.receipt.as_ref().map(|receipt| receipt.expires_at),
        });
        staged_media.push(staged);
    }
    provider
        .validate_staged_media_constraints(&draft, &staged_media)
        .map_err(|error| TaskFailure::provider(ServiceScope::Preparation, error, None))?;
    if cancel.load(Ordering::Acquire) {
        return Err(preparation_cancelled(provider_id));
    }
    let request = draft
        .to_video_request(&staged_media)
        .map_err(|error| TaskFailure {
            provider_id: provider_id.clone(),
            scope: ServiceScope::Preparation,
            message: error.to_string(),
            recoverable: true,
            job_id: None,
            kind: TaskFailureKind::Ordinary,
        })?;
    // Quote last so uploads and validation cannot consume most of its five
    // minute freshness window.
    let quote = provider
        .quote(&request)
        .await
        .map_err(|error| TaskFailure::provider(ServiceScope::Quote, error, None))?;
    Ok(PreparedPayload {
        revision,
        draft,
        request,
        staged_media,
        quote,
        draft_fingerprint,
        draft_fingerprint_candidates,
    })
}

fn preparation_cancelled(provider_id: ProviderId) -> TaskFailure {
    TaskFailure {
        provider_id,
        scope: ServiceScope::Preparation,
        message: "Review preparation was cancelled".into(),
        recoverable: true,
        job_id: None,
        kind: TaskFailureKind::Ordinary,
    }
}

fn generation_media_for_prepared(item: &PreparedGeneration) -> Vec<PreparedMediaAssociation> {
    item.draft
        .media
        .iter()
        .zip(&item.staged_media)
        .enumerate()
        .map(|(position, (draft, staged))| PreparedMediaAssociation {
            position,
            draft_media_id: format!("media-{position}"),
            role: media_role_name(draft.role).into(),
            source: stored_media_source(&draft.source),
            resolved_url: staged.public_url.clone(),
        })
        .collect()
}

fn stored_draft_from_generation(
    draft: &GenerationDraft,
    editor_state: DraftEditorState,
    revision: u64,
) -> StoredDraft {
    let settings = serde_json::json!({
        "duration": draft.duration,
        "resolution": draft.resolution,
        "aspect_ratio": draft.aspect_ratio,
        "size": draft.size,
        "generate_audio": draft.generate_audio,
        "seed": draft.seed,
        "adapter_options": draft.adapter_options,
    });
    let media = draft
        .media
        .iter()
        .enumerate()
        .map(|(index, media)| StoredDraftMedia {
            id: format!("media-{index}"),
            role: media_role_name(media.role).into(),
            source: stored_media_source(&media.source),
        })
        .collect();
    StoredDraft {
        revision,
        provider_id: draft.provider_id.clone(),
        model_id: draft.model.clone(),
        prompt: draft.prompt.clone(),
        settings,
        editor_state: Some(editor_state),
        media,
    }
}

fn legacy_editor_state(draft: &GenerationDraft) -> DraftEditorState {
    DraftEditorState {
        seed_text: draft
            .seed
            .map(|value| value.to_string())
            .unwrap_or_default(),
        advanced_json_text: draft
            .adapter_options
            .as_ref()
            .and_then(|value| serde_json::to_string_pretty(value).ok())
            .unwrap_or_else(|| "{}".into()),
        schema_text: BTreeMap::new(),
    }
}

fn stored_draft_contains_active_key(
    draft: &StoredDraft,
    sessions: &BTreeMap<ProviderId, ProviderSession>,
) -> bool {
    contains_active_session_key(sessions, |secret| {
        stored_draft_contains_secret(draft, secret)
    })
}

fn generation_draft_contains_active_key(
    draft: &GenerationDraft,
    sessions: &BTreeMap<ProviderId, ProviderSession>,
) -> bool {
    contains_active_session_key(sessions, |secret| {
        generation_draft_contains_secret(draft, secret)
    })
}

fn video_request_contains_active_key(
    request: &VideoRequest,
    sessions: &BTreeMap<ProviderId, ProviderSession>,
) -> bool {
    contains_active_session_key(sessions, |secret| {
        video_request_contains_secret(request, secret)
    })
}

fn contains_active_session_key(
    sessions: &BTreeMap<ProviderId, ProviderSession>,
    mut contains: impl FnMut(&str) -> bool,
) -> bool {
    sessions.values().any(|session| {
        session.key.as_ref().is_some_and(|key| {
            let secret = key.expose_secret();
            !secret.is_empty() && contains(secret)
        })
    })
}

fn stored_draft_contains_secret(draft: &StoredDraft, secret: &str) -> bool {
    draft.provider_id.as_str().contains(secret)
        || draft.model_id.contains(secret)
        || draft.prompt.contains(secret)
        || json_contains_secret(&draft.settings, secret)
        || draft
            .editor_state
            .as_ref()
            .is_some_and(|editor| editor.contains_secret(secret))
        || draft.media.iter().any(|media| {
            media.id.contains(secret)
                || media.role.contains(secret)
                || match &media.source {
                    StoredMediaSource::LocalFile(path) => path.to_string_lossy().contains(secret),
                    StoredMediaSource::RemoteUrl(url) => url.contains(secret),
                }
        })
}

fn generation_draft_contains_secret(draft: &GenerationDraft, secret: &str) -> bool {
    draft.provider_id.as_str().contains(secret)
        || draft.model.contains(secret)
        || draft.prompt.contains(secret)
        || draft
            .resolution
            .as_ref()
            .is_some_and(|value| value.contains(secret))
        || draft
            .aspect_ratio
            .as_ref()
            .is_some_and(|value| value.contains(secret))
        || draft
            .size
            .as_ref()
            .is_some_and(|value| value.contains(secret))
        || draft
            .adapter_options
            .as_ref()
            .is_some_and(|value| json_contains_secret(value, secret))
        || draft.media.iter().any(|media| match &media.source {
            MediaSource::LocalFile { path } => path.to_string_lossy().contains(secret),
            MediaSource::RemoteUrl { url } => url.contains(secret),
        })
}

fn video_request_contains_secret(request: &VideoRequest, secret: &str) -> bool {
    request.provider_id.as_str().contains(secret)
        || request.model.contains(secret)
        || request.prompt.contains(secret)
        || request
            .resolution
            .as_ref()
            .is_some_and(|value| value.contains(secret))
        || request
            .aspect_ratio
            .as_ref()
            .is_some_and(|value| value.contains(secret))
        || request
            .size
            .as_ref()
            .is_some_and(|value| value.contains(secret))
        || request
            .adapter_options
            .as_ref()
            .is_some_and(|value| json_contains_secret(value, secret))
        || request
            .frame_images
            .iter()
            .any(|image| image.url.contains(secret))
        || request
            .input_references
            .iter()
            .any(|reference| reference.url.contains(secret))
}

fn json_contains_secret(value: &Value, secret: &str) -> bool {
    match value {
        Value::String(value) => value.contains(secret),
        Value::Array(values) => values
            .iter()
            .any(|value| json_contains_secret(value, secret)),
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| key.contains(secret) || json_contains_secret(value, secret)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn generation_from_stored_draft(stored: &StoredDraft) -> Result<GenerationDraft, GuiStateError> {
    let mut draft = GenerationDraft::new(
        stored.provider_id.clone(),
        stored.model_id.clone(),
        stored.prompt.clone(),
    )
    .map_err(|error| GuiStateError::InvalidValue(error.to_string()))?;
    draft.duration = optional_u32(&stored.settings, "duration")?;
    draft.resolution = optional_string(&stored.settings, "resolution")?;
    draft.aspect_ratio = optional_string(&stored.settings, "aspect_ratio")?;
    draft.size = optional_string(&stored.settings, "size")?;
    draft.generate_audio = stored
        .settings
        .get("generate_audio")
        .and_then(Value::as_bool);
    draft.seed = stored.settings.get("seed").and_then(Value::as_i64);
    draft.adapter_options = stored
        .settings
        .get("adapter_options")
        .filter(|value| !value.is_null())
        .cloned();
    draft.media = stored
        .media
        .iter()
        .map(|media| {
            let role = parse_media_role(&media.role)?;
            match &media.source {
                StoredMediaSource::LocalFile(path) => Ok(DraftMedia::local(path.clone(), role)),
                StoredMediaSource::RemoteUrl(url) => DraftMedia::remote(url.clone(), role)
                    .map_err(|error| GuiStateError::InvalidValue(error.to_string())),
            }
        })
        .collect::<Result<Vec<_>, GuiStateError>>()?;
    Ok(draft)
}

fn video_request_fingerprint_candidates(
    request: &VideoRequest,
) -> Result<Vec<String>, GuiStateError> {
    let mut media = request
        .frame_images
        .iter()
        .map(|frame| DraftMedia {
            source: MediaSource::RemoteUrl {
                url: frame.url.clone(),
            },
            role: match frame.frame_type {
                crate::domain::FrameType::FirstFrame => MediaRole::StartFrame,
                crate::domain::FrameType::LastFrame => MediaRole::EndFrame,
            },
        })
        .collect::<Vec<_>>();
    media.extend(request.input_references.iter().map(|reference| DraftMedia {
        source: MediaSource::RemoteUrl {
            url: reference.url.clone(),
        },
        role: match reference.kind {
            InputReferenceKind::Image => MediaRole::Reference,
            InputReferenceKind::Video => MediaRole::VideoInput,
            InputReferenceKind::Audio => MediaRole::AudioInput,
        },
    }));
    generation_draft_fingerprint_candidates(&GenerationDraft {
        provider_id: request.provider_id.clone(),
        model: request.model.clone(),
        prompt: request.prompt.clone(),
        duration: request.duration,
        resolution: request.resolution.clone(),
        aspect_ratio: request.aspect_ratio.clone(),
        size: request.size.clone(),
        generate_audio: request.generate_audio,
        seed: request.seed,
        media,
        adapter_options: request.adapter_options.clone(),
    })
}

fn optional_u32(value: &Value, key: &str) -> Result<Option<u32>, GuiStateError> {
    let Some(number) = value.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let number = number
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| GuiStateError::InvalidValue(format!("draft {key} is invalid")))?;
    Ok(Some(number))
}

fn optional_string(value: &Value, key: &str) -> Result<Option<String>, GuiStateError> {
    let Some(string) = value.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    Ok(Some(
        string
            .as_str()
            .ok_or_else(|| GuiStateError::InvalidValue(format!("draft {key} is invalid")))?
            .to_owned(),
    ))
}

fn media_role_name(role: MediaRole) -> &'static str {
    match role {
        MediaRole::StartFrame => "start_frame",
        MediaRole::EndFrame => "end_frame",
        MediaRole::Reference => "reference",
        MediaRole::VideoInput => "video_input",
        MediaRole::AudioInput => "audio_input",
    }
}

fn parse_media_role(value: &str) -> Result<MediaRole, GuiStateError> {
    match value {
        "start_frame" => Ok(MediaRole::StartFrame),
        "end_frame" => Ok(MediaRole::EndFrame),
        "reference" => Ok(MediaRole::Reference),
        "video_input" => Ok(MediaRole::VideoInput),
        "audio_input" => Ok(MediaRole::AudioInput),
        _ => Err(GuiStateError::InvalidValue(format!(
            "unknown draft media role {value}"
        ))),
    }
}

fn stored_media_source(source: &MediaSource) -> StoredMediaSource {
    match source {
        MediaSource::LocalFile { path } => StoredMediaSource::LocalFile(path.clone()),
        MediaSource::RemoteUrl { url } => StoredMediaSource::RemoteUrl(url.clone()),
    }
}

fn domain_receipt_to_stored(receipt: &UploadReceipt, source_path: PathBuf) -> StoredUploadReceipt {
    StoredUploadReceipt {
        provider_id: receipt.provider_id.clone(),
        source_sha256: receipt.sha256.clone(),
        source_path,
        remote_url: receipt.public_url.clone(),
        content_type: receipt.content_type.clone().unwrap_or_default(),
        byte_length: receipt.size_bytes,
        created_at: receipt.uploaded_at,
        expires_at: receipt.expires_at,
    }
}

fn stored_receipt_to_domain(receipt: &StoredUploadReceipt) -> Result<UploadReceipt, GuiStateError> {
    UploadReceipt::new(
        receipt.provider_id.clone(),
        receipt.source_sha256.clone(),
        receipt.remote_url.clone(),
        receipt.created_at,
        receipt.expires_at,
        (!receipt.content_type.is_empty()).then(|| receipt.content_type.clone()),
        receipt.byte_length,
    )
    .map_err(|error| GuiStateError::InvalidValue(error.to_string()))
}

fn upload_receipt_covers_review(receipt: &UploadReceipt, now: DateTime<Utc>) -> bool {
    let required = PREPARED_REVIEW_LIFETIME.saturating_add(STAGED_MEDIA_EXPIRY_MARGIN);
    chrono::TimeDelta::from_std(required)
        .ok()
        .is_some_and(|horizon| receipt.expires_at >= now + horizon)
}

fn staged_media_valid_for_submission(staged: &[StagedMedia], now: DateTime<Utc>) -> bool {
    let Some(margin) = chrono::TimeDelta::from_std(STAGED_MEDIA_EXPIRY_MARGIN).ok() else {
        return false;
    };
    staged.iter().all(|media| {
        media
            .receipt
            .as_ref()
            .is_none_or(|receipt| receipt.expires_at > now + margin)
    })
}

fn staged_media_submit_before(staged: &[StagedMedia]) -> Option<DateTime<Utc>> {
    let margin = chrono::TimeDelta::from_std(STAGED_MEDIA_EXPIRY_MARGIN).ok()?;
    staged
        .iter()
        .filter_map(|media| media.receipt.as_ref())
        .map(|receipt| {
            receipt
                .expires_at
                .checked_sub_signed(margin)
                .unwrap_or(DateTime::<Utc>::MIN_UTC)
        })
        .min()
}

fn prepared_review_window(
    staged: &[StagedMedia],
    now: DateTime<Utc>,
) -> Option<(DateTime<Utc>, Duration)> {
    let full_lifetime = chrono::TimeDelta::from_std(PREPARED_REVIEW_LIFETIME).ok()?;
    let margin = chrono::TimeDelta::from_std(STAGED_MEDIA_EXPIRY_MARGIN).ok()?;
    let mut expires_at = now + full_lifetime;
    for receipt in staged.iter().filter_map(|media| media.receipt.as_ref()) {
        expires_at = expires_at.min(receipt.expires_at.checked_sub_signed(margin)?);
    }
    let lifetime = (expires_at - now).to_std().ok()?;
    (!lifetime.is_zero()).then_some((expires_at, lifetime))
}

fn emit_gui_state_error(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    op_id: u64,
    scope: ServiceScope,
    error: GuiStateError,
) {
    emit_error(events, op_id, None, scope, error.to_string(), true, None);
}

#[derive(Debug)]
enum TaskFailureKind {
    Ordinary,
    SubmissionUncertain,
    RecoveryFailed,
}

#[derive(Debug)]
struct TaskFailure {
    provider_id: ProviderId,
    scope: ServiceScope,
    message: String,
    recoverable: bool,
    job_id: Option<String>,
    kind: TaskFailureKind,
}

impl TaskFailure {
    fn provider(scope: ServiceScope, error: ProviderError, job_id: Option<String>) -> Self {
        let recoverable = error.retryable();
        let kind = if error.kind == ProviderErrorKind::SubmissionUncertain {
            TaskFailureKind::SubmissionUncertain
        } else {
            TaskFailureKind::Ordinary
        };
        Self {
            provider_id: error.provider_id,
            scope,
            message: error.message,
            recoverable,
            job_id,
            kind,
        }
    }

    fn history(
        provider_id: ProviderId,
        scope: ServiceScope,
        error: HistoryError,
        job_id: Option<String>,
    ) -> Self {
        Self {
            provider_id,
            scope,
            message: error.to_string(),
            recoverable: false,
            job_id,
            kind: TaskFailureKind::Ordinary,
        }
    }
}

struct GuiRecoverySaved {
    media_error: Option<GuiStateError>,
}

async fn persist_accepted_gui_state(
    gui_state: &GuiStateStore,
    key: &ProviderJobKey,
    locator: &JobLocator,
    prepared_media: Vec<PreparedMediaAssociation>,
) -> Result<GuiRecoverySaved, GuiStateError> {
    let state = gui_state.clone();
    let saved_key = key.clone();
    let saved_locator = locator.clone();
    tokio::task::spawn_blocking(move || {
        state.save_resumable_job(&ResumableJob {
            key: saved_key.clone(),
            locator: saved_locator,
            accepted_at: Utc::now(),
            monitoring_paused: false,
        })?;
        let media = prepared_media
            .into_iter()
            .map(|item| GenerationMediaAssociation {
                key: saved_key.clone(),
                position: item.position,
                draft_media_id: item.draft_media_id,
                role: item.role,
                source: item.source,
                resolved_url: item.resolved_url,
            })
            .collect::<Vec<_>>();
        let media_error = (!media.is_empty())
            .then(|| state.replace_generation_media(&saved_key, &media).err())
            .flatten();
        Ok::<GuiRecoverySaved, GuiStateError>(GuiRecoverySaved { media_error })
    })
    .await
    .map_err(|_| GuiStateError::InvalidValue("GUI recovery save task failed".into()))?
}

async fn persist_existing_gui_state(
    op_id: u64,
    gui_state: &GuiStateStore,
    job: &VideoJob,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) {
    match persist_accepted_gui_state(gui_state, &job.key(), &job.locator, Vec::new()).await {
        Ok(saved) => {
            if let Some(error) = saved.media_error {
                emit_error(
                    events,
                    op_id,
                    Some(job.provider_id.clone()),
                    ServiceScope::History,
                    error.to_string(),
                    true,
                    Some(job.id.clone()),
                );
            }
        }
        Err(error) => emit_error(
            events,
            op_id,
            Some(job.provider_id.clone()),
            ServiceScope::History,
            format!("Could not save resumable GUI job state: {error}"),
            true,
            Some(job.id.clone()),
        ),
    }
}

async fn clear_uncertain_submission_record(
    gui_state: &GuiStateStore,
    op_id: u64,
    record: &UncertainSubmissionRecord,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) -> Result<bool, GuiStateError> {
    let state = gui_state.clone();
    let provider_id = record.provider_id.clone();
    let draft_fingerprint = record.draft_fingerprint.clone();
    let clear_provider = provider_id.clone();
    let clear_fingerprint = draft_fingerprint.clone();
    let removed = tokio::task::spawn_blocking(move || {
        state.clear_uncertain_submission(&clear_provider, &clear_fingerprint)
    })
    .await
    .map_err(|_| {
        GuiStateError::InvalidValue("uncertain-submission safety clear task failed".into())
    })??;
    let _ = events.send(ServiceEvent::UncertainSubmissionCleared {
        op_id,
        provider_id,
        draft_fingerprint,
        removed,
    });
    Ok(removed)
}

#[allow(clippy::too_many_arguments)]
async fn run_generate(
    task_id: u64,
    op_id: u64,
    provider_id: ProviderId,
    request: VideoRequest,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    gui_state: GuiStateStore,
    safety_record: UncertainSubmissionRecord,
    prepared_media: Vec<PreparedMediaAssociation>,
    submit_before: Option<DateTime<Utc>>,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    notices: mpsc::UnboundedSender<OperationNotice>,
) {
    let result = run_generate_inner(
        task_id,
        op_id,
        provider_id,
        request,
        provider,
        history,
        gui_state,
        safety_record.clone(),
        prepared_media,
        submit_before,
        paths,
        config,
        cancel,
        &events,
        &notices,
    )
    .await;
    if let Err(error) = result {
        let draft_fingerprint = matches!(error.kind, TaskFailureKind::SubmissionUncertain)
            .then(|| safety_record.draft_fingerprint.clone());
        emit_task_failure(&events, op_id, error, draft_fingerprint);
    }
    let _ = notices.send(OperationNotice::Finished { task_id });
}

#[allow(clippy::too_many_arguments)]
async fn run_generate_inner(
    task_id: u64,
    op_id: u64,
    provider_id: ProviderId,
    request: VideoRequest,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    gui_state: GuiStateStore,
    safety_record: UncertainSubmissionRecord,
    prepared_media: Vec<PreparedMediaAssociation>,
    submit_before: Option<DateTime<Utc>>,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    events: &mpsc::UnboundedSender<ServiceEvent>,
    notices: &mpsc::UnboundedSender<OperationNotice>,
) -> Result<(), TaskFailure> {
    let _ = events.send(ServiceEvent::SubmissionStarted {
        op_id,
        provider_id: provider_id.clone(),
    });
    // Do not observe cancellation until the accepted id is surfaced and saved.
    let job = match provider.submit_prepared(&request, submit_before).await {
        Ok(job) => job,
        Err(error) if error.kind == ProviderErrorKind::SubmissionUncertain => {
            return Err(TaskFailure::provider(ServiceScope::Generation, error, None));
        }
        Err(error) => {
            if let Err(clear_error) =
                clear_uncertain_submission_record(&gui_state, op_id, &safety_record, events).await
            {
                emit_error(
                    events,
                    op_id,
                    Some(provider_id.clone()),
                    ServiceScope::Generation,
                    format!(
                        "The provider definitively rejected or did not receive the request, but its local safety barrier could not be cleared: {clear_error}"
                    ),
                    false,
                    None,
                );
            }
            return Err(TaskFailure::provider(ServiceScope::Generation, error, None));
        }
    };
    let _ = events.send(ServiceEvent::JobAccepted {
        op_id,
        provider_id: provider_id.clone(),
        job: job.clone(),
        record: None,
    });
    let key = job.key();
    let gui_recovery =
        persist_accepted_gui_state(&gui_state, &key, &job.locator, prepared_media).await;
    let mut gui_recovery_error = None;
    let mut safety_cleared = false;
    match gui_recovery {
        Ok(saved) => {
            let _ = events.send(ServiceEvent::JobRecoverySaved {
                op_id,
                provider_id: provider_id.clone(),
                key: key.clone(),
                store: RecoveryStore::GuiState,
            });
            let _ = notices.send(OperationNotice::Monitoring {
                task_id,
                key: key.clone(),
            });
            match clear_uncertain_submission_record(&gui_state, op_id, &safety_record, events).await
            {
                Ok(_) => safety_cleared = true,
                Err(error) => {
                    let _ = events.send(ServiceEvent::JobRecoveryWarning {
                        op_id,
                        provider_id: provider_id.clone(),
                        key: key.clone(),
                        message: format!(
                            "The accepted job is recoverable, but its conservative submission safety barrier remains: {error}"
                        ),
                    });
                }
            }
            if let Some(error) = saved.media_error {
                let _ = events.send(ServiceEvent::JobRecoveryWarning {
                    op_id,
                    provider_id: provider_id.clone(),
                    key: key.clone(),
                    message: format!(
                        "The job is recoverable, but its source media links were not saved: {error}"
                    ),
                });
            }
        }
        Err(error) => gui_recovery_error = Some(error),
    }
    let saved_history = history.clone();
    let saved_provider = provider_id.clone();
    let saved_request = request.clone();
    let saved_job = job.clone();
    let history_result = tokio::task::spawn_blocking(move || {
        saved_history.create_provider_job(&saved_provider, &saved_request, &saved_job)
    })
    .await
    .map_err(|_| TaskFailure {
        provider_id: provider_id.clone(),
        scope: ServiceScope::History,
        message: "History task failed after the provider accepted the job".into(),
        recoverable: false,
        job_id: Some(job.id.clone()),
        kind: TaskFailureKind::Ordinary,
    })
    .and_then(|result| {
        result.map_err(|error| {
            TaskFailure::history(
                provider_id.clone(),
                ServiceScope::History,
                error,
                Some(job.id.clone()),
            )
        })
    });
    let record = match history_result {
        Ok(record) => {
            if gui_recovery_error.is_some() {
                let _ = events.send(ServiceEvent::JobRecoverySaved {
                    op_id,
                    provider_id: provider_id.clone(),
                    key: key.clone(),
                    store: RecoveryStore::History,
                });
                let _ = notices.send(OperationNotice::Monitoring {
                    task_id,
                    key: key.clone(),
                });
                if !safety_cleared {
                    match clear_uncertain_submission_record(
                        &gui_state,
                        op_id,
                        &safety_record,
                        events,
                    )
                    .await
                    {
                        Ok(_) => {}
                        Err(error) => {
                            let _ = events.send(ServiceEvent::JobRecoveryWarning {
                                op_id,
                                provider_id: provider_id.clone(),
                                key: key.clone(),
                                message: format!(
                                    "The accepted job is in compatible history, but its conservative submission safety barrier remains: {error}"
                                ),
                            });
                        }
                    }
                }
                if let Some(error) = &gui_recovery_error {
                    let _ = events.send(ServiceEvent::JobRecoveryWarning {
                        op_id,
                        provider_id: provider_id.clone(),
                        key: key.clone(),
                        message: format!(
                            "The job was saved to compatible history, but GUI recovery state failed: {error}"
                        ),
                    });
                }
            }
            record
        }
        Err(mut error) => {
            if let Some(gui_error) = gui_recovery_error {
                error.message = format!(
                    "{}; GUI recovery state also failed: {gui_error}",
                    error.message
                );
                error.kind = TaskFailureKind::RecoveryFailed;
            }
            return Err(error);
        }
    };
    let _ = events.send(ServiceEvent::JobUpdated {
        op_id,
        provider_id: provider_id.clone(),
        job: job.clone(),
        record: record.clone(),
    });
    monitor_job(
        op_id,
        provider_id,
        provider,
        history,
        gui_state,
        paths,
        config,
        cancel,
        request,
        job,
        record,
        events,
    )
    .await
}

enum ExistingJob {
    Resume(ProviderJobKey),
    Import(JobLocator),
}

#[allow(clippy::too_many_arguments)]
async fn run_existing(
    task_id: u64,
    op_id: u64,
    provider_id: ProviderId,
    existing: ExistingJob,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    gui_state: GuiStateStore,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    notices: mpsc::UnboundedSender<OperationNotice>,
) {
    let result = run_existing_inner(
        op_id,
        provider_id,
        existing,
        provider,
        history,
        gui_state,
        paths,
        config,
        cancel,
        &events,
    )
    .await;
    if let Err(error) = result {
        emit_task_failure(&events, op_id, error, None);
    }
    let _ = notices.send(OperationNotice::Finished { task_id });
}

#[allow(clippy::too_many_arguments)]
async fn run_existing_inner(
    op_id: u64,
    provider_id: ProviderId,
    existing: ExistingJob,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    gui_state: GuiStateStore,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) -> Result<(), TaskFailure> {
    let (request, job, record) = match existing {
        ExistingJob::Import(locator) => {
            let requested_id = locator.remote_job_id().to_owned();
            let job = provider.import(&locator).await.map_err(|error| {
                TaskFailure::provider(ServiceScope::Import, error, Some(requested_id))
            })?;
            let saved_history = history.clone();
            let saved_provider = provider_id.clone();
            let saved_job = job.clone();
            let record = tokio::task::spawn_blocking(move || {
                saved_history.import_provider_job(&saved_provider, &saved_job, None)
            })
            .await
            .map_err(|_| TaskFailure {
                provider_id: provider_id.clone(),
                scope: ServiceScope::History,
                message: "History import task failed".into(),
                recoverable: true,
                job_id: Some(job.id.clone()),
                kind: TaskFailureKind::Ordinary,
            })?
            .map_err(|error| {
                TaskFailure::history(
                    provider_id.clone(),
                    ServiceScope::History,
                    error,
                    Some(job.id.clone()),
                )
            })?;
            let _ = events.send(ServiceEvent::Imported {
                op_id,
                provider_id: provider_id.clone(),
                job: job.clone(),
                record: record.clone(),
            });
            persist_existing_gui_state(op_id, &gui_state, &job, events).await;
            (record.request.clone(), job, record)
        }
        ExistingJob::Resume(key) => {
            let lookup = history.clone();
            let saved_key = key.clone();
            let stored = tokio::task::spawn_blocking(move || lookup.get_provider(&saved_key))
                .await
                .map_err(|_| TaskFailure {
                    provider_id: provider_id.clone(),
                    scope: ServiceScope::History,
                    message: "History lookup task failed".into(),
                    recoverable: true,
                    job_id: Some(key.remote_job_id.clone()),
                    kind: TaskFailureKind::Ordinary,
                })?
                .map_err(|error| {
                    TaskFailure::history(
                        provider_id.clone(),
                        ServiceScope::History,
                        error,
                        Some(key.remote_job_id.clone()),
                    )
                })?
                .ok_or_else(|| TaskFailure {
                    provider_id: provider_id.clone(),
                    scope: ServiceScope::History,
                    message: format!(
                        "No saved {} job named {} was found",
                        provider_id, key.remote_job_id
                    ),
                    recoverable: true,
                    job_id: Some(key.remote_job_id.clone()),
                    kind: TaskFailureKind::Ordinary,
                })?;
            let job = provider.poll(&stored.locator).await.map_err(|error| {
                TaskFailure::provider(ServiceScope::Generation, error, Some(stored.job_id.clone()))
            })?;
            let updated = update_history(&history, &provider_id, &job, None).await?;
            let _ = events.send(ServiceEvent::JobUpdated {
                op_id,
                provider_id: provider_id.clone(),
                job: job.clone(),
                record: updated.clone(),
            });
            if let Some(path) = updated.output_path.clone()
                && path.is_file()
            {
                remove_resumable_state(&gui_state, &updated.key()).await;
                let _ = events.send(ServiceEvent::Downloaded {
                    op_id,
                    provider_id,
                    job,
                    record: updated,
                    path,
                });
                return Ok(());
            }
            persist_existing_gui_state(op_id, &gui_state, &job, events).await;
            (updated.request.clone(), job, updated)
        }
    };
    let request = request.unwrap_or_else(|| request_from_job(&provider_id, &job));
    monitor_job(
        op_id,
        provider_id,
        provider,
        history,
        gui_state,
        paths,
        config,
        cancel,
        request,
        job,
        record,
        events,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn monitor_job(
    op_id: u64,
    provider_id: ProviderId,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    gui_state: GuiStateStore,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    request: VideoRequest,
    mut job: VideoJob,
    _record: JobRecord,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) -> Result<(), TaskFailure> {
    if cancel.load(Ordering::Acquire) {
        pause_resumable_state(&gui_state, &job.key()).await;
        emit_cancelled(events, op_id, provider_id, Some(job.id), true);
        return Ok(());
    }
    let mut attempts = 0usize;
    while !job.terminal() && attempts < config.max_poll_attempts {
        if wait_with_countdown(
            op_id,
            &provider_id,
            &job.id,
            attempts + 1,
            config.poll_interval,
            &cancel,
            events,
        )
        .await
        {
            pause_resumable_state(&gui_state, &job.key()).await;
            emit_cancelled(events, op_id, provider_id, Some(job.id), true);
            return Ok(());
        }
        attempts += 1;
        job = provider.poll(&job.locator).await.map_err(|error| {
            TaskFailure::provider(ServiceScope::Generation, error, Some(job.id.clone()))
        })?;
        let record = update_history(&history, &provider_id, &job, None).await?;
        let _ = events.send(ServiceEvent::JobUpdated {
            op_id,
            provider_id: provider_id.clone(),
            job: job.clone(),
            record,
        });
        if cancel.load(Ordering::Acquire) {
            pause_resumable_state(&gui_state, &job.key()).await;
            emit_cancelled(events, op_id, provider_id, Some(job.id), true);
            return Ok(());
        }
    }
    if !job.terminal() {
        pause_resumable_state(&gui_state, &job.key()).await;
        return Err(TaskFailure {
            provider_id,
            scope: ServiceScope::Generation,
            message: "Monitoring reached its local limit. The remote job was not cancelled; open History later to resume checking it.".into(),
            recoverable: true,
            job_id: Some(job.id),
            kind: TaskFailureKind::Ordinary,
        });
    }
    if job.status != JobStatus::Completed {
        remove_resumable_state(&gui_state, &job.key()).await;
        return Err(TaskFailure {
            provider_id,
            scope: ServiceScope::Generation,
            message: job
                .error
                .clone()
                .unwrap_or_else(|| format!("The provider marked the job {}.", job.status.as_str())),
            recoverable: false,
            job_id: Some(job.id),
            kind: TaskFailureKind::Ordinary,
        });
    }
    if cancel.load(Ordering::Acquire) {
        pause_resumable_state(&gui_state, &job.key()).await;
        emit_cancelled(events, op_id, provider_id, Some(job.id), true);
        return Ok(());
    }
    let artifact = job.artifacts.first().cloned().ok_or_else(|| TaskFailure {
        provider_id: provider_id.clone(),
        scope: ServiceScope::Generation,
        message: "The provider completed without a video artifact".into(),
        recoverable: true,
        job_id: Some(job.id.clone()),
        kind: TaskFailureKind::Ordinary,
    })?;
    let destination = make_output_path(&request.prompt, &job.id, &paths.videos_dir);
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
    let download_result = {
        let download = provider.download(&artifact, &destination, Some(progress_tx));
        tokio::pin!(download);
        let mut progress_open = true;
        loop {
            tokio::select! {
                result = &mut download => break Some(result),
                progress = progress_rx.recv(), if progress_open => {
                    match progress {
                        Some(progress) => { let _ = events.send(ServiceEvent::DownloadProgress {
                            op_id,
                            provider_id: provider_id.clone(),
                            job_id: job.id.clone(),
                            written: progress.written,
                            total: progress.total,
                        }); }
                        None => progress_open = false,
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if cancel.load(Ordering::Acquire) { break None; }
                }
            }
        }
    };
    let Some(download_result) = download_result else {
        let _ = tokio::fs::remove_file(partial_path(&destination)).await;
        pause_resumable_state(&gui_state, &job.key()).await;
        emit_cancelled(events, op_id, provider_id, Some(job.id), true);
        return Ok(());
    };
    let saved = download_result.map_err(|error| {
        TaskFailure::provider(ServiceScope::Generation, error, Some(job.id.clone()))
    })?;
    let record = update_history(&history, &provider_id, &job, Some(saved.clone())).await?;
    remove_resumable_state(&gui_state, &job.key()).await;
    let _ = events.send(ServiceEvent::Downloaded {
        op_id,
        provider_id,
        job,
        record,
        path: saved,
    });
    Ok(())
}

async fn pause_resumable_state(gui_state: &GuiStateStore, key: &ProviderJobKey) {
    let state = gui_state.clone();
    let key = key.clone();
    let _ = tokio::task::spawn_blocking(move || state.set_monitoring_paused(&key, true)).await;
}

async fn remove_resumable_state(gui_state: &GuiStateStore, key: &ProviderJobKey) {
    let state = gui_state.clone();
    let key = key.clone();
    let _ = tokio::task::spawn_blocking(move || state.remove_resumable_job(&key)).await;
}

async fn update_history(
    history: &HistoryStore,
    provider_id: &ProviderId,
    job: &VideoJob,
    output_path: Option<PathBuf>,
) -> Result<JobRecord, TaskFailure> {
    let history = history.clone();
    let provider_id = provider_id.clone();
    let saved_provider = provider_id.clone();
    let job = job.clone();
    let job_id = job.id.clone();
    tokio::task::spawn_blocking(move || {
        history.update_provider_job(&saved_provider, &job, output_path.as_deref())
    })
    .await
    .map_err(|_| TaskFailure {
        provider_id: provider_id.clone(),
        scope: ServiceScope::History,
        message: "History update task failed".into(),
        recoverable: true,
        job_id: Some(job_id.clone()),
        kind: TaskFailureKind::Ordinary,
    })?
    .map_err(|error| TaskFailure::history(provider_id, ServiceScope::History, error, Some(job_id)))
}

fn request_from_job(provider_id: &ProviderId, job: &VideoJob) -> VideoRequest {
    let model = job
        .raw
        .get("model")
        .or_else(|| job.raw.pointer("/result/model"))
        .and_then(Value::as_str)
        .unwrap_or("imported-video-job");
    let prompt = job
        .raw
        .get("prompt")
        .or_else(|| job.raw.pointer("/result/prompt"))
        .and_then(Value::as_str)
        .unwrap_or("Imported video");
    VideoRequest::for_provider(provider_id.clone(), model, prompt).unwrap_or_else(|_| {
        VideoRequest::for_provider(provider_id.clone(), "imported-video-job", "Imported video")
            .expect("static imported request is valid")
    })
}

async fn wait_or_cancel(duration: Duration, cancel: &AtomicBool) -> bool {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if cancel.load(Ordering::Acquire) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(100))).await;
    }
}

async fn wait_with_countdown(
    op_id: u64,
    provider_id: &ProviderId,
    job_id: &str,
    attempt: usize,
    duration: Duration,
    cancel: &AtomicBool,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) -> bool {
    let mut remaining = duration;
    while !remaining.is_zero() {
        let _ = events.send(ServiceEvent::PollWaiting {
            op_id,
            provider_id: provider_id.clone(),
            job_id: job_id.to_owned(),
            attempt,
            next_in: remaining,
        });
        let step = remaining.min(Duration::from_secs(1));
        if wait_or_cancel(step, cancel).await {
            return true;
        }
        remaining = remaining.saturating_sub(step);
    }
    cancel.load(Ordering::Acquire)
}

fn emit_cancelled(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    op_id: u64,
    provider_id: ProviderId,
    job_id: Option<String>,
    remote_continues: bool,
) {
    let _ = events.send(ServiceEvent::Cancelled {
        op_id,
        provider_id: Some(provider_id),
        job_id,
        remote_continues,
    });
}

fn emit_provider_error(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    op_id: u64,
    scope: ServiceScope,
    error: ProviderError,
    job_id: Option<String>,
) {
    if error.kind == ProviderErrorKind::SubmissionUncertain {
        let _ = events.send(ServiceEvent::SubmissionUncertain {
            op_id,
            provider_id: error.provider_id,
            message: error.message,
            draft_fingerprint: None,
        });
        return;
    }
    let recoverable = error.retryable();
    emit_error(
        events,
        op_id,
        Some(error.provider_id),
        scope,
        error.message,
        recoverable,
        job_id,
    );
}

fn emit_task_failure(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    op_id: u64,
    error: TaskFailure,
    draft_fingerprint: Option<String>,
) {
    match error.kind {
        TaskFailureKind::SubmissionUncertain => {
            let _ = events.send(ServiceEvent::SubmissionUncertain {
                op_id,
                provider_id: error.provider_id,
                message: error.message,
                draft_fingerprint,
            });
        }
        TaskFailureKind::RecoveryFailed => {
            let remote_job_id = error
                .job_id
                .unwrap_or_else(|| "accepted-job-id-unavailable".into());
            let key = ProviderJobKey {
                provider_id: error.provider_id.clone(),
                remote_job_id,
            };
            let _ = events.send(ServiceEvent::JobRecoveryFailed {
                op_id,
                provider_id: error.provider_id,
                key,
                message: error.message,
            });
        }
        TaskFailureKind::Ordinary => emit_error(
            events,
            op_id,
            Some(error.provider_id),
            error.scope,
            error.message,
            error.recoverable,
            error.job_id,
        ),
    }
}

fn emit_missing_key(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    op_id: u64,
    provider_id: &ProviderId,
    scope: ServiceScope,
    job_id: Option<String>,
) {
    emit_error(
        events,
        op_id,
        Some(provider_id.clone()),
        scope,
        format!(
            "Connect a {} API key first",
            descriptor(provider_id).display_name
        ),
        true,
        job_id,
    );
}

fn emit_unknown_provider(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    op_id: u64,
    provider_id: &ProviderId,
    scope: ServiceScope,
) {
    emit_error(
        events,
        op_id,
        Some(provider_id.clone()),
        scope,
        format!("Unsupported video provider {provider_id}"),
        false,
        None,
    );
}

fn emit_error(
    events: &mpsc::UnboundedSender<ServiceEvent>,
    op_id: u64,
    provider_id: Option<ProviderId>,
    scope: ServiceScope,
    message: String,
    recoverable: bool,
    job_id: Option<String>,
) {
    let _ = events.send(ServiceEvent::Error {
        op_id,
        provider_id,
        scope,
        message,
        recoverable,
        job_id,
    });
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn uploaded_media(now: DateTime<Utc>, expires_at: DateTime<Utc>) -> StagedMedia {
        let receipt = UploadReceipt::new(
            ProviderId::fal(),
            "a".repeat(64),
            "https://v3.fal.media/files/reference.png",
            now - TimeDelta::minutes(1),
            expires_at,
            Some("image/png".into()),
            128,
        )
        .expect("upload receipt");
        StagedMedia::uploaded(MediaRole::Reference, receipt).expect("staged media")
    }

    #[test]
    fn near_expiry_receipt_cannot_reach_paid_submission() {
        let now = Utc::now();
        let near_expiry = uploaded_media(now, now + TimeDelta::seconds(45));
        let receipt = near_expiry.receipt.as_ref().expect("receipt");

        assert!(!upload_receipt_covers_review(receipt, now));
        assert!(prepared_review_window(std::slice::from_ref(&near_expiry), now).is_none());
        assert!(!staged_media_valid_for_submission(&[near_expiry], now));

        let safe = uploaded_media(now, now + TimeDelta::hours(1));
        let expected_submit_before = safe.receipt.as_ref().expect("receipt").expires_at
            - TimeDelta::from_std(STAGED_MEDIA_EXPIRY_MARGIN).expect("fixed margin");
        assert!(upload_receipt_covers_review(
            safe.receipt.as_ref().expect("receipt"),
            now
        ));
        assert!(prepared_review_window(std::slice::from_ref(&safe), now).is_some());
        assert_eq!(
            staged_media_submit_before(std::slice::from_ref(&safe)),
            Some(expected_submit_before)
        );
        let earlier = uploaded_media(now, now + TimeDelta::minutes(45));
        let earlier_submit_before = earlier.receipt.as_ref().expect("receipt").expires_at
            - TimeDelta::from_std(STAGED_MEDIA_EXPIRY_MARGIN).expect("fixed margin");
        assert_eq!(
            staged_media_submit_before(&[safe.clone(), earlier.clone()]),
            Some(earlier_submit_before),
            "the earliest staged receipt controls the late paid-submit guard"
        );
        assert!(staged_media_valid_for_submission(
            &[safe.clone(), earlier],
            now
        ));
        assert!(staged_media_valid_for_submission(&[safe], now));
    }

    #[test]
    fn direct_request_fingerprints_follow_provider_media_ordering() {
        let reference = |url: &str, kind| {
            crate::domain::InputReference::with_kind(url, kind).expect("typed reference")
        };
        let mut fal =
            VideoRequest::for_provider(ProviderId::fal(), "model", "prompt").expect("fal request");
        fal.input_references = vec![
            reference(
                "https://media.example/video-a.mp4",
                InputReferenceKind::Video,
            ),
            reference("https://media.example/audio.wav", InputReferenceKind::Audio),
            reference(
                "https://media.example/video-b.mp4",
                InputReferenceKind::Video,
            ),
        ];
        let mut cross_kind = fal.clone();
        cross_kind.input_references.swap(0, 1);
        let fal_fingerprints =
            video_request_fingerprint_candidates(&fal).expect("fal fingerprints");
        assert_eq!(fal_fingerprints.len(), 2);
        assert_eq!(
            fal_fingerprints[0],
            video_request_fingerprint_candidates(&cross_kind).expect("cross-kind fingerprints")[0]
        );

        let mut same_kind = fal.clone();
        same_kind.input_references.swap(0, 2);
        assert_ne!(
            fal_fingerprints[0],
            video_request_fingerprint_candidates(&same_kind).expect("same-kind fingerprints")[0]
        );

        let mut openrouter = fal;
        openrouter.provider_id = ProviderId::openrouter();
        let mut reordered_openrouter = openrouter.clone();
        reordered_openrouter.input_references.swap(0, 1);
        assert_ne!(
            video_request_fingerprint_candidates(&openrouter).expect("OpenRouter fingerprints")[0],
            video_request_fingerprint_candidates(&reordered_openrouter)
                .expect("reordered OpenRouter fingerprints")[0],
            "OpenRouter's documented mixed input_references array remains ordered"
        );
    }
}
