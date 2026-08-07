use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
#[cfg(target_os = "linux")]
use video_harness::config::APP_NAME;
use video_harness::credentials::CredentialStatus;
use video_harness::domain::{
    CostQuote, DraftMedia, GenerationDraft as CoreDraft, JobStatus as CoreJobStatus,
    MediaKind as CoreMediaKind, MediaRole as CoreMediaRole, MediaSource, ProviderId,
    ProviderJobKey, VideoCatalog, VideoJob, VideoModel, VideoRequest,
};
use video_harness::gui_state::{DraftEditorState, UncertainSubmissionRecord};
use video_harness::history::JobRecord;
use video_harness::providers::ProviderAccount;
use video_harness::workflow::{PreparedGenerationId, ProviderConnection};
use video_harness::{AppPaths, ServiceCommand, ServiceConfig, ServiceEvent, spawn_service};
use zeroize::Zeroize;

const HISTORY_LIMIT: usize = 200;
const CROSS_PROVIDER_DISCLOSURE: &str = "Your local references will be uploaded to fal.ai as public-by-link files with a requested 24-hour expiry, then their URLs will be shared with OpenRouter and the selected model provider.";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MediaRole {
    Reference,
    StartFrame,
    EndFrame,
    VideoReference,
    AudioReference,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSummary {
    id: String,
    name: String,
    short_name: String,
    connected: bool,
    credential_storage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_label: Option<String>,
    description: String,
    local_media_note: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelCapabilities {
    images: bool,
    video: bool,
    audio_references: bool,
    generated_audio: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelSummary {
    id: String,
    provider_id: String,
    name: String,
    description: String,
    capabilities: ModelCapabilities,
    duration_options: Vec<String>,
    resolution_options: Vec<String>,
    aspect_ratio_options: Vec<String>,
    price_hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaItem {
    handle: String,
    display_name: String,
    kind: MediaKind,
    role: MediaRole,
    source: String,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerationSettings {
    duration: String,
    resolution: String,
    aspect_ratio: String,
    generated_audio: String,
    seed: String,
}

impl Default for GenerationSettings {
    fn default() -> Self {
        Self {
            duration: String::new(),
            resolution: String::new(),
            aspect_ratio: String::new(),
            generated_audio: "provider_default".into(),
            seed: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerationDraft {
    revision: u64,
    provider_id: String,
    model_id: String,
    prompt: String,
    media: Vec<MediaItem>,
    settings: GenerationSettings,
}

impl Default for GenerationDraft {
    fn default() -> Self {
        Self {
            revision: 0,
            provider_id: "openrouter".into(),
            model_id: String::new(),
            prompt: String::new(),
            media: Vec::new(),
            settings: GenerationSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedReview {
    prepared_id: u64,
    revision: u64,
    provider_id: String,
    provider_name: String,
    model_id: String,
    model_name: String,
    prompt: String,
    settings: GenerationSettings,
    media: Vec<MediaItem>,
    estimated_cost: String,
    expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_disclosure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    advanced_settings_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobSummary {
    id: String,
    provider_id: String,
    provider_name: String,
    model_name: String,
    prompt: String,
    status: String,
    status_label: String,
    detail: String,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_poll_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_file_name: Option<String>,
    has_local_output: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    playback_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    captions_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_continues: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_job_id: Option<String>,
    deletable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppSnapshot {
    providers: Vec<ProviderSummary>,
    models: Vec<ModelSummary>,
    draft: GenerationDraft,
    jobs: Vec<JobSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prepared_review: Option<PreparedReview>,
    draft_saved: bool,
    safety_holds: Vec<SafetyHoldSummary>,
}

impl Default for AppSnapshot {
    fn default() -> Self {
        Self {
            providers: vec![provider_summary("openrouter"), provider_summary("fal")],
            models: Vec::new(),
            draft: GenerationDraft::default(),
            jobs: Vec::new(),
            selected_job_id: None,
            prepared_review: None,
            draft_saved: true,
            safety_holds: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UiEvent {
    SnapshotChanged {
        snapshot: Box<AppSnapshot>,
    },
    ProviderChanged {
        provider: ProviderSummary,
    },
    ReviewReady {
        review: PreparedReview,
    },
    ReviewInvalidated {
        revision: u64,
    },
    JobAdded {
        job: JobSummary,
    },
    JobUpdated {
        job: JobSummary,
    },
    JobRemoved {
        #[serde(rename = "jobId")]
        job_id: String,
    },
    DraftSaved {
        revision: u64,
    },
    OperationFailed {
        operation: UiOperation,
        message: String,
    },
    Notice {
        tone: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum UiOperation {
    Preparation,
    Submission,
}

#[derive(Debug, Clone, Serialize)]
struct UiEventEnvelope {
    seq: u64,
    event: UiEvent,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenSessionResult {
    seq: u64,
    snapshot: AppSnapshot,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaybackGrant {
    grant_id: String,
    url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafetyHoldSummary {
    handle: String,
    provider_id: String,
    provider_name: String,
    recorded_at: String,
    message: String,
}

#[derive(Debug, Clone)]
enum MediaOrigin {
    Local(PathBuf),
    Remote(String),
}

#[derive(Debug, Clone)]
struct MediaGrant {
    origin: MediaOrigin,
    kind: MediaKind,
}

#[derive(Debug, Clone)]
struct JobGrant {
    key: ProviderJobKey,
    output_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct PlaybackGrantState {
    path: PathBuf,
    job_id: String,
}

#[derive(Debug, Clone)]
struct SafetyHoldGrant {
    provider_id: ProviderId,
    draft_fingerprint: String,
    summary: SafetyHoldSummary,
}

#[derive(Debug, Clone)]
struct PreservedDraftFields {
    provider_id: String,
    model_id: String,
    size: Option<String>,
    adapter_options: Option<serde_json::Value>,
    typed_seed: Option<i64>,
    editor_state: DraftEditorState,
}

impl PreservedDraftFields {
    fn from_core(draft: &CoreDraft, editor_state: DraftEditorState) -> Self {
        Self {
            provider_id: draft.provider_id.as_str().into(),
            model_id: draft.model.clone(),
            size: draft.size.clone(),
            adapter_options: draft.adapter_options.clone(),
            typed_seed: draft.seed,
            editor_state,
        }
    }

    fn matches(&self, draft: &GenerationDraft) -> bool {
        self.provider_id == draft.provider_id && self.model_id == draft.model_id
    }
}

#[derive(Debug, Clone, Copy)]
enum DraftPurpose {
    Review,
    Autosave,
}

struct Shared {
    seq: u64,
    opened: bool,
    snapshot: AppSnapshot,
    channels: Vec<Channel<UiEventEnvelope>>,
    media: HashMap<String, MediaGrant>,
    jobs: HashMap<String, JobGrant>,
    job_ids: HashMap<ProviderJobKey, String>,
    safety_holds: HashMap<String, SafetyHoldGrant>,
    safety_hold_ids: HashMap<(ProviderId, String), String>,
    pending_drafts: HashMap<u64, GenerationDraft>,
    pending_disclosures: HashMap<u64, String>,
    preparation_ops: HashMap<u64, u64>,
    submission_ops: HashSet<u64>,
    pending_save_ops: HashMap<u64, oneshot::Sender<Result<(), String>>>,
    pending_credential_ops: HashMap<u64, oneshot::Sender<Result<(), String>>>,
    pending_delete_ops: HashMap<u64, oneshot::Sender<Result<(), String>>>,
    preserved_draft: Option<PreservedDraftFields>,
    submitted_review: Option<PreparedReview>,
    playback_grants: HashMap<String, PlaybackGrantState>,
    deletion_pending: HashSet<String>,
    videos_dir: PathBuf,
    playback_dir: PathBuf,
    shutdown_requested: bool,
    shutdown_complete: bool,
    shutdown_retry_scheduled: bool,
    shutdown_block_notice_sent: bool,
}

impl Shared {
    fn new(videos_dir: PathBuf, playback_dir: PathBuf) -> Self {
        Self {
            seq: 0,
            opened: false,
            snapshot: AppSnapshot::default(),
            channels: Vec::new(),
            media: HashMap::new(),
            jobs: HashMap::new(),
            job_ids: HashMap::new(),
            safety_holds: HashMap::new(),
            safety_hold_ids: HashMap::new(),
            pending_drafts: HashMap::new(),
            pending_disclosures: HashMap::new(),
            preparation_ops: HashMap::new(),
            submission_ops: HashSet::new(),
            pending_save_ops: HashMap::new(),
            pending_credential_ops: HashMap::new(),
            pending_delete_ops: HashMap::new(),
            preserved_draft: None,
            submitted_review: None,
            playback_grants: HashMap::new(),
            deletion_pending: HashSet::new(),
            videos_dir,
            playback_dir,
            shutdown_requested: false,
            shutdown_complete: false,
            shutdown_retry_scheduled: false,
            shutdown_block_notice_sent: false,
        }
    }

    fn opaque_job_id(&mut self, key: &ProviderJobKey) -> String {
        if let Some(id) = self.job_ids.get(key) {
            return id.clone();
        }
        let id = format!("job-{}", Uuid::new_v4());
        self.job_ids.insert(key.clone(), id.clone());
        self.jobs.insert(
            id.clone(),
            JobGrant {
                key: key.clone(),
                output_path: None,
            },
        );
        id
    }

    fn upsert_job(&mut self, job: JobSummary, select: bool) -> bool {
        if let Some(index) = self.snapshot.jobs.iter().position(|item| item.id == job.id) {
            self.snapshot.jobs[index] = job;
            false
        } else {
            if select {
                self.snapshot.selected_job_id = Some(job.id.clone());
            }
            self.snapshot.jobs.insert(0, job);
            true
        }
    }

    fn refresh_safety_holds(&mut self) {
        let mut holds = self
            .safety_holds
            .values()
            .map(|grant| grant.summary.clone())
            .collect::<Vec<_>>();
        holds.sort_by(|left, right| right.recorded_at.cmp(&left.recorded_at));
        self.snapshot.safety_holds = holds;
    }

    fn upsert_safety_hold(&mut self, record: &UncertainSubmissionRecord) -> SafetyHoldSummary {
        let key = (record.provider_id.clone(), record.draft_fingerprint.clone());
        let handle = self
            .safety_hold_ids
            .entry(key)
            .or_insert_with(|| format!("hold-{}", Uuid::new_v4()))
            .clone();
        let summary = SafetyHoldSummary {
            handle: handle.clone(),
            provider_id: record.provider_id.as_str().into(),
            provider_name: provider_name(&record.provider_id).into(),
            recorded_at: record.recorded_at.to_rfc3339(),
            message: self.ui_safe_message(&record.message),
        };
        self.safety_holds.insert(
            handle,
            SafetyHoldGrant {
                provider_id: record.provider_id.clone(),
                draft_fingerprint: record.draft_fingerprint.clone(),
                summary: summary.clone(),
            },
        );
        self.refresh_safety_holds();
        summary
    }

    fn remove_safety_hold(&mut self, provider_id: &ProviderId, fingerprint: &str) {
        let key = (provider_id.clone(), fingerprint.to_owned());
        if let Some(handle) = self.safety_hold_ids.remove(&key) {
            self.safety_holds.remove(&handle);
            self.refresh_safety_holds();
        }
    }

    fn fail_operation(&mut self, op_id: u64) -> Option<UiOperation> {
        if self.preparation_ops.remove(&op_id).is_some() {
            self.pending_drafts.remove(&op_id);
            self.pending_disclosures.remove(&op_id);
            return Some(UiOperation::Preparation);
        }
        if self.submission_ops.remove(&op_id) {
            // A prepared token is single-use inside the core, including when
            // the provider call becomes uncertain or a later safety check
            // fails. Never leave a retry-looking Review in the renderer.
            self.snapshot.prepared_review = None;
            self.submitted_review = None;
            return Some(UiOperation::Submission);
        }
        None
    }

    fn ui_safe_message(&self, value: &str) -> String {
        let mut paths = vec![
            self.videos_dir.to_string_lossy().into_owned(),
            self.playback_dir.to_string_lossy().into_owned(),
        ];
        paths.extend(self.media.values().filter_map(|grant| match &grant.origin {
            MediaOrigin::Local(path) => Some(path.to_string_lossy().into_owned()),
            MediaOrigin::Remote(_) => None,
        }));
        paths.extend(
            self.jobs
                .values()
                .filter_map(|grant| grant.output_path.as_ref())
                .map(|path| path.to_string_lossy().into_owned()),
        );
        paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
        paths.dedup();

        let mut redacted = value.to_owned();
        for path in paths {
            if !path.is_empty() {
                redacted = redacted.replace(&path, "[local path]");
            }
        }
        safe_message(&redacted)
    }
}

struct DesktopState {
    commands: mpsc::Sender<ServiceCommand>,
    shared: Arc<Mutex<Shared>>,
    _runtime: Runtime,
}

impl Drop for DesktopState {
    fn drop(&mut self) {
        let _ = self.commands.try_send(ServiceCommand::Shutdown);
    }
}

fn provider_summary(id: &str) -> ProviderSummary {
    match id {
        "fal" => ProviderSummary {
            id: id.into(),
            name: "fal.ai".into(),
            short_name: "fal".into(),
            connected: false,
            credential_storage: "none".into(),
            account_label: None,
            description: "Video models, plus a bridge for local references.".into(),
            local_media_note: "Local files upload only in Review, as public-by-link files with a requested 24-hour expiry.".into(),
        },
        _ => ProviderSummary {
            id: "openrouter".into(),
            name: "OpenRouter".into(),
            short_name: "OpenRouter".into(),
            connected: false,
            credential_storage: "none".into(),
            account_label: None,
            description: "A front door to video models from many labs.".into(),
            local_media_note: "OpenRouter takes public links. Local files can travel through fal.ai after you approve the upload.".into(),
        },
    }
}

fn checked_provider(value: &str) -> Result<ProviderId, String> {
    match value {
        "openrouter" => Ok(ProviderId::openrouter()),
        "fal" => Ok(ProviderId::fal()),
        _ => Err("That provider is not supported by this build.".into()),
    }
}

fn provider_name(id: &ProviderId) -> &'static str {
    if id == &ProviderId::fal() {
        "fal.ai"
    } else {
        "OpenRouter"
    }
}

fn credential_storage(connected: bool, status: &CredentialStatus) -> String {
    if !connected {
        "none"
    } else if status.persistent {
        "keyring"
    } else {
        "memory"
    }
    .into()
}

fn update_provider(
    shared: &mut Shared,
    provider_id: &ProviderId,
    connected: bool,
    status: &CredentialStatus,
    account: Option<&ProviderAccount>,
) -> ProviderSummary {
    let id = provider_id.as_str();
    let mut summary = shared
        .snapshot
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .cloned()
        .unwrap_or_else(|| provider_summary(id));
    summary.connected = connected;
    summary.credential_storage = credential_storage(connected, status);
    summary.account_label = connected
        .then(|| account.map(|info| info.label.clone()))
        .flatten();
    if let Some(index) = shared
        .snapshot
        .providers
        .iter()
        .position(|item| item.id == id)
    {
        shared.snapshot.providers[index] = summary.clone();
    } else {
        shared.snapshot.providers.push(summary.clone());
    }
    summary
}

fn model_summary(model: &VideoModel) -> ModelSummary {
    ModelSummary {
        id: model.id.clone(),
        provider_id: model.provider_id.as_str().into(),
        name: model.name.clone(),
        description: model.description.clone(),
        capabilities: ModelCapabilities {
            images: model.supports_media_kind(CoreMediaKind::Image),
            video: model.supports_media_kind(CoreMediaKind::Video),
            audio_references: model.supports_media_kind(CoreMediaKind::Audio),
            generated_audio: model.generated_audio.supported,
        },
        duration_options: model
            .supported_durations
            .iter()
            .map(ToString::to_string)
            .collect(),
        resolution_options: model.supported_resolutions.clone(),
        aspect_ratio_options: model.supported_aspect_ratios.clone(),
        price_hint: if model.pricing_skus.is_empty() {
            "Price checked in Review".into()
        } else {
            "Fresh price in Review".into()
        },
    }
}

fn apply_catalog(shared: &mut Shared, catalog: &VideoCatalog) {
    let provider_id = catalog.provider_id.as_str();
    shared
        .snapshot
        .models
        .retain(|model| model.provider_id != provider_id);
    shared
        .snapshot
        .models
        .extend(catalog.models.iter().map(model_summary));
}

fn media_role(kind: MediaKind, requested: MediaRole) -> Result<(CoreMediaRole, MediaRole), String> {
    match kind {
        MediaKind::Video => Ok((CoreMediaRole::VideoInput, MediaRole::VideoReference)),
        MediaKind::Audio => Ok((CoreMediaRole::AudioInput, MediaRole::AudioReference)),
        MediaKind::Image => match requested {
            MediaRole::Reference => Ok((CoreMediaRole::Reference, requested)),
            MediaRole::StartFrame => Ok((CoreMediaRole::StartFrame, requested)),
            MediaRole::EndFrame => Ok((CoreMediaRole::EndFrame, requested)),
            MediaRole::VideoReference | MediaRole::AudioReference => {
                Err("Image media must use an image role.".into())
            }
        },
    }
}

fn media_kind_for_path(path: &Path) -> Result<MediaKind, String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "avif" | "bmp" | "tif" | "tiff" => {
            Ok(MediaKind::Image)
        }
        "mp4" | "mov" => Ok(MediaKind::Video),
        "mp3" | "wav" => Ok(MediaKind::Audio),
        _ => Err("Choose a supported image, MP4/MOV video, or MP3/WAV audio file.".into()),
    }
}

fn normalized_role(kind: MediaKind) -> MediaRole {
    match kind {
        MediaKind::Image => MediaRole::Reference,
        MediaKind::Video => MediaRole::VideoReference,
        MediaKind::Audio => MediaRole::AudioReference,
    }
}

fn local_media_item(shared: &mut Shared, path: PathBuf) -> Result<MediaItem, String> {
    let canonical = path
        .canonicalize()
        .map_err(|_| "That media file is no longer available.".to_string())?;
    let kind = media_kind_for_path(&canonical)?;
    let role = normalized_role(kind);
    DraftMedia::local(canonical.clone(), media_role(kind, role)?.0)
        .validate()
        .map_err(|_| {
            "That local media file is unavailable or does not match its selected format."
                .to_string()
        })?;
    let display_name = canonical
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("Selected media")
        .to_owned();
    let handle = format!("media-{}", Uuid::new_v4());
    shared.media.insert(
        handle.clone(),
        MediaGrant {
            origin: MediaOrigin::Local(canonical),
            kind,
        },
    );
    Ok(MediaItem {
        handle,
        display_name,
        kind,
        role,
        source: "local".into(),
        detail: format!("Local {}", kind.label()),
        preview_url: None,
    })
}

fn remote_media_item(
    shared: &mut Shared,
    url: String,
    kind: MediaKind,
    role: MediaRole,
) -> Result<MediaItem, String> {
    let (core_role, role) = media_role(kind, role)?;
    DraftMedia::remote(url.clone(), core_role)
        .and_then(|media| media.validate())
        .map_err(|error| error.to_string())?;
    let parsed = tauri::Url::parse(&url).map_err(|_| "Enter a valid public HTTPS URL.")?;
    let host = parsed.host_str().unwrap_or("Remote media").to_owned();
    let display_name = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|value| !value.is_empty())
        .unwrap_or(&host)
        .to_owned();
    let handle = format!("media-{}", Uuid::new_v4());
    shared.media.insert(
        handle.clone(),
        MediaGrant {
            origin: MediaOrigin::Remote(url),
            kind,
        },
    );
    Ok(MediaItem {
        handle,
        display_name,
        kind,
        role,
        source: "remote".into(),
        detail: host,
        preview_url: None,
    })
}

fn core_media_item(shared: &mut Shared, media: &DraftMedia) -> Result<MediaItem, String> {
    let kind = match media.role {
        CoreMediaRole::VideoInput => MediaKind::Video,
        CoreMediaRole::AudioInput => MediaKind::Audio,
        CoreMediaRole::StartFrame | CoreMediaRole::EndFrame | CoreMediaRole::Reference => {
            MediaKind::Image
        }
    };
    let role = match media.role {
        CoreMediaRole::StartFrame => MediaRole::StartFrame,
        CoreMediaRole::EndFrame => MediaRole::EndFrame,
        CoreMediaRole::VideoInput => MediaRole::VideoReference,
        CoreMediaRole::AudioInput => MediaRole::AudioReference,
        CoreMediaRole::Reference => MediaRole::Reference,
    };
    match &media.source {
        MediaSource::LocalFile { path } => {
            let canonical = path
                .canonicalize()
                .map_err(|_| "A saved local media file is no longer available.".to_string())?;
            media.validate().map_err(|_| {
                "A saved local media item is unavailable or invalid; choose it again.".to_string()
            })?;
            let handle = format!("media-{}", Uuid::new_v4());
            let display_name = canonical
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("Saved media")
                .to_owned();
            shared.media.insert(
                handle.clone(),
                MediaGrant {
                    origin: MediaOrigin::Local(canonical),
                    kind,
                },
            );
            Ok(MediaItem {
                handle,
                display_name,
                kind,
                role,
                source: "local".into(),
                detail: format!("Local {}", kind.label()),
                preview_url: None,
            })
        }
        MediaSource::RemoteUrl { url } => remote_media_item(shared, url.clone(), kind, role),
    }
}

fn core_draft(
    shared: &Shared,
    draft: &GenerationDraft,
    purpose: DraftPurpose,
) -> Result<CoreDraft, String> {
    if draft.prompt.len() > 100_000 || draft.model_id.len() > 256 || draft.media.len() > 32 {
        return Err("The draft is larger than this build supports.".into());
    }
    for value in [
        &draft.settings.duration,
        &draft.settings.resolution,
        &draft.settings.aspect_ratio,
        &draft.settings.seed,
    ] {
        if value.len() > 256 || value.chars().any(char::is_control) {
            return Err("A generation setting contains an invalid value.".into());
        }
    }
    let provider_id = checked_provider(&draft.provider_id)?;
    let duration = if draft.settings.duration.trim().is_empty() {
        None
    } else {
        Some(
            draft
                .settings
                .duration
                .trim()
                .parse::<u32>()
                .map_err(|_| "Duration must be a whole number of seconds.")?,
        )
    };
    let preserved = shared
        .preserved_draft
        .as_ref()
        .filter(|fields| fields.matches(draft));
    let seed = if draft.settings.seed.trim().is_empty() {
        None
    } else {
        match draft.settings.seed.trim().parse::<i64>() {
            Ok(seed) => Some(seed),
            Err(_) if matches!(purpose, DraftPurpose::Autosave) => {
                preserved.and_then(|fields| fields.typed_seed)
            }
            Err(_) => return Err("Seed must be a whole number.".into()),
        }
    };
    let generate_audio = match draft.settings.generated_audio.as_str() {
        "provider_default" => None,
        "on" => Some(true),
        "off" => Some(false),
        _ => return Err("Generated audio has an invalid value.".into()),
    };
    let mut media = Vec::with_capacity(draft.media.len());
    for item in &draft.media {
        let grant = shared
            .media
            .get(&item.handle)
            .ok_or_else(|| "A media selection expired; choose it again.".to_string())?;
        let (role, _) = media_role(grant.kind, item.role)?;
        let (value, is_local) = match &grant.origin {
            MediaOrigin::Local(path) => (DraftMedia::local(path.clone(), role), true),
            MediaOrigin::Remote(url) => (
                DraftMedia::remote(url.clone(), role)
                    .map_err(|_| "Enter a valid public HTTPS reference URL.".to_string())?,
                false,
            ),
        };
        value.validate().map_err(|_| {
            if is_local {
                "A local media item is unavailable or no longer matches its selected format."
                    .to_string()
            } else {
                "Enter a valid public HTTPS reference URL.".to_string()
            }
        })?;
        media.push(value);
    }
    Ok(CoreDraft {
        provider_id,
        model: draft.model_id.clone(),
        prompt: draft.prompt.clone(),
        duration,
        resolution: nonempty(&draft.settings.resolution),
        aspect_ratio: nonempty(&draft.settings.aspect_ratio),
        size: preserved.and_then(|fields| fields.size.clone()),
        generate_audio,
        seed,
        media,
        adapter_options: preserved.and_then(|fields| fields.adapter_options.clone()),
    })
}

fn editor_state_for_draft(shared: &Shared, draft: &GenerationDraft) -> DraftEditorState {
    let mut editor = shared
        .preserved_draft
        .as_ref()
        .filter(|fields| fields.matches(draft))
        .map(|fields| fields.editor_state.clone())
        .unwrap_or_default();
    editor.seed_text = draft.settings.seed.clone();
    editor
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn settings_from_core(draft: &CoreDraft) -> GenerationSettings {
    GenerationSettings {
        duration: draft
            .duration
            .map(|value| value.to_string())
            .unwrap_or_default(),
        resolution: draft.resolution.clone().unwrap_or_default(),
        aspect_ratio: draft.aspect_ratio.clone().unwrap_or_default(),
        generated_audio: match draft.generate_audio {
            None => "provider_default",
            Some(true) => "on",
            Some(false) => "off",
        }
        .into(),
        seed: draft
            .seed
            .map(|value| value.to_string())
            .unwrap_or_default(),
    }
}

fn ui_draft_from_core(
    shared: &mut Shared,
    draft: &CoreDraft,
    revision: u64,
) -> Result<GenerationDraft, String> {
    let media = draft
        .media
        .iter()
        .map(|item| core_media_item(shared, item))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GenerationDraft {
        revision,
        provider_id: draft.provider_id.as_str().into(),
        model_id: draft.model.clone(),
        prompt: draft.prompt.clone(),
        media,
        settings: settings_from_core(draft),
    })
}

fn settings_from_request(request: &VideoRequest) -> GenerationSettings {
    GenerationSettings {
        duration: request
            .duration
            .map(|value| value.to_string())
            .unwrap_or_default(),
        resolution: request.resolution.clone().unwrap_or_default(),
        aspect_ratio: request.aspect_ratio.clone().unwrap_or_default(),
        generated_audio: match request.generate_audio {
            None => "provider_default",
            Some(true) => "on",
            Some(false) => "off",
        }
        .into(),
        seed: request
            .seed
            .map(|value| value.to_string())
            .unwrap_or_default(),
    }
}

fn quote_label(quote: &CostQuote) -> String {
    quote
        .amount
        .map(|amount| format!("{} {amount}", quote.currency))
        .unwrap_or_else(|| "Provider estimate unavailable".into())
}

fn advanced_settings_json(request: &VideoRequest) -> Option<String> {
    let mut settings = serde_json::Map::new();
    if let Some(size) = &request.size {
        settings.insert("exact_size".into(), serde_json::Value::String(size.clone()));
    }
    if let Some(adapter_options) = &request.adapter_options {
        settings.insert("provider_specific".into(), adapter_options.clone());
    }
    (!settings.is_empty())
        .then(|| serde_json::to_string_pretty(&settings).ok())
        .flatten()
}

struct ReviewEvent<'a> {
    op_id: u64,
    prepared_id: PreparedGenerationId,
    revision: u64,
    provider_id: &'a ProviderId,
    request: &'a VideoRequest,
    quote: &'a CostQuote,
    expires_at: String,
}

fn review_from_event(shared: &mut Shared, event: ReviewEvent<'_>) -> PreparedReview {
    let draft = shared
        .pending_drafts
        .remove(&event.op_id)
        .unwrap_or_else(|| GenerationDraft {
            revision: event.revision,
            provider_id: event.provider_id.as_str().into(),
            model_id: event.request.model.clone(),
            prompt: event.request.prompt.clone(),
            media: Vec::new(),
            settings: settings_from_request(event.request),
        });
    let model_name = shared
        .snapshot
        .models
        .iter()
        .find(|model| {
            model.provider_id == event.provider_id.as_str() && model.id == event.request.model
        })
        .map(|model| model.name.clone())
        .unwrap_or_else(|| event.request.model.clone());
    PreparedReview {
        prepared_id: event.prepared_id.0,
        revision: event.revision,
        provider_id: event.provider_id.as_str().into(),
        provider_name: provider_name(event.provider_id).into(),
        model_id: event.request.model.clone(),
        model_name,
        prompt: event.request.prompt.clone(),
        settings: draft.settings,
        media: draft.media,
        estimated_cost: quote_label(event.quote),
        expires_at: event.expires_at,
        upload_disclosure: shared.pending_disclosures.remove(&event.op_id),
        advanced_settings_json: advanced_settings_json(event.request),
    }
}

fn status_projection(status: &CoreJobStatus, has_output: bool) -> (String, String, String) {
    match status {
        CoreJobStatus::Pending => (
            "queued".into(),
            "Waiting in line".into(),
            "Your render is queued with the provider.".into(),
        ),
        CoreJobStatus::InProgress => (
            "processing".into(),
            "Making your video".into(),
            "Tiny Cloud Cinema is keeping watch while the model works.".into(),
        ),
        CoreJobStatus::Completed if has_output => (
            "completed".into(),
            "Ready to watch".into(),
            "Your finished video is waiting in the Videos folder.".into(),
        ),
        CoreJobStatus::Completed => (
            "downloading".into(),
            "Saving the final cut".into(),
            "The render is done. Video Harness is tucking it into your Videos folder.".into(),
        ),
        CoreJobStatus::Failed => (
            "attention".into(),
            "Render needs attention".into(),
            "The provider couldn’t finish this one.".into(),
        ),
        CoreJobStatus::Cancelled => (
            "attention".into(),
            "Render cancelled".into(),
            "This render was cancelled.".into(),
        ),
        CoreJobStatus::Expired => (
            "attention".into(),
            "Render expired".into(),
            "The provider no longer has this render.".into(),
        ),
        CoreJobStatus::Unknown(_) => (
            "processing".into(),
            "Checking the provider".into(),
            "The provider sent an unfamiliar status, so we’re keeping watch.".into(),
        ),
    }
}

fn redact_url_secrets(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let tail = &value[cursor..];
        let bytes = tail.as_bytes();
        let next_http = bytes
            .windows(b"http://".len())
            .position(|window| window.eq_ignore_ascii_case(b"http://"));
        let next_https = bytes
            .windows(b"https://".len())
            .position(|window| window.eq_ignore_ascii_case(b"https://"));
        let Some(relative_start) = (match (next_http, next_https) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(start), None) | (None, Some(start)) => Some(start),
            (None, None) => None,
        }) else {
            output.push_str(tail);
            break;
        };
        let start = cursor + relative_start;
        output.push_str(&value[cursor..start]);
        let url_tail = &value[start..];
        let end = url_tail
            .char_indices()
            .find_map(|(index, character)| {
                (character.is_whitespace()
                    || character.is_control()
                    || matches!(character, '"' | '\'' | '<' | '>'))
                .then_some(index)
            })
            .unwrap_or(url_tail.len());
        let token = &url_tail[..end];
        let public_end = token.find(['?', '#']).unwrap_or(token.len());
        output.push_str(&token[..public_end]);
        cursor = start + end;
    }
    output
}

fn redact_secret_prefix(value: &str, prefix: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(relative_start) = value[cursor..].find(prefix) {
        let start = cursor + relative_start;
        output.push_str(&value[cursor..start]);
        let secret_tail = &value[start..];
        let end = secret_tail
            .char_indices()
            .find_map(|(index, character)| {
                (index > 0
                    && (character.is_whitespace()
                        || matches!(
                            character,
                            ',' | ';'
                                | ':'
                                | '"'
                                | '\''
                                | '('
                                | ')'
                                | '['
                                | ']'
                                | '{'
                                | '}'
                                | '<'
                                | '>'
                        )))
                .then_some(index)
            })
            .unwrap_or(secret_tail.len());
        output.push_str("[redacted credential]");
        cursor = start + end;
    }
    output.push_str(&value[cursor..]);
    output
}

fn safe_message(value: &str) -> String {
    let without_controls: String = value
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .collect();
    let without_url_secrets = redact_url_secrets(&without_controls);
    let without_openrouter_keys = redact_secret_prefix(&without_url_secrets, "sk-or-v1-");
    redact_secret_prefix(&without_openrouter_keys, "fal_key_")
        .chars()
        .take(2_000)
        .collect()
}

fn safe_shared_message(shared: &Arc<Mutex<Shared>>, value: &str) -> String {
    shared
        .lock()
        .map(|state| state.ui_safe_message(value))
        .unwrap_or_else(|_| safe_message(value))
}

fn displayable_remote_job_id(value: &str) -> Option<String> {
    let value = value.split(['?', '#']).next().unwrap_or_default().trim();
    let valid = !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));
    valid.then(|| value.to_owned())
}

fn job_from_record(shared: &mut Shared, record: &JobRecord) -> JobSummary {
    let key = record.key();
    let id = shared.opaque_job_id(&key);
    let output_path = record.output_path.clone();
    if let Some(grant) = shared.jobs.get_mut(&id) {
        grant.output_path = output_path.clone();
    }
    let status = CoreJobStatus::from_raw(record.status.clone());
    let (status, status_label, mut detail) = status_projection(&status, output_path.is_some());
    if let Some(error) = &record.error {
        detail = shared.ui_safe_message(error);
    }
    let request = record.request.as_ref();
    let model_id = request
        .map(|value| value.model.as_str())
        .unwrap_or("Unknown model");
    let model_name = shared
        .snapshot
        .models
        .iter()
        .find(|model| model.provider_id == record.provider_id.as_str() && model.id == model_id)
        .map(|model| model.name.clone())
        .unwrap_or_else(|| model_id.to_owned());
    JobSummary {
        id,
        provider_id: record.provider_id.as_str().into(),
        provider_name: provider_name(&record.provider_id).into(),
        model_name,
        prompt: request
            .map(|value| value.prompt.clone())
            .unwrap_or_default(),
        status,
        status_label,
        detail,
        created_at: record.created_at.to_rfc3339(),
        elapsed_seconds: None,
        next_poll_seconds: None,
        progress: None,
        output_file_name: output_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            .map(ToOwned::to_owned),
        has_local_output: output_path.is_some(),
        playback_url: None,
        captions_url: None,
        remote_continues: None,
        provider_job_id: displayable_remote_job_id(record.remote_id()),
        deletable: record.terminal(),
    }
}

fn job_from_acceptance(shared: &mut Shared, job: &VideoJob) -> JobSummary {
    let key = job.key();
    let id = shared.opaque_job_id(&key);
    let review = shared.submitted_review.clone();
    let (status, status_label, mut detail) = status_projection(&job.status, false);
    if let Some(error) = &job.error {
        detail = shared.ui_safe_message(error);
    }
    JobSummary {
        id,
        provider_id: job.provider_id.as_str().into(),
        provider_name: provider_name(&job.provider_id).into(),
        model_name: review
            .as_ref()
            .map(|value| value.model_name.clone())
            .unwrap_or_else(|| "Video model".into()),
        prompt: review
            .as_ref()
            .map(|value| value.prompt.clone())
            .unwrap_or_default(),
        status,
        status_label,
        detail,
        created_at: Utc::now().to_rfc3339(),
        elapsed_seconds: Some(0),
        next_poll_seconds: None,
        progress: None,
        output_file_name: None,
        has_local_output: false,
        playback_url: None,
        captions_url: None,
        remote_continues: None,
        provider_job_id: displayable_remote_job_id(&key.remote_job_id),
        // A record-less acceptance has no durable history row to remove yet.
        deletable: false,
    }
}

fn broadcast(shared: &Arc<Mutex<Shared>>, event: UiEvent) {
    let (envelope, channels) = {
        let mut guard = match shared.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        guard.seq = guard.seq.saturating_add(1);
        (
            UiEventEnvelope {
                seq: guard.seq,
                event,
            },
            guard.channels.clone(),
        )
    };
    for channel in channels {
        let _ = channel.send(envelope.clone());
    }
}

fn mutate_and_broadcast<F>(shared: &Arc<Mutex<Shared>>, update: F)
where
    F: FnOnce(&mut Shared) -> Option<UiEvent>,
{
    let event = shared.lock().ok().and_then(|mut guard| update(&mut guard));
    if let Some(event) = event {
        broadcast(shared, event);
    }
}

async fn service_events(
    mut events: mpsc::UnboundedReceiver<ServiceEvent>,
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::Sender<ServiceCommand>,
    app: AppHandle,
) {
    while let Some(event) = events.recv().await {
        match event {
            ServiceEvent::Ready {
                providers,
                default_provider,
            } => mutate_and_broadcast(&shared, |state| {
                for ProviderConnection {
                    descriptor,
                    connected,
                    credential_status,
                } in &providers
                {
                    update_provider(state, &descriptor.id, *connected, credential_status, None);
                }
                if state.snapshot.draft.revision == 0 && state.snapshot.draft.model_id.is_empty() {
                    state.snapshot.draft.provider_id = default_provider.as_str().into();
                }
                Some(UiEvent::SnapshotChanged {
                    snapshot: Box::new(state.snapshot.clone()),
                })
            }),
            ServiceEvent::ApiKeyConnected {
                op_id,
                provider_id,
                info,
                credential_status,
                ..
            } => {
                if let Some(reply) = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.pending_credential_ops.remove(&op_id))
                {
                    let _ = reply.send(Ok(()));
                }
                mutate_and_broadcast(&shared, |state| {
                    let provider =
                        update_provider(state, &provider_id, true, &credential_status, Some(&info));
                    Some(UiEvent::ProviderChanged { provider })
                });
            }
            ServiceEvent::ApiKeyForgotten {
                op_id,
                provider_id,
                credential_status,
                ..
            } => {
                if let Some(reply) = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.pending_credential_ops.remove(&op_id))
                {
                    let _ = reply.send(Ok(()));
                }
                mutate_and_broadcast(&shared, |state| {
                    let provider =
                        update_provider(state, &provider_id, false, &credential_status, None);
                    state
                        .snapshot
                        .models
                        .retain(|model| model.provider_id != provider_id.as_str());
                    Some(UiEvent::ProviderChanged { provider })
                });
            }
            ServiceEvent::CatalogLoaded { catalog, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    apply_catalog(state, &catalog);
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
            }
            ServiceEvent::DraftLoaded {
                draft,
                editor_state,
                revision,
                ..
            } => mutate_and_broadcast(&shared, |state| {
                if let Some(draft) = draft {
                    let editor_state = editor_state.unwrap_or_else(|| DraftEditorState {
                        seed_text: draft.seed.map(|seed| seed.to_string()).unwrap_or_default(),
                        ..DraftEditorState::default()
                    });
                    match ui_draft_from_core(state, &draft, revision.unwrap_or_default()) {
                        Ok(mut ui_draft) => {
                            ui_draft.settings.seed = editor_state.seed_text.clone();
                            state.preserved_draft =
                                Some(PreservedDraftFields::from_core(&draft, editor_state));
                            state.snapshot.draft = ui_draft;
                        }
                        Err(message) => {
                            return Some(UiEvent::Notice {
                                tone: "warning".into(),
                                message,
                            });
                        }
                    }
                }
                state.snapshot.draft_saved = true;
                Some(UiEvent::SnapshotChanged {
                    snapshot: Box::new(state.snapshot.clone()),
                })
            }),
            ServiceEvent::DraftSaved {
                op_id, revision, ..
            } => {
                if let Some(reply) = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.pending_save_ops.remove(&op_id))
                {
                    let _ = reply.send(Ok(()));
                }
                mutate_and_broadcast(&shared, |state| {
                    state.snapshot.draft_saved = state.snapshot.draft.revision == revision;
                    Some(UiEvent::DraftSaved { revision })
                });
            }
            ServiceEvent::ReviewReady {
                op_id,
                prepared_id,
                revision,
                provider_id,
                request,
                quote,
                expires_at,
                ..
            } => mutate_and_broadcast(&shared, |state| {
                state.preparation_ops.remove(&op_id);
                let review = review_from_event(
                    state,
                    ReviewEvent {
                        op_id,
                        prepared_id,
                        revision,
                        provider_id: &provider_id,
                        request: &request,
                        quote: &quote,
                        expires_at: expires_at.to_rfc3339(),
                    },
                );
                state.snapshot.prepared_review = Some(review.clone());
                Some(UiEvent::ReviewReady { review })
            }),
            ServiceEvent::PreparedInvalidated {
                op_id,
                prepared_id,
                revision,
            } => {
                mutate_and_broadcast(&shared, |state| {
                    let replacement_is_still_preparing =
                        prepared_id.is_some() && state.preparation_ops.contains_key(&op_id);
                    state.snapshot.prepared_review = None;
                    if replacement_is_still_preparing {
                        return Some(UiEvent::SnapshotChanged {
                            snapshot: Box::new(state.snapshot.clone()),
                        });
                    }
                    if state.preparation_ops.remove(&op_id).is_some() {
                        state.pending_drafts.remove(&op_id);
                        state.pending_disclosures.remove(&op_id);
                    }
                    Some(UiEvent::ReviewInvalidated { revision })
                });
            }
            ServiceEvent::HistoryLoaded { records, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    for record in &records {
                        let job = job_from_record(state, record);
                        state.upsert_job(job, false);
                    }
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
            }
            ServiceEvent::GenerationDeleted {
                op_id,
                key,
                output_deleted: _,
            } => {
                let removed_id = shared.lock().ok().and_then(|mut state| {
                    let id = state.job_ids.remove(&key)?;
                    state.jobs.remove(&id);
                    state.deletion_pending.remove(&id);
                    state.snapshot.jobs.retain(|job| job.id != id);
                    if state.snapshot.selected_job_id.as_deref() == Some(id.as_str()) {
                        state.snapshot.selected_job_id =
                            state.snapshot.jobs.first().map(|job| job.id.clone());
                    }
                    Some(id)
                });
                if let Some(job_id) = removed_id {
                    broadcast(&shared, UiEvent::JobRemoved { job_id });
                }
                if let Some(reply) = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.pending_delete_ops.remove(&op_id))
                {
                    let _ = reply.send(Ok(()));
                }
            }
            ServiceEvent::JobAccepted {
                op_id, job, record, ..
            } => {
                mutate_and_broadcast(&shared, |state| {
                    state.submission_ops.remove(&op_id);
                    let job = record
                        .as_ref()
                        .map(|record| job_from_record(state, record))
                        .unwrap_or_else(|| job_from_acceptance(state, &job));
                    state.snapshot.prepared_review = None;
                    let added = state.upsert_job(job.clone(), true);
                    Some(if added {
                        UiEvent::JobAdded { job }
                    } else {
                        UiEvent::JobUpdated { job }
                    })
                });
            }
            ServiceEvent::Imported { record, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    let job = job_from_record(state, &record);
                    state.snapshot.prepared_review = None;
                    let added = state.upsert_job(job.clone(), true);
                    Some(if added {
                        UiEvent::JobAdded { job }
                    } else {
                        UiEvent::JobUpdated { job }
                    })
                });
            }
            ServiceEvent::JobUpdated { record, .. } | ServiceEvent::Downloaded { record, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    let job = job_from_record(state, &record);
                    let added = state.upsert_job(job.clone(), false);
                    Some(if added {
                        UiEvent::JobAdded { job }
                    } else {
                        UiEvent::JobUpdated { job }
                    })
                });
            }
            ServiceEvent::PollWaiting {
                provider_id,
                job_id,
                next_in,
                ..
            } => mutate_and_broadcast(&shared, |state| {
                let key = ProviderJobKey::new(provider_id, job_id).ok()?;
                let id = state.job_ids.get(&key)?.clone();
                let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                let mut job = state.snapshot.jobs[index].clone();
                job.status = "processing".into();
                job.status_label = "Generating video".into();
                job.next_poll_seconds = Some(next_in.as_secs());
                job.remote_continues = None;
                state.snapshot.jobs[index] = job.clone();
                Some(UiEvent::JobUpdated { job })
            }),
            ServiceEvent::DownloadProgress {
                provider_id,
                job_id,
                written,
                total,
                ..
            } => mutate_and_broadcast(&shared, |state| {
                let key = ProviderJobKey::new(provider_id, job_id).ok()?;
                let id = state.job_ids.get(&key)?.clone();
                let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                let mut job = state.snapshot.jobs[index].clone();
                job.status = "downloading".into();
                job.status_label = "Saving finished video".into();
                job.detail = "The output is being written to your Videos folder.".into();
                job.progress = total
                    .filter(|total| *total > 0)
                    .map(|total| written as f64 / total as f64);
                state.snapshot.jobs[index] = job.clone();
                Some(UiEvent::JobUpdated { job })
            }),
            ServiceEvent::MonitorPaused {
                key,
                remote_continues,
                ..
            } => mutate_and_broadcast(&shared, |state| {
                let id = state.job_ids.get(&key)?.clone();
                let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                let mut job = state.snapshot.jobs[index].clone();
                job.status = "paused".into();
                job.status_label = "Monitoring paused".into();
                job.detail =
                    "The provider job continues remotely; only local checks are paused.".into();
                job.remote_continues = Some(remote_continues);
                job.next_poll_seconds = None;
                state.snapshot.jobs[index] = job.clone();
                Some(UiEvent::JobUpdated { job })
            }),
            ServiceEvent::PreparationStarted { media_count, .. } => {
                broadcast(
                    &shared,
                    UiEvent::Notice {
                        tone: "neutral".into(),
                        message: if media_count == 0 {
                            "Checking the current provider quote.".into()
                        } else {
                            format!("Preparing {media_count} reference item(s) for Review.")
                        },
                    },
                );
            }
            ServiceEvent::MediaUploadProgress {
                media_index,
                sent,
                total,
                ..
            } => {
                let percent = sent.saturating_mul(100).checked_div(total).unwrap_or(0);
                broadcast(
                    &shared,
                    UiEvent::Notice {
                        tone: "neutral".into(),
                        message: format!("Uploading reference {} — {percent}%", media_index + 1),
                    },
                );
            }
            ServiceEvent::SubmissionStarted { .. } => broadcast(
                &shared,
                UiEvent::Notice {
                    tone: "neutral".into(),
                    message: "Submitting the reviewed generation once.".into(),
                },
            ),
            ServiceEvent::JobRecoveryFailed { key, message, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    let Some(id) = state.job_ids.get(&key).cloned() else {
                        return Some(UiEvent::Notice {
                            tone: "danger".into(),
                            message: state.ui_safe_message(&message),
                        });
                    };
                    let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                    let mut job = state.snapshot.jobs[index].clone();
                    job.status = "attention".into();
                    job.status_label = "Remote job needs attention".into();
                    job.detail = state.ui_safe_message(&message);
                    job.remote_continues = Some(true);
                    state.snapshot.jobs[index] = job.clone();
                    Some(UiEvent::JobUpdated { job })
                });
            }
            ServiceEvent::SubmissionUncertain { op_id, message, .. } => {
                let message = safe_shared_message(&shared, &message);
                let operation = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.fail_operation(op_id))
                    .unwrap_or(UiOperation::Submission);
                broadcast(&shared, UiEvent::OperationFailed { operation, message });
            }
            ServiceEvent::UncertainSubmissionsLoaded { records, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    for record in &records {
                        state.upsert_safety_hold(record);
                    }
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
            }
            ServiceEvent::UncertainSubmissionSaved { record, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    state.upsert_safety_hold(&record);
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
            }
            ServiceEvent::UncertainSubmissionCleared {
                provider_id,
                draft_fingerprint,
                removed,
                ..
            } => {
                mutate_and_broadcast(&shared, |state| {
                    if removed {
                        state.remove_safety_hold(&provider_id, &draft_fingerprint);
                    }
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
            }
            ServiceEvent::UncertainSubmissionBlocked { op_id, record } => {
                mutate_and_broadcast(&shared, |state| {
                    state.upsert_safety_hold(&record);
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
                let operation = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.fail_operation(op_id))
                    .unwrap_or(UiOperation::Preparation);
                broadcast(
                    &shared,
                    UiEvent::OperationFailed {
                        operation,
                        message: "This exact draft may already have been submitted. Check the provider dashboard, then acknowledge its safety hold in Providers & Settings."
                            .into(),
                    },
                );
            }
            ServiceEvent::Error {
                op_id,
                provider_id,
                message,
                recoverable,
                job_id,
                ..
            } => {
                let message = safe_shared_message(&shared, &message);
                if let Some(reply) = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.pending_save_ops.remove(&op_id))
                {
                    let _ = reply.send(Err(message.clone()));
                    continue;
                }
                if let Some(reply) = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.pending_credential_ops.remove(&op_id))
                {
                    let _ = reply.send(Err(message.clone()));
                    continue;
                }
                if let Some(reply) = shared.lock().ok().and_then(|mut state| {
                    let reply = state.pending_delete_ops.remove(&op_id)?;
                    if let (Some(provider_id), Some(remote_job_id)) =
                        (provider_id.as_ref(), job_id.as_ref())
                        && let Ok(key) =
                            ProviderJobKey::new(provider_id.clone(), remote_job_id.clone())
                        && let Some(id) = state.job_ids.get(&key).cloned()
                    {
                        state.deletion_pending.remove(&id);
                    }
                    Some(reply)
                }) {
                    let _ = reply.send(Err(message.clone()));
                    continue;
                }
                let pending_operation = shared
                    .lock()
                    .ok()
                    .and_then(|mut state| state.fail_operation(op_id));
                if let Some(operation) = pending_operation {
                    broadcast(
                        &shared,
                        UiEvent::OperationFailed {
                            operation,
                            message: message.clone(),
                        },
                    );
                    continue;
                }
                let handled = if let (Some(provider_id), Some(remote_job_id)) =
                    (provider_id.as_ref(), job_id.as_ref())
                {
                    let key = ProviderJobKey::new(provider_id.clone(), remote_job_id.clone()).ok();
                    key.and_then(|key| {
                        shared
                            .lock()
                            .ok()
                            .and_then(|state| state.job_ids.get(&key).cloned())
                    })
                } else {
                    None
                };
                if let Some(id) = handled {
                    mutate_and_broadcast(&shared, |state| {
                        let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                        let mut job = state.snapshot.jobs[index].clone();
                        job.status = "attention".into();
                        job.status_label = "Generation needs attention".into();
                        job.detail = message.clone();
                        state.snapshot.jobs[index] = job.clone();
                        Some(UiEvent::JobUpdated { job })
                    });
                } else {
                    broadcast(
                        &shared,
                        UiEvent::Notice {
                            tone: if recoverable { "warning" } else { "danger" }.into(),
                            message,
                        },
                    );
                }
            }
            ServiceEvent::ShutdownBlocked { reason } => {
                let reason = safe_shared_message(&shared, &reason);
                let should_notify = shared.lock().ok().is_some_and(|mut state| {
                    if state.shutdown_block_notice_sent {
                        false
                    } else {
                        state.shutdown_block_notice_sent = true;
                        true
                    }
                });
                if should_notify {
                    broadcast(
                        &shared,
                        UiEvent::Notice {
                            tone: "warning".into(),
                            message: reason,
                        },
                    );
                }
                let schedule_retry = shared.lock().ok().is_some_and(|mut state| {
                    if state.shutdown_requested
                        && !state.shutdown_complete
                        && !state.shutdown_retry_scheduled
                    {
                        state.shutdown_retry_scheduled = true;
                        true
                    } else {
                        false
                    }
                });
                if schedule_retry {
                    let retry_commands = commands.clone();
                    let retry_shared = shared.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        let should_retry = retry_shared.lock().ok().is_some_and(|mut state| {
                            state.shutdown_retry_scheduled = false;
                            state.shutdown_requested && !state.shutdown_complete
                        });
                        if should_retry {
                            let _ = retry_commands.send(ServiceCommand::Shutdown).await;
                        }
                    });
                }
            }
            ServiceEvent::ShutdownComplete => {
                if let Ok(mut state) = shared.lock() {
                    state.shutdown_complete = true;
                }
                app.exit(0);
            }
            _ => {}
        }
    }
}

