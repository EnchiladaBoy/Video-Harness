//! Provider-aware long-running service bridge between the reducer and backend I/O.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    CostQuote, JobLocator, JobStatus, ProviderDescriptor, ProviderId, ProviderJobKey, VideoCatalog,
    VideoJob, VideoRequest,
};
use crate::history::{HistoryError, HistoryStore, JobRecord};
use crate::providers::fal::{FalOptions, FalProvider};
use crate::providers::openrouter::OpenRouterProvider;
use crate::providers::{ProviderAccount, ProviderError, ProviderErrorKind, VideoProvider};

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
    Import {
        op_id: u64,
        provider_id: ProviderId,
        locator: JobLocator,
    },
    CancelCurrent {
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
    op_id: u64,
    cancel: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
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
    let init_history = history.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || init_history.initialize())
        .await
        .unwrap_or_else(|_| Err(HistoryError::MissingSavedRecord))
    {
        emit_error(
            &events,
            0,
            None,
            ServiceScope::Startup,
            error.to_string(),
            false,
            None,
        );
        return;
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

    let (finished_tx, mut finished_rx) = mpsc::unbounded_channel::<u64>();
    let mut active: Option<ActiveOperation> = None;
    let mut shutting_down = false;

    loop {
        tokio::select! {
            Some(finished_op) = finished_rx.recv() => {
                if active.as_ref().is_some_and(|operation| operation.op_id == finished_op)
                    && let Some(operation) = active.take()
                {
                    let _ = operation.task.await;
                }
                if shutting_down && active.is_none() {
                    let _ = events.send(ServiceEvent::ShutdownComplete);
                    break;
                }
            }
            command = commands.recv(), if !shutting_down => {
                let Some(command) = command else {
                    shutting_down = true;
                    if let Some(operation) = &active {
                        operation.cancel.store(true, Ordering::Release);
                    } else {
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
                                        provider_id,
                                        info,
                                        credential_status,
                                    });
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
                            provider_id,
                            credential_status,
                        });
                    }
                    ServiceCommand::RefreshCatalog { op_id, provider_id } => {
                        let key = sessions.get(&provider_id).and_then(|session| session.key.as_ref());
                        refresh_catalog(
                            op_id,
                            provider_id,
                            key,
                            &paths,
                            &config,
                            executor.clone(),
                            &events,
                        ).await;
                    }
                    ServiceCommand::Quote { op_id, provider_id, request } => {
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
                        if let Some(operation) = &active {
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
                    ServiceCommand::Generate { op_id, provider_id, mut request } => {
                        if active.is_some() {
                            emit_error(&events, op_id, Some(provider_id), ServiceScope::Generation, "Another generation is already active".into(), true, None);
                            continue;
                        }
                        request.provider_id = provider_id.clone();
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
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task = tokio::spawn(run_generate(
                            op_id,
                            provider_id.clone(),
                            request,
                            provider,
                            history.clone(),
                            paths.clone(),
                            config.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            finished_tx.clone(),
                        ));
                        active = Some(ActiveOperation { op_id, cancel, task });
                    }
                    ServiceCommand::Resume { op_id, key } => {
                        if active.is_some() {
                            emit_error(&events, op_id, Some(key.provider_id.clone()), ServiceScope::Generation, "Another job is already active".into(), true, Some(key.remote_job_id));
                            continue;
                        }
                        let provider_id = key.provider_id.clone();
                        let Some(secret) = sessions.get(&provider_id).and_then(|session| session.key.clone()) else {
                            emit_missing_key(&events, op_id, &provider_id, ServiceScope::Credential, Some(key.remote_job_id));
                            continue;
                        };
                        let provider = match make_provider(&provider_id, &secret, &config, executor.clone()) {
                            Ok(provider) => provider,
                            Err(error) => {
                                emit_provider_error(&events, op_id, ServiceScope::Generation, error, Some(key.remote_job_id));
                                continue;
                            }
                        };
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task = tokio::spawn(run_existing(
                            op_id,
                            provider_id.clone(),
                            ExistingJob::Resume(key),
                            provider,
                            history.clone(),
                            paths.clone(),
                            config.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            finished_tx.clone(),
                        ));
                        active = Some(ActiveOperation { op_id, cancel, task });
                    }
                    ServiceCommand::Import { op_id, provider_id, locator } => {
                        if active.is_some() {
                            emit_error(&events, op_id, Some(provider_id), ServiceScope::Import, "Another job is already active".into(), true, Some(locator.remote_job_id().into()));
                            continue;
                        }
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
                        let cancel = Arc::new(AtomicBool::new(false));
                        let task = tokio::spawn(run_existing(
                            op_id,
                            provider_id.clone(),
                            ExistingJob::Import(locator),
                            provider,
                            history.clone(),
                            paths.clone(),
                            config.clone(),
                            Arc::clone(&cancel),
                            events.clone(),
                            finished_tx.clone(),
                        ));
                        active = Some(ActiveOperation { op_id, cancel, task });
                    }
                    ServiceCommand::Shutdown => {
                        shutting_down = true;
                        if let Some(operation) = &active {
                            operation.cancel.store(true, Ordering::Release);
                        } else {
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
    key: Option<&SecretString>,
    paths: &AppPaths,
    config: &ServiceConfig,
    executor: Option<Arc<dyn HttpExecutor>>,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) {
    // Both catalog APIs are public. A placeholder only satisfies adapter
    // construction and is never attached to catalog HTTP requests.
    let catalog_only_key = SecretString::from("catalog-only-placeholder".to_owned());
    let provider = match make_provider(
        &provider_id,
        key.unwrap_or(&catalog_only_key),
        config,
        executor,
    ) {
        Ok(provider) => provider,
        Err(error) => {
            emit_provider_error(events, op_id, ServiceScope::Catalog, error, None);
            return;
        }
    };
    let cache_path = match paths.provider_catalog_cache(&provider_id) {
        Ok(path) => path,
        Err(error) => {
            emit_error(
                events,
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
    let catalog = match provider.load_catalog().await {
        Ok(catalog) => {
            let cached = catalog.clone();
            let save_path = cache_path.clone();
            let _ = tokio::task::spawn_blocking(move || cached.save(save_path)).await;
            catalog
        }
        Err(live_error) => {
            match tokio::task::spawn_blocking(move || VideoCatalog::load(cache_path)).await {
                Ok(Ok(catalog)) if catalog.provider_id == provider_id => catalog,
                _ => {
                    emit_provider_error(events, op_id, ServiceScope::Catalog, live_error, None);
                    return;
                }
            }
        }
    };
    let settings_path = match paths.provider_model_settings(&provider_id) {
        Ok(path) => path,
        Err(error) => {
            emit_error(
                events,
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
    let _ = events.send(ServiceEvent::CatalogLoaded {
        op_id,
        provider_id,
        catalog,
        remembered_settings,
    });
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

#[derive(Debug)]
struct TaskFailure {
    provider_id: ProviderId,
    scope: ServiceScope,
    message: String,
    recoverable: bool,
    job_id: Option<String>,
}

impl TaskFailure {
    fn provider(scope: ServiceScope, error: ProviderError, job_id: Option<String>) -> Self {
        let recoverable = error.retryable();
        Self {
            provider_id: error.provider_id,
            scope,
            message: error.message,
            recoverable,
            job_id,
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
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_generate(
    op_id: u64,
    provider_id: ProviderId,
    request: VideoRequest,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    finished: mpsc::UnboundedSender<u64>,
) {
    let result = run_generate_inner(
        op_id,
        provider_id,
        request,
        provider,
        history,
        paths,
        config,
        cancel,
        &events,
    )
    .await;
    if let Err(error) = result {
        emit_error(
            &events,
            op_id,
            Some(error.provider_id),
            error.scope,
            error.message,
            error.recoverable,
            error.job_id,
        );
    }
    let _ = finished.send(op_id);
}

#[allow(clippy::too_many_arguments)]
async fn run_generate_inner(
    op_id: u64,
    provider_id: ProviderId,
    request: VideoRequest,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) -> Result<(), TaskFailure> {
    let _ = events.send(ServiceEvent::SubmissionStarted {
        op_id,
        provider_id: provider_id.clone(),
    });
    // Do not observe cancellation until the accepted id is surfaced and saved.
    let job = provider
        .submit(&request)
        .await
        .map_err(|error| TaskFailure::provider(ServiceScope::Generation, error, None))?;
    let _ = events.send(ServiceEvent::JobAccepted {
        op_id,
        provider_id: provider_id.clone(),
        job: job.clone(),
        record: None,
    });
    let saved_history = history.clone();
    let saved_provider = provider_id.clone();
    let saved_request = request.clone();
    let saved_job = job.clone();
    let record = tokio::task::spawn_blocking(move || {
        saved_history.create_provider_job(&saved_provider, &saved_request, &saved_job)
    })
    .await
    .map_err(|_| TaskFailure {
        provider_id: provider_id.clone(),
        scope: ServiceScope::History,
        message: "History task failed after the provider accepted the job".into(),
        recoverable: false,
        job_id: Some(job.id.clone()),
    })?
    .map_err(|error| {
        TaskFailure::history(
            provider_id.clone(),
            ServiceScope::History,
            error,
            Some(job.id.clone()),
        )
    })?;
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
    op_id: u64,
    provider_id: ProviderId,
    existing: ExistingJob,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    finished: mpsc::UnboundedSender<u64>,
) {
    let result = run_existing_inner(
        op_id,
        provider_id,
        existing,
        provider,
        history,
        paths,
        config,
        cancel,
        &events,
    )
    .await;
    if let Err(error) = result {
        emit_error(
            &events,
            op_id,
            Some(error.provider_id),
            error.scope,
            error.message,
            error.recoverable,
            error.job_id,
        );
    }
    let _ = finished.send(op_id);
}

#[allow(clippy::too_many_arguments)]
async fn run_existing_inner(
    op_id: u64,
    provider_id: ProviderId,
    existing: ExistingJob,
    provider: Arc<dyn VideoProvider>,
    history: HistoryStore,
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
                let _ = events.send(ServiceEvent::Downloaded {
                    op_id,
                    provider_id,
                    job,
                    record: updated,
                    path,
                });
                return Ok(());
            }
            (updated.request.clone(), job, updated)
        }
    };
    let request = request.unwrap_or_else(|| request_from_job(&provider_id, &job));
    monitor_job(
        op_id,
        provider_id,
        provider,
        history,
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
    paths: AppPaths,
    config: ServiceConfig,
    cancel: Arc<AtomicBool>,
    request: VideoRequest,
    mut job: VideoJob,
    _record: JobRecord,
    events: &mpsc::UnboundedSender<ServiceEvent>,
) -> Result<(), TaskFailure> {
    if cancel.load(Ordering::Acquire) {
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
            emit_cancelled(events, op_id, provider_id, Some(job.id), true);
            return Ok(());
        }
    }
    if !job.terminal() {
        return Err(TaskFailure {
            provider_id,
            scope: ServiceScope::Generation,
            message: "Monitoring reached its local limit. The remote job was not cancelled; open History later to resume checking it.".into(),
            recoverable: true,
            job_id: Some(job.id),
        });
    }
    if job.status != JobStatus::Completed {
        return Err(TaskFailure {
            provider_id,
            scope: ServiceScope::Generation,
            message: job
                .error
                .clone()
                .unwrap_or_else(|| format!("The provider marked the job {}.", job.status.as_str())),
            recoverable: false,
            job_id: Some(job.id),
        });
    }
    if cancel.load(Ordering::Acquire) {
        emit_cancelled(events, op_id, provider_id, Some(job.id), true);
        return Ok(());
    }
    let artifact = job.artifacts.first().cloned().ok_or_else(|| TaskFailure {
        provider_id: provider_id.clone(),
        scope: ServiceScope::Generation,
        message: "The provider completed without a video artifact".into(),
        recoverable: true,
        job_id: Some(job.id.clone()),
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
        emit_cancelled(events, op_id, provider_id, Some(job.id), true);
        return Ok(());
    };
    let saved = download_result.map_err(|error| {
        TaskFailure::provider(ServiceScope::Generation, error, Some(job.id.clone()))
    })?;
    let record = update_history(&history, &provider_id, &job, Some(saved.clone())).await?;
    let _ = events.send(ServiceEvent::Downloaded {
        op_id,
        provider_id,
        job,
        record,
        path: saved,
    });
    Ok(())
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