fn next_op_id() -> u64 {
    let bytes = *Uuid::new_v4().as_bytes();
    u64::from_le_bytes(bytes[..8].try_into().expect("eight UUID bytes"))
}

fn queue(state: &DesktopState, command: ServiceCommand) -> Result<(), String> {
    state
        .commands
        .try_send(command)
        .map_err(|_| "The background service is busy; try again in a moment.".into())
}

async fn await_credential_ack(
    shared: Arc<Mutex<Shared>>,
    op_id: u64,
    reply: oneshot::Receiver<Result<(), String>>,
) -> Result<(), String> {
    match tokio::time::timeout(std::time::Duration::from_secs(30), reply).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("The credential service stopped before confirming the change.".into()),
        Err(_) => {
            if let Ok(mut state) = shared.lock() {
                state.pending_credential_ops.remove(&op_id);
            }
            Err("The provider credential check timed out.".into())
        }
    }
}

#[tauri::command]
fn open_session(
    on_event: Channel<UiEventEnvelope>,
    state: State<'_, DesktopState>,
) -> Result<OpenSessionResult, String> {
    let (result, start) = {
        let mut shared = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())?;
        shared.channels.push(on_event);
        let start = !shared.opened;
        shared.opened = true;
        (
            OpenSessionResult {
                seq: shared.seq,
                snapshot: shared.snapshot.clone(),
            },
            start,
        )
    };
    if start {
        queue(
            &state,
            ServiceCommand::LoadHistory {
                op_id: next_op_id(),
                limit: HISTORY_LIMIT,
            },
        )?;
        queue(
            &state,
            ServiceCommand::LoadDraft {
                op_id: next_op_id(),
            },
        )?;
        for provider_id in [ProviderId::openrouter(), ProviderId::fal()] {
            queue(
                &state,
                ServiceCommand::RefreshCatalog {
                    op_id: next_op_id(),
                    provider_id,
                },
            )?;
        }
    }
    Ok(result)
}

#[tauri::command]
fn get_snapshot(state: State<'_, DesktopState>) -> Result<OpenSessionResult, String> {
    let shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    Ok(OpenSessionResult {
        seq: shared.seq,
        snapshot: shared.snapshot.clone(),
    })
}

#[tauri::command]
async fn connect_provider(
    provider_id: String,
    mut key: String,
    persist_on_success: bool,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let provider_id = checked_provider(&provider_id)?;
    if key.trim().is_empty() || key.chars().any(char::is_whitespace) {
        key.zeroize();
        return Err("API keys cannot be empty or contain whitespace.".into());
    }
    let secret = SecretString::from(key.clone());
    key.zeroize();
    let op_id = next_op_id();
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .pending_credential_ops
        .insert(op_id, reply_tx);
    if let Err(error) = queue(
        &state,
        ServiceCommand::ConnectApiKey {
            op_id,
            provider_id,
            key: secret,
            persist_on_success,
        },
    ) {
        if let Ok(mut shared) = state.shared.lock() {
            shared.pending_credential_ops.remove(&op_id);
        }
        return Err(error);
    }
    await_credential_ack(state.shared.clone(), op_id, reply_rx).await
}

#[tauri::command]
async fn forget_provider(
    provider_id: String,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let provider_id = checked_provider(&provider_id)?;
    let op_id = next_op_id();
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .pending_credential_ops
        .insert(op_id, reply_tx);
    if let Err(error) = queue(&state, ServiceCommand::ForgetApiKey { op_id, provider_id }) {
        if let Ok(mut shared) = state.shared.lock() {
            shared.pending_credential_ops.remove(&op_id);
        }
        return Err(error);
    }
    await_credential_ack(state.shared.clone(), op_id, reply_rx).await
}

#[tauri::command]
fn acknowledge_safety_hold(handle: String, state: State<'_, DesktopState>) -> Result<(), String> {
    let grant = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .safety_holds
        .get(&handle)
        .cloned()
        .ok_or_else(|| "That safety hold is no longer current.".to_string())?;
    queue(
        &state,
        ServiceCommand::ClearUncertainSubmission {
            op_id: next_op_id(),
            provider_id: grant.provider_id,
            draft_fingerprint: grant.draft_fingerprint,
        },
    )
}

#[tauri::command]
async fn choose_media(
    app: AppHandle,
    state: State<'_, DesktopState>,
) -> Result<Vec<MediaItem>, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter(
            "Supported media",
            &[
                "png", "jpg", "jpeg", "webp", "gif", "avif", "bmp", "tif", "tiff", "mp4", "mov",
                "mp3", "wav",
            ],
        )
        .pick_files(move |selection| {
            let _ = sender.send(selection);
        });
    let selected = receiver
        .await
        .map_err(|_| "The media chooser closed unexpectedly.".to_string())?
        .unwrap_or_default();
    let mut shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    selected
        .into_iter()
        .map(|path| {
            path.into_path()
                .map_err(|_| "Only local files can be selected here.".to_string())
                .and_then(|path| local_media_item(&mut shared, path))
        })
        .collect()
}

#[tauri::command]
fn attach_dropped_media(
    app: AppHandle,
    paths: Vec<PathBuf>,
    state: State<'_, DesktopState>,
) -> Result<Vec<MediaItem>, String> {
    if paths.len() > 32 {
        return Err("Drop at most 32 media files at once.".into());
    }
    let scope = app.asset_protocol_scope();
    let mut shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    paths
        .into_iter()
        .map(|path| {
            if !scope.is_allowed(&path) {
                return Err("That path did not come from a native file drop.".into());
            }
            local_media_item(&mut shared, path)
        })
        .collect()
}

#[tauri::command]
fn add_remote_media(
    url: String,
    kind: MediaKind,
    role: MediaRole,
    state: State<'_, DesktopState>,
) -> Result<MediaItem, String> {
    let mut shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    remote_media_item(&mut shared, url, kind, role)
}

#[tauri::command]
fn prepare_generation(
    draft: GenerationDraft,
    allow_cross_provider_upload: Option<bool>,
    local_media_upload_confirmed: Option<bool>,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let allowed = allow_cross_provider_upload.unwrap_or(false)
        || local_media_upload_confirmed.unwrap_or(false);
    let mut shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    if !shared.preparation_ops.is_empty() || !shared.submission_ops.is_empty() {
        return Err("Another Review preparation or paid submission is already in progress.".into());
    }
    let core = core_draft(&shared, &draft, DraftPurpose::Review)?;
    core.validate()
        .map_err(|error| shared.ui_safe_message(&error.to_string()))?;
    let has_local = core
        .media
        .iter()
        .any(|media| matches!(media.source, MediaSource::LocalFile { .. }));
    let needs_cross_provider_upload = core.provider_id == ProviderId::openrouter() && has_local;
    if needs_cross_provider_upload && !allowed {
        return Err("Local-media staging was not confirmed. No files were uploaded.".into());
    }
    let staging_provider_id = needs_cross_provider_upload.then(ProviderId::fal);
    let op_id = next_op_id();
    shared.pending_drafts.insert(op_id, draft.clone());
    if needs_cross_provider_upload {
        shared
            .pending_disclosures
            .insert(op_id, CROSS_PROVIDER_DISCLOSURE.into());
    } else {
        shared.pending_disclosures.remove(&op_id);
    }
    let draft_was_saved = shared.snapshot.draft_saved && shared.snapshot.draft == draft;
    shared.snapshot.draft = draft.clone();
    shared.snapshot.draft_saved = draft_was_saved;
    shared.preparation_ops.insert(op_id, draft.revision);
    drop(shared);
    if let Err(error) = queue(
        &state,
        ServiceCommand::PrepareGeneration {
            op_id,
            draft: core,
            revision: draft.revision,
            staging_provider_id,
        },
    ) {
        if let Ok(mut shared) = state.shared.lock() {
            shared.fail_operation(op_id);
        }
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
fn submit_prepared(prepared_id: u64, state: State<'_, DesktopState>) -> Result<(), String> {
    let op_id = next_op_id();
    let review = {
        let mut shared = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())?;
        if !shared.submission_ops.is_empty() {
            return Err("A paid submission is already in progress.".into());
        }
        let review = shared
            .snapshot
            .prepared_review
            .as_ref()
            .filter(|review| review.prepared_id == prepared_id)
            .cloned()
            .ok_or_else(|| "This Review is no longer current.".to_string())?;
        shared.submission_ops.insert(op_id);
        shared.submitted_review = Some(review.clone());
        review
    };
    if let Err(error) = queue(
        &state,
        ServiceCommand::SubmitPrepared {
            op_id,
            prepared_id: PreparedGenerationId(prepared_id),
        },
    ) {
        if let Ok(mut shared) = state.shared.lock() {
            shared.fail_operation(op_id);
            if shared
                .submitted_review
                .as_ref()
                .is_some_and(|candidate| candidate.prepared_id == review.prepared_id)
            {
                shared.submitted_review = None;
            }
        }
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
fn invalidate_prepared(revision: u64, state: State<'_, DesktopState>) -> Result<(), String> {
    queue(
        &state,
        ServiceCommand::InvalidatePrepared {
            op_id: next_op_id(),
            revision,
        },
    )
}

#[tauri::command]
async fn save_draft(draft: GenerationDraft, state: State<'_, DesktopState>) -> Result<(), String> {
    let op_id = next_op_id();
    let (reply_tx, reply_rx) = oneshot::channel();
    let (core, editor_state) = {
        let mut shared = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())?;
        let core = core_draft(&shared, &draft, DraftPurpose::Autosave)?;
        let editor_state = editor_state_for_draft(&shared, &draft);
        shared.preserved_draft = Some(PreservedDraftFields::from_core(&core, editor_state.clone()));
        shared.snapshot.draft = draft.clone();
        shared.snapshot.draft_saved = false;
        shared.pending_save_ops.insert(op_id, reply_tx);
        (core, editor_state)
    };
    if let Err(error) = queue(
        &state,
        ServiceCommand::SaveDraft {
            op_id,
            draft: core,
            editor_state,
            revision: draft.revision,
        },
    ) {
        if let Ok(mut shared) = state.shared.lock() {
            shared.pending_save_ops.remove(&op_id);
        }
        return Err(error);
    }
    match tokio::time::timeout(std::time::Duration::from_secs(30), reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("The draft service stopped before confirming the save.".into()),
        Err(_) => {
            if let Ok(mut shared) = state.shared.lock() {
                shared.pending_save_ops.remove(&op_id);
            }
            Err("Saving the draft timed out; Review was not started.".into())
        }
    }
}

fn job_command(state: &DesktopState, job_id: &str, resume: bool) -> Result<ServiceCommand, String> {
    let shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    let key = shared
        .jobs
        .get(job_id)
        .map(|grant| grant.key.clone())
        .ok_or_else(|| "That job handle is no longer valid.".to_string())?;
    Ok(if resume {
        ServiceCommand::Resume {
            op_id: next_op_id(),
            key,
        }
    } else {
        ServiceCommand::PauseMonitor {
            op_id: next_op_id(),
            key,
        }
    })
}

#[tauri::command]
fn pause_job(job_id: String, state: State<'_, DesktopState>) -> Result<(), String> {
    queue(&state, job_command(&state, &job_id, false)?)
}

#[tauri::command]
fn resume_job(job_id: String, state: State<'_, DesktopState>) -> Result<(), String> {
    queue(&state, job_command(&state, &job_id, true)?)
}

fn verified_output(shared: &Shared, job_id: &str) -> Result<PathBuf, String> {
    if shared.deletion_pending.contains(job_id) {
        return Err("This render is being removed from the reel.".into());
    }
    let path = shared
        .jobs
        .get(job_id)
        .and_then(|grant| grant.output_path.as_ref())
        .ok_or_else(|| "This job does not have a local output yet.".to_string())?;
    let path = path
        .canonicalize()
        .map_err(|_| "The saved output is no longer available.".to_string())?;
    let videos = shared
        .videos_dir
        .canonicalize()
        .map_err(|_| "The Videos folder is unavailable.".to_string())?;
    if !path.starts_with(&videos) || !path.is_file() {
        return Err("The output path failed the Videos-folder safety check.".into());
    }
    Ok(path)
}

#[tauri::command]
async fn delete_render(
    job_id: String,
    delete_output: bool,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let op_id = next_op_id();
    let (reply_tx, reply_rx) = oneshot::channel();
    let key = {
        let mut shared = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())?;
        let job = shared
            .snapshot
            .jobs
            .iter()
            .find(|job| job.id == job_id)
            .ok_or_else(|| "That render is no longer on the reel.".to_string())?;
        if !job.deletable {
            return Err("Only finished renders can be removed from the reel.".into());
        }
        if shared.deletion_pending.contains(&job_id) {
            return Err("That render is already being removed.".into());
        }
        if shared
            .playback_grants
            .values()
            .any(|grant| grant.job_id == job_id)
        {
            return Err("Stop the in-app video before removing this render.".into());
        }
        let key = shared
            .jobs
            .get(&job_id)
            .map(|grant| grant.key.clone())
            .ok_or_else(|| "That job handle is no longer valid.".to_string())?;
        shared.deletion_pending.insert(job_id.clone());
        shared.pending_delete_ops.insert(op_id, reply_tx);
        key
    };

    if let Err(error) = queue(
        &state,
        ServiceCommand::DeleteGeneration {
            op_id,
            key,
            delete_output,
        },
    ) {
        if let Ok(mut shared) = state.shared.lock() {
            shared.pending_delete_ops.remove(&op_id);
            shared.deletion_pending.remove(&job_id);
        }
        return Err(error);
    }

    match tokio::time::timeout(std::time::Duration::from_secs(30), reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("The render service stopped before confirming cleanup.".into()),
        Err(_) => {
            if let Ok(mut shared) = state.shared.lock() {
                shared.pending_delete_ops.remove(&op_id);
                shared.deletion_pending.remove(&job_id);
            }
            Err("Removing the render timed out; it may still be on the reel.".into())
        }
    }
}

#[tauri::command]
fn open_output(
    app: AppHandle,
    job_id: String,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let path = {
        let shared = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())?;
        verified_output(&shared, &job_id)?
    };
    app.opener()
        .open_path(path.to_string_lossy().into_owned(), None::<String>)
        .map_err(|_| "The system could not open that video.".into())
}

fn playback_url(path: &Path) -> String {
    let encoded = utf8_percent_encode(&path.to_string_lossy(), NON_ALPHANUMERIC).to_string();
    if cfg!(windows) {
        format!("http://asset.localhost/{encoded}")
    } else {
        format!("asset://localhost/{encoded}")
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PlaybackPurge {
    removed: usize,
    retained: usize,
}

fn playback_grant_file_name(name: &OsStr) -> bool {
    let Some(stem) = Path::new(name).file_stem().and_then(OsStr::to_str) else {
        return false;
    };
    stem.strip_prefix("playback-")
        .is_some_and(|id| Uuid::parse_str(id).is_ok())
}

fn remove_playback_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes hard links left behind by a previous process without following
/// symlinks or touching files that were not created by the playback-grant
/// naming scheme.
fn purge_stale_playback_grants(playback_dir: &Path) -> io::Result<PlaybackPurge> {
    match std::fs::symlink_metadata(playback_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the playback-grants path is not a real directory",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(playback_dir)?;
            let metadata = std::fs::symlink_metadata(playback_dir)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the playback-grants path is not a real directory",
                ));
            }
        }
        Err(error) => return Err(error),
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(playback_dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let mut purge = PlaybackPurge::default();
    for entry in std::fs::read_dir(playback_dir)? {
        let Ok(entry) = entry else {
            purge.retained += 1;
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            purge.retained += 1;
            continue;
        };
        if !file_type.is_file() || !playback_grant_file_name(&entry.file_name()) {
            continue;
        }
        match remove_playback_file(&entry.path()) {
            Ok(()) => purge.removed += 1,
            Err(_) => purge.retained += 1,
        }
    }
    Ok(purge)
}

fn rollback_playback_grant(shared: &mut Shared, grant_id: &str, path: &Path) -> io::Result<()> {
    if shared
        .playback_grants
        .get(grant_id)
        .is_some_and(|registered| registered.path == path)
    {
        shared.playback_grants.remove(grant_id);
    }
    remove_playback_file(path)
}

fn finish_playback_release(shared: &mut Shared, grant_id: &str, path: &Path) -> io::Result<()> {
    // On Windows the webview can retain a short-lived file handle after the
    // asset scope is revoked. Delete first and remove the opaque handle only
    // after deletion succeeds, so the frontend can retry the same grant.
    remove_playback_file(path)?;
    if shared
        .playback_grants
        .get(grant_id)
        .is_some_and(|registered| registered.path == path)
    {
        shared.playback_grants.remove(grant_id);
    }
    Ok(())
}

#[tauri::command]
fn grant_playback(
    app: AppHandle,
    job_id: String,
    state: State<'_, DesktopState>,
) -> Result<PlaybackGrant, String> {
    let (grant_id, link) = {
        let mut shared = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())?;
        if shared
            .playback_grants
            .values()
            .any(|grant| grant.job_id == job_id)
        {
            return Err("This video already has an active in-app playback session.".into());
        }
        let source = verified_output(&shared, &job_id)?;
        std::fs::create_dir_all(&shared.playback_dir)
            .map_err(|_| "The secure playback cache is unavailable.".to_string())?;
        let grant_id = format!("playback-{}", Uuid::new_v4());
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("mp4");
        let link = shared.playback_dir.join(format!("{grant_id}.{extension}"));
        std::fs::hard_link(&source, &link).map_err(|_| {
            "Secure inline playback is unavailable for this filesystem; use Open file instead."
                .to_string()
        })?;
        shared.playback_grants.insert(
            grant_id.clone(),
            PlaybackGrantState {
                path: link.clone(),
                job_id: job_id.clone(),
            },
        );
        (grant_id, link)
    };
    if app.asset_protocol_scope().allow_file(&link).is_err() {
        let rollback = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())
            .and_then(|mut shared| {
                rollback_playback_grant(&mut shared, &grant_id, &link)
                    .map_err(|_| "The failed playback grant could not be cleaned up.".to_string())
            });
        return Err(if rollback.is_ok() {
            "The playback grant could not be scoped.".to_string()
        } else {
            "The playback grant could not be scoped or cleaned up; it will be purged on the next launch."
                .to_string()
        });
    }
    Ok(PlaybackGrant {
        grant_id,
        url: playback_url(&link),
    })
}

#[tauri::command]
fn release_playback(
    app: AppHandle,
    grant_id: String,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let path = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .playback_grants
        .get(&grant_id)
        .map(|grant| grant.path.clone())
        .ok_or_else(|| "That playback grant is no longer active.".to_string())?;
    app.asset_protocol_scope()
        .forbid_file(&path)
        .map_err(|_| "The playback grant could not be revoked; try again.".to_string())?;
    let mut shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    finish_playback_release(&mut shared, &grant_id, &path)
        .map_err(|_| "The playback file is still in use; try releasing it again.".into())
}

#[cfg(test)]
// Runtime entry points stay at the bottom of this long facade so they remain
// easy to audit; keeping focused unit tests adjacent to the command helpers is
// intentional.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn renderer_messages_remove_url_tokens_credentials_and_controls() {
        let message = safe_message(
            "Upload HTTPS://cdn.example/video.mp4?token=secret#fragment failed for sk-or-v1-testsecret\u{0007}",
        );

        assert_eq!(
            message,
            "Upload HTTPS://cdn.example/video.mp4 failed for [redacted credential]"
        );
        assert!(!message.contains("secret"));
        assert!(!message.contains("fragment"));
    }

    #[test]
    fn provider_job_ids_are_never_visually_abbreviated_by_the_facade() {
        let long_id = format!("request-{}", "a".repeat(180));
        assert_eq!(
            displayable_remote_job_id(&long_id).as_deref(),
            Some(long_id.as_str())
        );
        assert_eq!(
            displayable_remote_job_id("request-123?token=secret#fragment").as_deref(),
            Some("request-123")
        );
    }

    #[test]
    fn job_removed_event_uses_the_renderer_camel_case_handle() {
        let value = serde_json::to_value(UiEvent::JobRemoved {
            job_id: "job-fixture".into(),
        })
        .expect("serialize event");
        assert_eq!(value["type"], "job_removed");
        assert_eq!(value["jobId"], "job-fixture");
        assert!(value.get("job_id").is_none());
    }

    #[test]
    fn verified_outputs_stay_in_videos_and_lock_during_deletion() {
        let root = std::env::temp_dir().join(format!("video-harness-test-{}", Uuid::new_v4()));
        let videos = root.join("Videos");
        let playback = root.join("playback");
        std::fs::create_dir_all(&videos).expect("create Videos fixture");
        let output = videos.join("finished.mp4");
        std::fs::write(&output, b"video").expect("create output fixture");
        let key =
            ProviderJobKey::new(ProviderId::fal(), "request-fixture").expect("create provider key");
        let mut shared = Shared::new(videos.clone(), playback);
        shared.jobs.insert(
            "job-fixture".into(),
            JobGrant {
                key,
                output_path: Some(output.clone()),
            },
        );

        assert_eq!(
            verified_output(&shared, "job-fixture").expect("verify output"),
            output.canonicalize().expect("canonical output")
        );
        shared.deletion_pending.insert("job-fixture".into());
        assert!(
            verified_output(&shared, "job-fixture")
                .expect_err("pending deletion blocks new playback")
                .contains("being removed")
        );

        std::fs::remove_file(output).expect("remove output fixture");
        std::fs::remove_dir(videos).expect("remove Videos fixture");
        std::fs::remove_dir(root).expect("remove root fixture");
    }

    #[test]
    fn renderer_messages_replace_known_local_paths_and_keep_plain_errors() {
        let local = PathBuf::from("/private/video-harness/reference frame.png");
        let mut shared = Shared::new(
            PathBuf::from("/private/video-harness/Videos"),
            PathBuf::from("/private/video-harness/cache/playback-grants"),
        );
        shared.media.insert(
            "fixture".into(),
            MediaGrant {
                origin: MediaOrigin::Local(local.clone()),
                kind: MediaKind::Image,
            },
        );

        let message = shared.ui_safe_message(&format!(
            "Could not inspect {}: permission denied",
            local.display()
        ));
        assert_eq!(message, "Could not inspect [local path]: permission denied");
        assert_eq!(
            shared.ui_safe_message("The provider is still processing."),
            "The provider is still processing."
        );
    }

    #[test]
    fn invalid_local_media_errors_never_expose_the_selected_path() {
        let root = std::env::temp_dir().join(format!("video-harness-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create fixture directory");
        let path = root.join("private-reference.png");
        std::fs::write(&path, b"not a png").expect("create invalid fixture");
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));

        let error = local_media_item(&mut shared, path.clone())
            .expect_err("an invalid media signature must be rejected");

        assert!(!error.contains(path.to_string_lossy().as_ref()));
        assert!(error.contains("does not match"));
        std::fs::remove_file(path).expect("remove fixture");
        std::fs::remove_dir(root).expect("remove fixture directory");
    }

    #[test]
    fn operation_failures_are_correlated_and_clear_preparation_payloads() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        shared.preparation_ops.insert(41, 7);
        shared.pending_drafts.insert(41, GenerationDraft::default());
        shared.pending_disclosures.insert(41, "disclosure".into());
        shared.submission_ops.insert(42);

        assert!(matches!(
            shared.fail_operation(41),
            Some(UiOperation::Preparation)
        ));
        assert!(!shared.pending_drafts.contains_key(&41));
        assert!(!shared.pending_disclosures.contains_key(&41));
        assert!(matches!(
            shared.fail_operation(42),
            Some(UiOperation::Submission)
        ));
        assert!(shared.fail_operation(999).is_none());
    }

    #[test]
    fn autosave_preserves_hidden_fields_and_raw_seed_but_review_is_strict() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let mut draft = GenerationDraft {
            model_id: "fixture/model".into(),
            prompt: "A fixture prompt".into(),
            ..GenerationDraft::default()
        };
        draft.settings.seed = "half-written".into();
        let editor_state = DraftEditorState {
            seed_text: "old raw seed".into(),
            advanced_json_text: r#"{"guidance": 4}"#.into(),
            schema_text: Default::default(),
        };
        shared.preserved_draft = Some(PreservedDraftFields {
            provider_id: "openrouter".into(),
            model_id: draft.model_id.clone(),
            size: Some("1280x720".into()),
            adapter_options: Some(serde_json::json!({"guidance": 4})),
            typed_seed: Some(23),
            editor_state,
        });

        let saved = core_draft(&shared, &draft, DraftPurpose::Autosave)
            .expect("autosave accepts raw editor seed");
        assert_eq!(saved.seed, Some(23));
        assert_eq!(saved.size.as_deref(), Some("1280x720"));
        assert_eq!(
            saved.adapter_options,
            Some(serde_json::json!({"guidance": 4}))
        );
        assert_eq!(
            editor_state_for_draft(&shared, &draft).seed_text,
            "half-written"
        );
        assert!(core_draft(&shared, &draft, DraftPurpose::Review).is_err());
    }

    #[test]
    fn failed_playback_release_keeps_the_handle_for_retry() {
        let root = std::env::temp_dir().join(format!("video-harness-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create fixture directory");
        let grant_id = "playback-fixture";
        let path = root.join("playback-fixture.mp4");
        std::fs::create_dir(&path).expect("make deletion fail on every platform");
        let mut shared = Shared::new(PathBuf::from("videos"), root.clone());
        shared.playback_grants.insert(
            grant_id.into(),
            PlaybackGrantState {
                path: path.clone(),
                job_id: "job-fixture".into(),
            },
        );

        finish_playback_release(&mut shared, grant_id, &path)
            .expect_err("a directory cannot be deleted as a playback file");
        assert_eq!(
            shared
                .playback_grants
                .get(grant_id)
                .map(|grant| &grant.path),
            Some(&path)
        );

        std::fs::remove_dir(&path).expect("remove blocking fixture");
        std::fs::write(&path, b"fixture").expect("create retry fixture");
        finish_playback_release(&mut shared, grant_id, &path).expect("retry release");
        assert!(!shared.playback_grants.contains_key(grant_id));
        std::fs::remove_dir(root).expect("remove fixture directory");
    }

    #[test]
    fn rollback_removes_the_failed_grant_and_hard_link() {
        let root = std::env::temp_dir().join(format!("video-harness-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create fixture directory");
        let path = root.join("playback-fixture.mp4");
        std::fs::write(&path, b"fixture").expect("create fixture grant");
        let mut shared = Shared::new(PathBuf::from("videos"), root.clone());
        shared.playback_grants.insert(
            "playback-fixture".into(),
            PlaybackGrantState {
                path: path.clone(),
                job_id: "job-fixture".into(),
            },
        );

        rollback_playback_grant(&mut shared, "playback-fixture", &path)
            .expect("roll back fixture grant");

        assert!(!shared.playback_grants.contains_key("playback-fixture"));
        assert!(!path.exists());
        std::fs::remove_dir(&root).expect("remove fixture directory");
    }

    #[test]
    fn startup_purge_only_removes_regular_playback_grants() {
        let root = std::env::temp_dir().join(format!("video-harness-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create fixture directory");
        let stale = root.join("playback-67e55044-10b1-426f-9247-bb680e5fe0c8.mp4");
        let unrelated = root.join("do-not-delete.mp4");
        let lookalike_dir = root.join("playback-9c858901-8a57-4791-81fe-4c455b099bc9.mp4");
        std::fs::write(&stale, b"stale").expect("create stale grant");
        std::fs::write(&unrelated, b"keep").expect("create unrelated file");
        std::fs::create_dir(&lookalike_dir).expect("create lookalike directory");

        let purge = purge_stale_playback_grants(&root).expect("purge stale grants");

        assert_eq!(
            purge,
            PlaybackPurge {
                removed: 1,
                retained: 0
            }
        );
        assert!(!stale.exists());
        assert!(unrelated.exists());
        assert!(lookalike_dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&root)
                    .expect("read playback directory metadata")
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
        std::fs::remove_file(unrelated).expect("remove unrelated fixture");
        std::fs::remove_dir(lookalike_dir).expect("remove directory fixture");
        std::fs::remove_dir(root).expect("remove fixture directory");
    }

    #[cfg(unix)]
    #[test]
    fn startup_purge_refuses_a_symlinked_grant_directory() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("video-harness-test-{}", Uuid::new_v4()));
        let target = root.join("target");
        let playback_dir = root.join("playback-grants");
        std::fs::create_dir_all(&target).expect("create fixture target");
        let protected = target.join("playback-67e55044-10b1-426f-9247-bb680e5fe0c8.mp4");
        std::fs::write(&protected, b"keep").expect("create protected fixture");
        symlink(&target, &playback_dir).expect("create fixture symlink");

        let error = purge_stale_playback_grants(&playback_dir)
            .expect_err("a symlinked playback directory must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(protected.exists());
        std::fs::remove_file(playback_dir).expect("remove fixture symlink");
        std::fs::remove_file(protected).expect("remove protected fixture");
        std::fs::remove_dir(target).expect("remove target fixture");
        std::fs::remove_dir(root).expect("remove fixture directory");
    }
}

fn request_safe_shutdown(app: &AppHandle) {
    let Some(state) = app.try_state::<DesktopState>() else {
        return;
    };
    let should_request = state.shared.lock().ok().is_some_and(|mut shared| {
        if shared.shutdown_requested || shared.shutdown_complete {
            false
        } else {
            shared.shutdown_requested = true;
            true
        }
    });
    if !should_request {
        return;
    }
    let commands = state.commands.clone();
    let shared = state.shared.clone();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if commands.send(ServiceCommand::Shutdown).await.is_err() {
            if let Ok(mut state) = shared.lock() {
                state.shutdown_complete = true;
            }
            app.exit(1);
        }
    });
}

pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .setup(|app| {
            let resolver = app.path();
            let videos_dir = resolver.video_dir()?;
            // Existing Linux releases deliberately keep their historical XDG
            // storage identity so history, drafts, upload receipts, and
            // keyring records remain available during the shell migration.
            #[cfg(target_os = "linux")]
            let paths = AppPaths::new(
                resolver.data_dir()?.join(APP_NAME),
                resolver.cache_dir()?.join(APP_NAME),
                resolver.config_dir()?.join(APP_NAME),
                videos_dir,
            );
            #[cfg(not(target_os = "linux"))]
            let paths = AppPaths::new(
                resolver.app_data_dir()?,
                resolver.app_cache_dir()?,
                resolver.app_config_dir()?,
                videos_dir,
            );
            let playback_dir = paths.cache_dir.join("playback-grants");
            let purge = purge_stale_playback_grants(&playback_dir)?;
            if purge.retained > 0 {
                eprintln!(
                    "Video Harness retained {} stale playback grant(s) that could not yet be removed.",
                    purge.retained
                );
            }
            let videos_dir = paths.videos_dir.clone();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("video-harness-service")
                .build()?;
            let service = {
                let _guard = runtime.enter();
                spawn_service(paths, ServiceConfig::default())?
            };
            let shared = Arc::new(Mutex::new(Shared::new(videos_dir, playback_dir)));
            runtime.spawn(service_events(
                service.events,
                shared.clone(),
                service.commands.clone(),
                app.handle().clone(),
            ));
            app.manage(DesktopState {
                commands: service.commands,
                shared,
                _runtime: runtime,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_session,
            get_snapshot,
            connect_provider,
            forget_provider,
            acknowledge_safety_hold,
            choose_media,
            attach_dropped_media,
            add_remote_media,
            prepare_generation,
            submit_prepared,
            invalidate_prepared,
            save_draft,
            pause_job,
            resume_job,
            delete_render,
            open_output,
            grant_playback,
            release_playback,
        ])
        .build(tauri::generate_context!())
        .expect("Video Harness desktop runtime failed");
    app.run(|app, event| match event {
        tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } => {
            api.prevent_close();
            request_safe_shutdown(app);
        }
        tauri::RunEvent::ExitRequested { api, .. } => {
            let complete = app
                .try_state::<DesktopState>()
                .and_then(|state| {
                    state
                        .shared
                        .lock()
                        .ok()
                        .map(|shared| shared.shutdown_complete)
                })
                .unwrap_or(true);
            if !complete {
                api.prevent_exit();
                request_safe_shutdown(app);
            }
        }
        _ => {}
    });
}
