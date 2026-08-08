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
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
#[cfg(target_os = "linux")]
use video_harness::config::APP_NAME;
use video_harness::credentials::CredentialStatus;
use video_harness::domain::{
    CostQuote, DraftMedia, GenerationDraft as CoreDraft, JobStatus as CoreJobStatus,
    MediaCardinality, MediaKind as CoreMediaKind, MediaRole as CoreMediaRole, MediaSource,
    ProviderId, ProviderJobKey, VideoCatalog, VideoJob, VideoModel, VideoRequest,
};
use video_harness::gui_state::{DraftEditorState, ResumableJob, UncertainSubmissionRecord};
use video_harness::history::JobRecord;
use video_harness::providers::{
    MAX_AUDIO_INPUTS, MAX_IMAGE_INPUTS, MAX_MEDIA_INPUTS_TOTAL, MAX_VIDEO_INPUTS, ProviderAccount,
    audio_input_requires_visual,
};
use video_harness::workflow::{PreparedGenerationId, ProviderConnection};
use video_harness::{AppPaths, ServiceCommand, ServiceConfig, ServiceEvent, spawn_service};
use zeroize::Zeroize;

const HISTORY_LIMIT: usize = 200;
const CLOSE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const CLOSE_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const CROSS_PROVIDER_DISCLOSURE: &str = "Your local references will be uploaded to fal.ai as public-by-link files with a requested 24-hour expiry, then their URLs will be shared with OpenRouter and the selected model provider.";
const DIRECT_FAL_DISCLOSURE: &str = "Your local references will be uploaded to fal.ai as public-by-link files with a requested 24-hour expiry, then used by the selected fal.ai model.";

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
    seed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaConstraintSummary {
    kind: MediaKind,
    roles: Vec<MediaRole>,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_items: Option<usize>,
    /// A conditional schema minimum: zero items are valid, but once this
    /// media bucket is used it must contain at least this many items.
    #[serde(skip_serializing_if = "Option::is_none")]
    min_items_when_present: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_items: Option<usize>,
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
    size_options: Vec<String>,
    supported_image_roles: Vec<MediaRole>,
    required_image_roles: Vec<MediaRole>,
    media_constraints: Vec<MediaConstraintSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_media_items: Option<usize>,
    audio_requires_visual: bool,
    frames_exclusive_with_references: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    display_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerationSettings {
    duration: String,
    resolution: String,
    aspect_ratio: String,
    #[serde(default)]
    size: String,
    generated_audio: String,
    seed: String,
    #[serde(default)]
    advanced_json: String,
}

impl Default for GenerationSettings {
    fn default() -> Self {
        Self {
            duration: String::new(),
            resolution: String::new(),
            aspect_ratio: String::new(),
            size: String::new(),
            generated_audio: "provider_default".into(),
            seed: String::new(),
            advanced_json: "{}".into(),
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
    monitor_state: MonitorState,
    can_resume: bool,
    can_pause: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MonitorState {
    Active,
    Paused,
    Recoverable,
    Terminal,
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
    BulkMonitorAcknowledged {
        action: String,
        #[serde(rename = "targetJobIds")]
        target_job_ids: Vec<String>,
    },
    CloseRequested {
        #[serde(rename = "requestId")]
        request_id: u64,
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
    preparing: bool,
    submitting: bool,
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
    adapter_options: Option<serde_json::Value>,
    typed_seed: Option<i64>,
    editor_state: DraftEditorState,
}

impl PreservedDraftFields {
    fn from_core(draft: &CoreDraft, editor_state: DraftEditorState) -> Self {
        Self {
            provider_id: draft.provider_id.as_str().into(),
            model_id: draft.model.clone(),
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalMediaUploadPlan {
    requires_consent: bool,
    staging_provider_id: Option<ProviderId>,
    disclosure: Option<&'static str>,
}

fn local_media_upload_plan(
    provider_id: &ProviderId,
    has_local_media: bool,
) -> LocalMediaUploadPlan {
    if !has_local_media {
        return LocalMediaUploadPlan {
            requires_consent: false,
            staging_provider_id: None,
            disclosure: None,
        };
    }
    if provider_id == &ProviderId::openrouter() {
        LocalMediaUploadPlan {
            requires_consent: true,
            staging_provider_id: Some(ProviderId::fal()),
            disclosure: Some(CROSS_PROVIDER_DISCLOSURE),
        }
    } else {
        LocalMediaUploadPlan {
            requires_consent: true,
            staging_provider_id: None,
            disclosure: Some(DIRECT_FAL_DISCLOSURE),
        }
    }
}

struct Shared {
    seq: u64,
    opened: bool,
    snapshot: AppSnapshot,
    channels: Vec<Channel<UiEventEnvelope>>,
    channel_generation: u64,
    media: HashMap<String, MediaGrant>,
    jobs: HashMap<String, JobGrant>,
    job_ids: HashMap<ProviderJobKey, String>,
    resumable_jobs: HashMap<ProviderJobKey, ResumableJob>,
    active_monitors: HashSet<ProviderJobKey>,
    pausing_monitors: HashMap<ProviderJobKey, bool>,
    stopping_monitors: HashSet<ProviderJobKey>,
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
    close_flush_next_id: u64,
    close_flush_pending: Option<u64>,
    close_flush_acknowledged: bool,
    close_flush_watchdog_generation: u64,
    close_flush_save_attempt: Option<u64>,
}

impl Shared {
    fn new(videos_dir: PathBuf, playback_dir: PathBuf) -> Self {
        Self {
            seq: 0,
            opened: false,
            snapshot: AppSnapshot::default(),
            channels: Vec::new(),
            channel_generation: 0,
            media: HashMap::new(),
            jobs: HashMap::new(),
            job_ids: HashMap::new(),
            resumable_jobs: HashMap::new(),
            active_monitors: HashSet::new(),
            pausing_monitors: HashMap::new(),
            stopping_monitors: HashSet::new(),
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
            close_flush_next_id: 0,
            close_flush_pending: None,
            close_flush_acknowledged: false,
            close_flush_watchdog_generation: 0,
            close_flush_save_attempt: None,
        }
    }

    fn begin_close_flush(&mut self) -> Option<u64> {
        if self.shutdown_requested || self.shutdown_complete || self.close_flush_pending.is_some() {
            return None;
        }
        self.close_flush_next_id = self.close_flush_next_id.wrapping_add(1);
        if self.close_flush_next_id == 0 {
            self.close_flush_next_id = 1;
        }
        self.close_flush_pending = Some(self.close_flush_next_id);
        self.close_flush_acknowledged = false;
        self.close_flush_pending
    }

    fn advance_close_flush_watchdog(&mut self) -> u64 {
        self.close_flush_watchdog_generation = self.close_flush_watchdog_generation.wrapping_add(1);
        if self.close_flush_watchdog_generation == 0 {
            self.close_flush_watchdog_generation = 1;
        }
        self.close_flush_watchdog_generation
    }

    fn issue_close_flush(&mut self) -> Option<(u64, u64)> {
        if self.shutdown_requested || self.shutdown_complete {
            return None;
        }
        let request_id = match self.close_flush_pending {
            Some(request_id) => request_id,
            None => self.begin_close_flush()?,
        };
        let watchdog_generation = self.advance_close_flush_watchdog();
        Some((request_id, watchdog_generation))
    }

    fn begin_close_save(&mut self, request_id: u64) -> Option<u64> {
        if self.close_flush_pending != Some(request_id)
            || self.shutdown_requested
            || self.close_flush_save_attempt.is_some()
        {
            return None;
        }
        let watchdog_generation = self.advance_close_flush_watchdog();
        self.close_flush_save_attempt = Some(watchdog_generation);
        Some(watchdog_generation)
    }

    fn suspend_failed_close_save(&mut self, request_id: u64, watchdog_generation: u64) -> bool {
        if self.close_flush_pending == Some(request_id)
            && self.close_flush_save_attempt == Some(watchdog_generation)
            && !self.shutdown_requested
        {
            self.close_flush_save_attempt = None;
            // A second window-close request may have armed a newer watchdog
            // while this save was running. Invalidate that timeout too: a
            // failed save must always leave the application open.
            self.advance_close_flush_watchdog();
            true
        } else {
            false
        }
    }

    fn acknowledge_close_flush(&mut self, request_id: u64) -> bool {
        if self.close_flush_pending == Some(request_id) && !self.shutdown_requested {
            self.close_flush_acknowledged = true;
            true
        } else {
            false
        }
    }

    fn cancel_close_flush(&mut self, request_id: u64) -> bool {
        if self.close_flush_pending == Some(request_id) && !self.shutdown_requested {
            self.close_flush_pending = None;
            self.close_flush_acknowledged = false;
            self.close_flush_save_attempt = None;
            true
        } else {
            false
        }
    }

    fn finish_close_flush(&mut self, request_id: u64, watchdog_generation: u64) -> bool {
        if self.close_flush_pending == Some(request_id)
            && self.close_flush_save_attempt == Some(watchdog_generation)
            && !self.shutdown_requested
        {
            self.close_flush_pending = None;
            self.close_flush_acknowledged = false;
            self.close_flush_save_attempt = None;
            true
        } else {
            false
        }
    }

    fn begin_shutdown(&mut self) -> bool {
        if self.shutdown_requested || self.shutdown_complete {
            return false;
        }
        self.close_flush_pending = None;
        self.close_flush_acknowledged = false;
        self.close_flush_save_attempt = None;
        self.shutdown_requested = true;
        true
    }

    fn begin_close_timeout_shutdown(
        &mut self,
        request_id: u64,
        watchdog_generation: Option<u64>,
        require_unacknowledged: bool,
    ) -> bool {
        if self.close_flush_pending != Some(request_id)
            || watchdog_generation
                .is_some_and(|generation| self.close_flush_watchdog_generation != generation)
            || (require_unacknowledged && self.close_flush_acknowledged)
        {
            false
        } else {
            self.begin_shutdown()
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

    fn sort_jobs_newest_first(&mut self) {
        self.snapshot
            .jobs
            .sort_by(|left, right| right.created_at.cmp(&left.created_at));
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

const fn provider_media_limit(kind: CoreMediaKind) -> usize {
    match kind {
        CoreMediaKind::Image => MAX_IMAGE_INPUTS,
        CoreMediaKind::Video => MAX_VIDEO_INPUTS,
        CoreMediaKind::Audio => MAX_AUDIO_INPUTS,
    }
}

fn media_constraint(model: &VideoModel, kind: CoreMediaKind) -> Option<MediaConstraintSummary> {
    let bindings = model
        .media_bindings
        .iter()
        .filter(|binding| binding.kind == kind)
        .collect::<Vec<_>>();
    let ui_kind = match kind {
        CoreMediaKind::Image => MediaKind::Image,
        CoreMediaKind::Video => MediaKind::Video,
        CoreMediaKind::Audio => MediaKind::Audio,
    };
    let mut roles = match kind {
        CoreMediaKind::Image => vec![MediaRole::Reference],
        CoreMediaKind::Video => vec![MediaRole::VideoReference],
        CoreMediaKind::Audio => vec![MediaRole::AudioReference],
    };
    if kind == CoreMediaKind::Image && !bindings.is_empty() {
        if model.field_map.get("first_frame").is_some_and(|property| {
            bindings
                .iter()
                .any(|binding| &binding.property_name == property)
        }) {
            roles.push(MediaRole::StartFrame);
        }
        if model.field_map.get("last_frame").is_some_and(|property| {
            bindings
                .iter()
                .any(|binding| &binding.property_name == property)
        }) {
            roles.push(MediaRole::EndFrame);
        }
    }
    if bindings.is_empty() {
        return model
            .supports_media_kind(kind)
            .then_some(MediaConstraintSummary {
                kind: ui_kind,
                roles,
                required: false,
                min_items: None,
                min_items_when_present: None,
                max_items: Some(provider_media_limit(kind)),
            });
    }

    let minimum =
        bindings
            .iter()
            .filter(|binding| binding.required)
            .fold(0usize, |total, binding| {
                // An optional array may still declare `minItems`; that minimum
                // applies only when the property is present and must not make
                // media globally required in the composer.
                total.saturating_add(binding.min_items.unwrap_or(1))
            });
    let minimum_when_present = (minimum == 0)
        .then(|| {
            bindings
                .iter()
                .filter(|binding| !binding.required)
                .map(|binding| match binding.cardinality {
                    MediaCardinality::Scalar => 1,
                    MediaCardinality::List => binding.min_items.unwrap_or(0).max(1),
                })
                // Fal discovery exposes at most one unambiguous binding for a
                // media kind. Taking the least positive minimum also keeps
                // hand-authored multi-binding catalogs permissive rather than
                // incorrectly requiring every optional property at once.
                .min()
        })
        .flatten();
    let maximum = bindings
        .iter()
        .try_fold(0usize, |total, binding| {
            let binding_maximum = match binding.cardinality {
                MediaCardinality::Scalar => Some(1),
                MediaCardinality::List => binding.max_items,
            }?;
            Some(total.saturating_add(binding_maximum))
        })
        .or(Some(provider_media_limit(kind)));
    Some(MediaConstraintSummary {
        kind: ui_kind,
        roles,
        required: minimum > 0,
        min_items: (minimum > 0).then_some(minimum),
        min_items_when_present: minimum_when_present,
        max_items: maximum,
    })
}

fn model_max_media_items(model: &VideoModel) -> Option<usize> {
    match model.provider_id.as_str() {
        "openrouter" => Some(MAX_MEDIA_INPUTS_TOTAL),
        "fal" => {
            // Fal deliberately honors a non-Seedance schema that advertises
            // a higher per-kind maximum instead of applying the conservative
            // cross-provider total fallback.
            let has_explicit_higher_maximum =
                !audio_input_requires_visual(&model.provider_id, &model.id)
                    && model.media_bindings.iter().any(|binding| {
                        binding.cardinality == MediaCardinality::List
                            && binding
                                .max_items
                                .is_some_and(|maximum| maximum > provider_media_limit(binding.kind))
                    });
            (!has_explicit_higher_maximum).then_some(MAX_MEDIA_INPUTS_TOTAL)
        }
        _ => None,
    }
}

fn frames_exclusive_with_references(model: &VideoModel) -> bool {
    // OpenRouter has two mutually exclusive request shapes: frame_images or
    // input_references of any kind. Fal conflicts are property-specific, so a
    // single all-reference flag would incorrectly reject valid frame+audio or
    // frame+video schemas there; its schema validator remains authoritative.
    model.provider_id == ProviderId::openrouter()
}

fn schema_requires_property(schema: &serde_json::Value, property: &str) -> bool {
    schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|required| {
            required
                .iter()
                .any(|candidate| candidate.as_str() == Some(property))
        })
        || schema
            .get("allOf")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|branches| {
                branches
                    .iter()
                    .any(|branch| schema_requires_property(branch, property))
            })
}

fn required_image_roles(model: &VideoModel) -> Vec<MediaRole> {
    let Some(schema) = model.input_schema.as_ref() else {
        return Vec::new();
    };
    [
        ("first_frame", MediaRole::StartFrame),
        ("last_frame", MediaRole::EndFrame),
    ]
    .into_iter()
    .filter_map(|(field, role)| {
        model
            .field_map
            .get(field)
            .is_some_and(|property| schema_requires_property(schema, property))
            .then_some(role)
    })
    .collect()
}

fn model_summary(model: &VideoModel) -> ModelSummary {
    let mut supported_image_roles = Vec::new();
    if model.supports_media_kind(CoreMediaKind::Image) {
        supported_image_roles.push(MediaRole::Reference);
    }
    if model
        .supported_frame_images
        .iter()
        .any(|role| role == "first_frame")
    {
        supported_image_roles.push(MediaRole::StartFrame);
    }
    if model
        .supported_frame_images
        .iter()
        .any(|role| role == "last_frame")
    {
        supported_image_roles.push(MediaRole::EndFrame);
    }
    // Frame inputs travel through dedicated provider fields rather than the
    // general image-reference binding. A frame-only model must still expose
    // an image picker to the renderer.
    let accepts_images = !supported_image_roles.is_empty();
    ModelSummary {
        id: model.id.clone(),
        provider_id: model.provider_id.as_str().into(),
        name: model.name.clone(),
        description: model.description.clone(),
        capabilities: ModelCapabilities {
            images: accepts_images,
            video: model.supports_media_kind(CoreMediaKind::Video),
            audio_references: model.supports_media_kind(CoreMediaKind::Audio),
            generated_audio: model.generated_audio.supported,
            seed: model.supports_seed(),
        },
        duration_options: model
            .supported_durations
            .iter()
            .map(ToString::to_string)
            .collect(),
        resolution_options: model.supported_resolutions.clone(),
        aspect_ratio_options: model.supported_aspect_ratios.clone(),
        size_options: model.supported_sizes.clone(),
        supported_image_roles,
        required_image_roles: required_image_roles(model),
        media_constraints: [
            CoreMediaKind::Image,
            CoreMediaKind::Video,
            CoreMediaKind::Audio,
        ]
        .into_iter()
        .filter_map(|kind| media_constraint(model, kind))
        .collect(),
        max_media_items: model_max_media_items(model),
        audio_requires_visual: audio_input_requires_visual(&model.provider_id, &model.id),
        frames_exclusive_with_references: frames_exclusive_with_references(model),
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
        display_url: None,
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
    let mut display_url = parsed;
    display_url.set_query(None);
    display_url.set_fragment(None);
    Ok(MediaItem {
        handle,
        display_name,
        kind,
        role,
        source: "remote".into(),
        detail: host,
        preview_url: None,
        display_url: Some(display_url.to_string()),
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
                display_url: None,
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
        &draft.settings.size,
        &draft.settings.seed,
    ] {
        if value.len() > 256 || value.chars().any(char::is_control) {
            return Err("A generation setting contains an invalid value.".into());
        }
    }
    if draft.settings.advanced_json.len() > 100_000
        || draft
            .settings
            .advanced_json
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        return Err("Advanced settings are larger than this build supports.".into());
    }
    editor_state_for_draft(shared, draft)
        .validate()
        .map_err(|error| shared.ui_safe_message(&error.to_string()))?;
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
    let advanced_text = draft.settings.advanced_json.trim();
    let adapter_options = if advanced_text.is_empty() {
        None
    } else {
        match serde_json::from_str::<serde_json::Value>(advanced_text) {
            Ok(serde_json::Value::Object(values)) => {
                (!values.is_empty()).then_some(serde_json::Value::Object(values))
            }
            Ok(_) if matches!(purpose, DraftPurpose::Autosave) => {
                preserved.and_then(|fields| fields.adapter_options.clone())
            }
            Err(_) if matches!(purpose, DraftPurpose::Autosave) => {
                preserved.and_then(|fields| fields.adapter_options.clone())
            }
            Ok(_) => return Err("Advanced settings must be a JSON object.".into()),
            Err(_) => return Err("Advanced settings must contain valid JSON.".into()),
        }
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
        size: nonempty(&draft.settings.size),
        generate_audio,
        seed,
        media,
        adapter_options,
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
    editor.advanced_json_text = draft.settings.advanced_json.clone();
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
        size: draft.size.clone().unwrap_or_default(),
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
        advanced_json: draft
            .adapter_options
            .as_ref()
            .and_then(|value| serde_json::to_string_pretty(value).ok())
            .unwrap_or_else(|| "{}".into()),
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
        size: request.size.clone().unwrap_or_default(),
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
        advanced_json: request
            .adapter_options
            .as_ref()
            .and_then(|value| serde_json::to_string_pretty(value).ok())
            .unwrap_or_else(|| "{}".into()),
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
            "Your video job is queued with the provider.".into(),
        ),
        CoreJobStatus::InProgress => (
            "processing".into(),
            "Making your video".into(),
            "Video Harness is checking the provider while the model works.".into(),
        ),
        CoreJobStatus::Completed if has_output => (
            "completed".into(),
            "Ready to watch".into(),
            "Your finished video is waiting in the Videos folder.".into(),
        ),
        CoreJobStatus::Completed => (
            "downloading".into(),
            "Saving your video".into(),
            "The video is ready. Video Harness is saving it to your Videos folder.".into(),
        ),
        CoreJobStatus::Failed => (
            "attention".into(),
            "Video job needs attention".into(),
            "The provider couldn’t finish this one.".into(),
        ),
        CoreJobStatus::Cancelled => (
            "attention".into(),
            "Video job cancelled".into(),
            "This video job was cancelled.".into(),
        ),
        CoreJobStatus::Expired => (
            "attention".into(),
            "Video job expired".into(),
            "The provider no longer has this video job.".into(),
        ),
        CoreJobStatus::Unknown(_) => (
            "processing".into(),
            "Checking the provider".into(),
            "The provider sent an unfamiliar status, so we’re keeping watch.".into(),
        ),
    }
}

fn monitor_capabilities(state: MonitorState) -> (bool, bool) {
    match state {
        MonitorState::Active => (false, true),
        MonitorState::Paused | MonitorState::Recoverable => (true, false),
        MonitorState::Terminal => (false, false),
    }
}

fn monitor_for_record(
    shared: &Shared,
    key: &ProviderJobKey,
    status: &CoreJobStatus,
    has_output: bool,
) -> MonitorState {
    match status {
        CoreJobStatus::Completed if has_output => MonitorState::Terminal,
        CoreJobStatus::Failed | CoreJobStatus::Cancelled | CoreJobStatus::Expired => {
            MonitorState::Terminal
        }
        _ if shared.active_monitors.contains(key) => MonitorState::Active,
        _ => MonitorState::Recoverable,
    }
}

fn record_deletable(status: &CoreJobStatus, has_output: bool) -> bool {
    matches!(
        status,
        CoreJobStatus::Failed | CoreJobStatus::Cancelled | CoreJobStatus::Expired
    ) || matches!(status, CoreJobStatus::Completed) && has_output
}

fn set_monitor_state(job: &mut JobSummary, monitor_state: MonitorState) {
    let (can_resume, can_pause) = monitor_capabilities(monitor_state);
    job.monitor_state = monitor_state;
    job.can_resume = can_resume;
    job.can_pause = can_pause;
}

fn set_monitor_starting(job: &mut JobSummary) {
    set_monitor_state(job, MonitorState::Active);
    // The paid request is active, but Pause is addressable only after the
    // service actor acknowledges insertion into its monitor registry.
    job.can_pause = false;
}

fn set_paused_projection(job: &mut JobSummary, remote_continues: bool) {
    job.status = "paused".into();
    job.status_label = "Monitoring paused".into();
    job.detail = if remote_continues {
        "The provider job continues remotely; only local checks are paused."
    } else {
        "The provider job has finished; local follow-up is paused."
    }
    .into();
    job.remote_continues = Some(remote_continues);
    job.next_poll_seconds = None;
    job.progress = None;
    set_monitor_state(job, MonitorState::Paused);
}

fn set_pause_pending_projection(job: &mut JobSummary, remote_continues: bool) {
    job.status = "paused".into();
    job.status_label = "Pausing monitoring".into();
    job.detail = if remote_continues {
        "Finishing the current provider check before local monitoring pauses."
    } else {
        "Finishing the current local step before monitoring pauses."
    }
    .into();
    job.remote_continues = Some(remote_continues);
    job.next_poll_seconds = None;
    job.progress = None;
    set_monitor_state(job, MonitorState::Paused);
    // The monitor actor has acknowledged Pause, but it has not removed the
    // old task from its registry yet. Resume must stay unavailable until the
    // final Cancelled event makes that hand-off authoritative.
    job.can_resume = false;
    job.can_pause = false;
}

fn set_recovery_stop_pending(job: &mut JobSummary, remote_continues: bool) {
    job.remote_continues = Some(remote_continues);
    job.next_poll_seconds = None;
    job.progress = None;
    set_monitor_state(job, MonitorState::Recoverable);
    // A recoverable task failure has been reported, but the actor may still
    // own its registry entry. Wait for MonitorStopped before offering Resume.
    job.can_resume = false;
    job.can_pause = false;
}

fn pause_projection_allowed(job: &JobSummary) -> bool {
    job.monitor_state != MonitorState::Terminal
}

fn apply_pause_all_projection(shared: &mut Shared, remote_continues: bool) -> usize {
    let keys = shared.active_monitors.iter().cloned().collect::<Vec<_>>();
    let mut projected = 0;
    for key in keys {
        shared.active_monitors.remove(&key);
        shared.stopping_monitors.remove(&key);
        shared
            .pausing_monitors
            .insert(key.clone(), remote_continues);
        let Some(id) = shared.job_ids.get(&key) else {
            continue;
        };
        let Some(index) = shared.snapshot.jobs.iter().position(|job| &job.id == id) else {
            continue;
        };
        let job = &mut shared.snapshot.jobs[index];
        if pause_projection_allowed(job) {
            set_pause_pending_projection(job, remote_continues);
            projected += 1;
        } else {
            shared.pausing_monitors.remove(&key);
        }
    }
    projected
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
    let output_path = record.output_path.clone().filter(|path| path.is_file());
    if let Some(grant) = shared.jobs.get_mut(&id) {
        grant.output_path = output_path.clone();
    }
    let status = CoreJobStatus::from_raw(record.status.clone());
    let monitor_state = monitor_for_record(shared, &key, &status, output_path.is_some());
    let (can_resume, can_pause) = monitor_capabilities(monitor_state);
    let deletable = record_deletable(&status, output_path.is_some());
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
            .unwrap_or_else(|| "Imported provider job".into()),
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
        deletable,
        monitor_state,
        can_resume,
        can_pause,
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
        monitor_state: MonitorState::Active,
        can_resume: false,
        can_pause: false,
    }
}

fn job_from_resumable(shared: &mut Shared, resumable: &ResumableJob) -> JobSummary {
    let id = shared.opaque_job_id(&resumable.key);
    JobSummary {
        id,
        provider_id: resumable.key.provider_id.as_str().into(),
        provider_name: provider_name(&resumable.key.provider_id).into(),
        model_name: "Recovered provider job".into(),
        prompt: "Recovered provider job".into(),
        status: "paused".into(),
        status_label: "Ready to resume".into(),
        detail: if resumable.monitoring_paused {
            "Local monitoring is paused; the provider job continues remotely."
        } else {
            "Video Harness recovered this provider job after relaunch. Resume to continue local checks."
        }
        .into(),
        created_at: resumable.accepted_at.to_rfc3339(),
        elapsed_seconds: None,
        next_poll_seconds: None,
        progress: None,
        output_file_name: None,
        has_local_output: false,
        playback_url: None,
        captions_url: None,
        remote_continues: Some(true),
        provider_job_id: displayable_remote_job_id(&resumable.key.remote_job_id),
        deletable: false,
        monitor_state: MonitorState::Recoverable,
        can_resume: true,
        can_pause: false,
    }
}

fn broadcast(shared: &Arc<Mutex<Shared>>, event: UiEvent) -> bool {
    let (envelope, channels, channel_generation) = {
        let mut guard = match shared.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        guard.seq = guard.seq.saturating_add(1);
        (
            UiEventEnvelope {
                seq: guard.seq,
                event,
            },
            guard.channels.clone(),
            guard.channel_generation,
        )
    };
    let mut delivered = false;
    for channel in channels {
        delivered |= channel.send(envelope.clone()).is_ok();
    }
    if !delivered && let Ok(mut guard) = shared.lock() {
        if guard.channel_generation == channel_generation {
            guard.channels.clear();
        } else {
            // A renderer replaced the failed channel while this event was in
            // flight. Its subsequent snapshot resync is authoritative.
            delivered = true;
        }
    }
    delivered
}

fn mutate_and_broadcast<F>(shared: &Arc<Mutex<Shared>>, update: F)
where
    F: FnOnce(&mut Shared) -> Option<UiEvent>,
{
    let event = shared.lock().ok().and_then(|mut guard| update(&mut guard));
    if let Some(event) = event {
        let _ = broadcast(shared, event);
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
                if commands
                    .send(ServiceCommand::RefreshCatalog {
                        op_id: next_op_id(),
                        provider_id,
                    })
                    .await
                    .is_err()
                {
                    broadcast(
                        &shared,
                        UiEvent::Notice {
                            tone: "warning".into(),
                            message: "The provider connected, but its model list could not be refreshed. Restart Video Harness to try again."
                                .into(),
                        },
                    );
                }
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
                    update_provider(state, &provider_id, false, &credential_status, None);
                    state
                        .snapshot
                        .models
                        .retain(|model| model.provider_id != provider_id.as_str());
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
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
                    let mut editor_state = editor_state.unwrap_or_default();
                    if editor_state.seed_text.is_empty() {
                        editor_state.seed_text =
                            draft.seed.map(|seed| seed.to_string()).unwrap_or_default();
                    }
                    if editor_state.advanced_json_text.is_empty() {
                        editor_state.advanced_json_text = draft
                            .adapter_options
                            .as_ref()
                            .and_then(|value| serde_json::to_string_pretty(value).ok())
                            .unwrap_or_else(|| "{}".into());
                    }
                    match ui_draft_from_core(state, &draft, revision.unwrap_or_default()) {
                        Ok(mut ui_draft) => {
                            ui_draft.settings.seed = editor_state.seed_text.clone();
                            ui_draft.settings.advanced_json =
                                editor_state.advanced_json_text.clone();
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
                    state.sort_jobs_newest_first();
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
            }
            ServiceEvent::ResumableJobsLoaded { jobs, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    for resumable in jobs {
                        state
                            .resumable_jobs
                            .insert(resumable.key.clone(), resumable.clone());
                        let job = job_from_resumable(state, &resumable);
                        state.upsert_job(job, false);
                    }
                    state.sort_jobs_newest_first();
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
                    state.active_monitors.remove(&key);
                    state.pausing_monitors.remove(&key);
                    state.stopping_monitors.remove(&key);
                    state.resumable_jobs.remove(&key);
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
                    let key = job.key();
                    state.pausing_monitors.remove(&key);
                    state.stopping_monitors.remove(&key);
                    let mut job = record
                        .as_ref()
                        .map(|record| job_from_record(state, record))
                        .unwrap_or_else(|| job_from_acceptance(state, &job));
                    set_monitor_starting(&mut job);
                    state.snapshot.prepared_review = None;
                    state.submitted_review = None;
                    let added = state.upsert_job(job.clone(), true);
                    Some(if added {
                        UiEvent::JobAdded { job }
                    } else {
                        UiEvent::JobUpdated { job }
                    })
                });
            }
            ServiceEvent::MonitorStarted { key, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    // Import can be acknowledged by the actor before its
                    // first provider response creates a visible job. Retain
                    // that authoritative registry state even when there is
                    // nothing to broadcast yet.
                    state.pausing_monitors.remove(&key);
                    state.stopping_monitors.remove(&key);
                    state.active_monitors.insert(key.clone());
                    let id = state.job_ids.get(&key)?.clone();
                    let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                    let mut job = state.snapshot.jobs[index].clone();
                    if job.monitor_state == MonitorState::Terminal {
                        state.active_monitors.remove(&key);
                        return Some(UiEvent::JobUpdated { job });
                    }
                    set_monitor_state(&mut job, MonitorState::Active);
                    state.snapshot.jobs[index] = job.clone();
                    Some(UiEvent::JobUpdated { job })
                });
            }
            ServiceEvent::Imported { record, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    let key = record.key();
                    state.pausing_monitors.remove(&key);
                    state.stopping_monitors.remove(&key);
                    let mut job = job_from_record(state, &record);
                    if job.monitor_state != MonitorState::Terminal
                        && !state.active_monitors.contains(&key)
                    {
                        set_monitor_starting(&mut job);
                    }
                    state.snapshot.prepared_review = None;
                    let added = state.upsert_job(job.clone(), true);
                    Some(if added {
                        UiEvent::JobAdded { job }
                    } else {
                        UiEvent::JobUpdated { job }
                    })
                });
            }
            ServiceEvent::JobUpdated { record, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    let key = record.key();
                    let pause_pending = state.pausing_monitors.get(&key).copied();
                    let mut job = job_from_record(state, &record);
                    if job.monitor_state == MonitorState::Terminal {
                        state.active_monitors.remove(&key);
                        state.pausing_monitors.remove(&key);
                        state.stopping_monitors.remove(&key);
                    } else if let Some(remote_continues) = pause_pending {
                        set_pause_pending_projection(&mut job, remote_continues);
                    } else if !state.active_monitors.contains(&key) {
                        set_monitor_starting(&mut job);
                    }
                    let added = state.upsert_job(job.clone(), false);
                    Some(if added {
                        UiEvent::JobAdded { job }
                    } else {
                        UiEvent::JobUpdated { job }
                    })
                });
            }
            ServiceEvent::Downloaded { record, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    let key = record.key();
                    state.active_monitors.remove(&key);
                    state.pausing_monitors.remove(&key);
                    state.stopping_monitors.remove(&key);
                    state.resumable_jobs.remove(&key);
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
                if let Some(remote_continues) = state.pausing_monitors.get(&key).copied() {
                    set_pause_pending_projection(&mut job, remote_continues);
                    state.snapshot.jobs[index] = job.clone();
                    return Some(UiEvent::JobUpdated { job });
                }
                if state.active_monitors.contains(&key) {
                    set_monitor_state(&mut job, MonitorState::Active);
                } else {
                    set_monitor_starting(&mut job);
                }
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
                if let Some(remote_continues) = state.pausing_monitors.get(&key).copied() {
                    set_pause_pending_projection(&mut job, remote_continues);
                    state.snapshot.jobs[index] = job.clone();
                    return Some(UiEvent::JobUpdated { job });
                }
                if state.active_monitors.contains(&key) {
                    set_monitor_state(&mut job, MonitorState::Active);
                } else {
                    set_monitor_starting(&mut job);
                }
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
                state.active_monitors.remove(&key);
                state.stopping_monitors.remove(&key);
                state.pausing_monitors.insert(key.clone(), remote_continues);
                let id = state.job_ids.get(&key)?.clone();
                let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                let mut job = state.snapshot.jobs[index].clone();
                if !pause_projection_allowed(&job) {
                    // The monitor can finish between the UI's pause click and
                    // the actor's acknowledgement. Never regress a completed
                    // or failed terminal result back to resumable.
                    state.pausing_monitors.remove(&key);
                    return Some(UiEvent::JobUpdated { job });
                }
                set_pause_pending_projection(&mut job, remote_continues);
                state.snapshot.jobs[index] = job.clone();
                Some(UiEvent::JobUpdated { job })
            }),
            ServiceEvent::MonitorsPaused {
                count,
                remote_continue,
                keys,
                ..
            } => {
                let target_job_ids = shared
                    .lock()
                    .ok()
                    .map(|state| {
                        keys.iter()
                            .filter_map(|key| state.job_ids.get(key).cloned())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                mutate_and_broadcast(&shared, |state| {
                    apply_pause_all_projection(state, remote_continue);
                    Some(UiEvent::SnapshotChanged {
                        snapshot: Box::new(state.snapshot.clone()),
                    })
                });
                broadcast(
                    &shared,
                    UiEvent::Notice {
                        tone: "neutral".into(),
                        message: if remote_continue {
                            format!(
                                "Pausing {count} local monitor(s). Provider jobs continue remotely."
                            )
                        } else {
                            format!("Pausing {count} local monitor(s).")
                        },
                    },
                );
                broadcast(
                    &shared,
                    UiEvent::BulkMonitorAcknowledged {
                        action: "pause".into(),
                        target_job_ids,
                    },
                );
            }
            ServiceEvent::ResumeAllStarted {
                started,
                skipped,
                started_keys,
                ..
            } => {
                let target_job_ids = shared
                    .lock()
                    .ok()
                    .map(|state| {
                        started_keys
                            .iter()
                            .filter_map(|key| state.job_ids.get(key).cloned())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                broadcast(
                    &shared,
                    UiEvent::BulkMonitorAcknowledged {
                        action: "resume".into(),
                        target_job_ids,
                    },
                );
                broadcast(
                    &shared,
                    UiEvent::Notice {
                        tone: if skipped > 0 { "warning" } else { "neutral" }.into(),
                        message: format!("Resumed {started} job(s); skipped {skipped}."),
                    },
                );
            }
            ServiceEvent::Cancelled {
                provider_id: Some(provider_id),
                job_id: Some(job_id),
                remote_continues,
                ..
            } => mutate_and_broadcast(&shared, |state| {
                // The task has observed cancellation, but the actor may not
                // have removed it from the monitor registry yet. Keep Resume
                // disabled until MonitorStopped confirms that hand-off.
                let key = ProviderJobKey::new(provider_id, job_id).ok()?;
                state.active_monitors.remove(&key);
                state.stopping_monitors.remove(&key);
                state.pausing_monitors.insert(key.clone(), remote_continues);
                let id = state.job_ids.get(&key)?.clone();
                let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                let mut job = state.snapshot.jobs[index].clone();
                if !pause_projection_allowed(&job) {
                    state.pausing_monitors.remove(&key);
                    return Some(UiEvent::JobUpdated { job });
                }
                set_pause_pending_projection(&mut job, remote_continues);
                state.snapshot.jobs[index] = job.clone();
                Some(UiEvent::JobUpdated { job })
            }),
            ServiceEvent::MonitorStopped { key, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    let was_active = state.active_monitors.remove(&key);
                    let was_stopping = state.stopping_monitors.remove(&key);
                    let remote_continues = state.pausing_monitors.remove(&key);
                    let id = state.job_ids.get(&key)?.clone();
                    let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                    let mut job = state.snapshot.jobs[index].clone();
                    if !pause_projection_allowed(&job) {
                        return Some(UiEvent::JobUpdated { job });
                    }
                    if let Some(remote_continues) = remote_continues {
                        set_paused_projection(&mut job, remote_continues);
                    } else if was_stopping {
                        set_monitor_state(&mut job, MonitorState::Recoverable);
                        job.can_pause = false;
                    } else if was_active {
                        job.status = "attention".into();
                        job.status_label = "Monitoring stopped".into();
                        job.detail =
                            "Local monitoring ended before this remote job reached a final state."
                                .into();
                        job.remote_continues = Some(true);
                        job.next_poll_seconds = None;
                        job.progress = None;
                        set_monitor_state(&mut job, MonitorState::Recoverable);
                    } else {
                        return None;
                    }
                    state.snapshot.jobs[index] = job.clone();
                    Some(UiEvent::JobUpdated { job })
                });
            }
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
            ServiceEvent::SubmissionStarted { .. } => {
                broadcast(
                    &shared,
                    UiEvent::Notice {
                        tone: "neutral".into(),
                        message: "Submitting the reviewed generation once.".into(),
                    },
                );
            }
            ServiceEvent::JobRecoveryFailed { key, message, .. } => {
                mutate_and_broadcast(&shared, |state| {
                    state.active_monitors.remove(&key);
                    state.pausing_monitors.remove(&key);
                    state.stopping_monitors.remove(&key);
                    state.resumable_jobs.remove(&key);
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
                    set_monitor_state(&mut job, MonitorState::Terminal);
                    state.snapshot.jobs[index] = job.clone();
                    Some(UiEvent::JobUpdated { job })
                });
            }
            ServiceEvent::JobRecoveryWarning { message, .. } => {
                broadcast(
                    &shared,
                    UiEvent::Notice {
                        tone: "warning".into(),
                        message: safe_shared_message(&shared, &message),
                    },
                );
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
                remote_continues,
                ..
            } => {
                // Older or non-job errors do not carry this hint. Preserve
                // the prior recoverability convention only as a fallback;
                // task failures now report the provider's terminal state.
                let remote_continues = remote_continues.unwrap_or(recoverable);
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
                        let key = state.jobs.get(&id).map(|grant| grant.key.clone());
                        let monitor_stop_pending = key.as_ref().is_some_and(|key| {
                            let was_active = state.active_monitors.remove(key);
                            let was_pausing = state.pausing_monitors.remove(key).is_some();
                            let was_stopping = state.stopping_monitors.contains(key);
                            was_active || was_pausing || was_stopping
                        });
                        if let Some(key) = &key {
                            if recoverable && monitor_stop_pending {
                                state.stopping_monitors.insert(key.clone());
                            } else {
                                state.stopping_monitors.remove(key);
                            }
                        }
                        let index = state.snapshot.jobs.iter().position(|job| job.id == id)?;
                        let mut job = state.snapshot.jobs[index].clone();
                        if job.monitor_state == MonitorState::Terminal {
                            return Some(UiEvent::JobUpdated { job });
                        }
                        job.status = "attention".into();
                        job.status_label = "Generation needs attention".into();
                        job.detail = message.clone();
                        if recoverable {
                            if monitor_stop_pending {
                                set_recovery_stop_pending(&mut job, remote_continues);
                            } else {
                                set_monitor_state(&mut job, MonitorState::Recoverable);
                                job.remote_continues = Some(remote_continues);
                            }
                            job.deletable = false;
                        } else {
                            set_monitor_state(&mut job, MonitorState::Terminal);
                            job.remote_continues = Some(remote_continues);
                        }
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

async fn await_credential_ack(reply: oneshot::Receiver<Result<(), String>>) -> Result<(), String> {
    // Provider validation has its own finite HTTP deadline and retry policy.
    // Do not report a timeout while the actor can still persist the key later.
    reply
        .await
        .map_err(|_| "The credential service stopped before confirming the change.".to_string())?
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
        // There is one production renderer. Replacing its channel on reload
        // prevents disconnected WebView subscriptions from accumulating.
        shared.channels.clear();
        shared.channels.push(on_event);
        shared.channel_generation = shared.channel_generation.saturating_add(1);
        let start = !shared.opened;
        shared.opened = true;
        (
            OpenSessionResult {
                seq: shared.seq,
                snapshot: shared.snapshot.clone(),
                preparing: !shared.preparation_ops.is_empty(),
                submitting: !shared.submission_ops.is_empty(),
            },
            start,
        )
    };
    if start {
        let startup_commands = [
            ServiceCommand::LoadHistory {
                op_id: next_op_id(),
                limit: HISTORY_LIMIT,
            },
            ServiceCommand::LoadDraft {
                op_id: next_op_id(),
            },
            ServiceCommand::RefreshCatalog {
                op_id: next_op_id(),
                provider_id: ProviderId::openrouter(),
            },
            ServiceCommand::RefreshCatalog {
                op_id: next_op_id(),
                provider_id: ProviderId::fal(),
            },
        ];
        for command in startup_commands {
            if let Err(error) = queue(&state, command) {
                if let Ok(mut shared) = state.shared.lock() {
                    shared.opened = false;
                }
                return Err(error);
            }
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
        preparing: !shared.preparation_ops.is_empty(),
        submitting: !shared.submission_ops.is_empty(),
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
    await_credential_ack(reply_rx).await
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
    await_credential_ack(reply_rx).await
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
    local_media_upload_confirmed: Option<bool>,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let local_media_upload_confirmed = local_media_upload_confirmed.unwrap_or(false);
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
    let upload_plan = local_media_upload_plan(&core.provider_id, has_local);
    if upload_plan.requires_consent && !local_media_upload_confirmed {
        return Err("Local-media upload was not confirmed. No files were uploaded.".into());
    }
    let staging_provider_id = upload_plan.staging_provider_id;
    let op_id = next_op_id();
    shared.pending_drafts.insert(op_id, draft.clone());
    if let Some(disclosure) = upload_plan.disclosure {
        shared.pending_disclosures.insert(op_id, disclosure.into());
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

async fn save_draft_inner(draft: GenerationDraft, state: &DesktopState) -> Result<(), String> {
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
        state,
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
    reply_rx
        .await
        .map_err(|_| "The draft service stopped before confirming the save.".to_string())?
}

#[tauri::command]
async fn save_draft(draft: GenerationDraft, state: State<'_, DesktopState>) -> Result<(), String> {
    save_draft_inner(draft, &state).await
}

#[tauri::command]
fn acknowledge_close_request(
    request_id: u64,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let acknowledged = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .acknowledge_close_flush(request_id);
    acknowledged
        .then_some(())
        .ok_or_else(|| "That close request is no longer current.".to_string())
}

#[tauri::command]
fn cancel_close_request(request_id: u64, state: State<'_, DesktopState>) -> Result<(), String> {
    let cancelled = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .cancel_close_flush(request_id);
    cancelled
        .then_some(())
        .ok_or_else(|| "That close request is no longer current.".to_string())
}

#[tauri::command]
async fn save_draft_and_close(
    app: AppHandle,
    draft: GenerationDraft,
    request_id: u64,
    state: State<'_, DesktopState>,
) -> Result<(), String> {
    let watchdog_generation = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .begin_close_save(request_id)
        .ok_or_else(|| "That close request is no longer current.".to_string())?;
    spawn_close_flush_watchdog(
        app.clone(),
        request_id,
        watchdog_generation,
        CLOSE_FLUSH_TIMEOUT,
    );
    if let Err(error) = save_draft_inner(draft, &state).await {
        if let Ok(mut shared) = state.shared.lock() {
            shared.suspend_failed_close_save(request_id, watchdog_generation);
        }
        return Err(error);
    }
    let finished = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?
        .finish_close_flush(request_id, watchdog_generation);
    if !finished {
        return Err("That close request is no longer current.".into());
    }
    request_safe_shutdown(&app);
    Ok(())
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
    let job = shared
        .snapshot
        .jobs
        .iter()
        .find(|job| job.id == job_id)
        .ok_or_else(|| "That video job is no longer in My videos.".to_string())?;
    if resume && !job.can_resume {
        return Err("That job is not currently resumable.".into());
    }
    if !resume && !job.can_pause {
        return Err("That job is not currently being monitored.".into());
    }
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

fn bulk_job_command(state: &DesktopState, resume: bool) -> Result<ServiceCommand, String> {
    let shared = state
        .shared
        .lock()
        .map_err(|_| "The desktop session is unavailable.".to_string())?;
    let eligible = shared.snapshot.jobs.iter().any(|job| {
        if resume {
            job.can_resume
        } else {
            job.can_pause
        }
    });
    if !eligible {
        return Err(if resume {
            "There are no paused or recoverable jobs to resume."
        } else {
            "There are no active monitors to pause."
        }
        .into());
    }
    Ok(if resume {
        ServiceCommand::ResumeAll {
            op_id: next_op_id(),
        }
    } else {
        ServiceCommand::PauseAll {
            op_id: next_op_id(),
        }
    })
}

#[tauri::command]
fn pause_all_jobs(state: State<'_, DesktopState>) -> Result<(), String> {
    queue(&state, bulk_job_command(&state, false)?)
}

#[tauri::command]
fn resume_all_jobs(state: State<'_, DesktopState>) -> Result<(), String> {
    queue(&state, bulk_job_command(&state, true)?)
}

fn verified_output(shared: &Shared, job_id: &str) -> Result<PathBuf, String> {
    if shared.deletion_pending.contains(job_id) {
        return Err("This video job is being removed from My videos.".into());
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
            .ok_or_else(|| "That video job is no longer in My videos.".to_string())?;
        if !job.deletable {
            return Err("Only finished video jobs can be removed from My videos.".into());
        }
        if shared.deletion_pending.contains(&job_id) {
            return Err("That video job is already being removed.".into());
        }
        if shared
            .playback_grants
            .values()
            .any(|grant| grant.job_id == job_id)
        {
            return Err("Stop the in-app video before removing this video job.".into());
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

    reply_rx
        .await
        .map_err(|_| "The background service stopped before confirming cleanup.".to_string())?
}

#[tauri::command]
fn open_output(job_id: String, state: State<'_, DesktopState>) -> Result<(), String> {
    let path = {
        let shared = state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())?;
        verified_output(&shared, &job_id)?
    };
    // The plugin's free Rust API accepts an OS path. Avoid converting through
    // UTF-8 so Windows UTF-16 paths and unusual Unix file names remain exact.
    tauri_plugin_opener::open_path(&path, None::<&str>)
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

/// Materialize a renderer-scoped playback file without ever overwriting an
/// existing path. Hard links are effectively free, but Windows users often
/// redirect Videos to another volume and macOS users may keep it on external
/// storage. In those cases a private cache copy keeps in-app playback working.
fn materialize_playback_file(source: &Path, target: &Path) -> io::Result<()> {
    match std::fs::hard_link(source, target) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(error),
        Err(_) => {}
    }

    copy_playback_file(source, target)
}

fn copy_playback_file(source: &Path, target: &Path) -> io::Result<()> {
    let mut created = false;
    let result = (|| -> io::Result<()> {
        let mut input = std::fs::File::open(source)?;
        let expected_length = input.metadata()?.len();
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        created = true;
        let copied = io::copy(&mut input, &mut output)?;
        if copied != expected_length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the playback copy changed length while it was being prepared",
            ));
        }
        output.sync_all()
    })();
    if result.is_err() && created {
        let _ = remove_playback_file(target);
    }
    result
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
async fn grant_playback(
    app: AppHandle,
    job_id: String,
    state: State<'_, DesktopState>,
) -> Result<PlaybackGrant, String> {
    let (grant_id, source, link) = {
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
        shared.playback_grants.insert(
            grant_id.clone(),
            PlaybackGrantState {
                path: link.clone(),
                job_id: job_id.clone(),
            },
        );
        (grant_id, source, link)
    };

    let source_for_copy = source.clone();
    let link_for_copy = link.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        materialize_playback_file(&source_for_copy, &link_for_copy)
    })
    .await
    .map_err(|_| io::Error::other("the playback worker stopped unexpectedly"))
    .and_then(|result| result);
    if prepared.is_err() {
        // `materialize_playback_file` removes only a partial it created. Do
        // not remove `link` here: an AlreadyExists failure means that path
        // predated this request and is not ours to destroy.
        state
            .shared
            .lock()
            .map_err(|_| "The desktop session is unavailable.".to_string())
            .map(|mut shared| {
                if shared
                    .playback_grants
                    .get(&grant_id)
                    .is_some_and(|registered| registered.path == link)
                {
                    shared.playback_grants.remove(&grant_id);
                }
            })?;
        return Err("Secure inline playback could not be prepared; use Open file instead.".into());
    }
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
    fn every_local_media_path_requires_provider_specific_upload_consent() {
        let direct_fal = local_media_upload_plan(&ProviderId::fal(), true);
        assert!(direct_fal.requires_consent);
        assert_eq!(direct_fal.staging_provider_id, None);
        assert_eq!(direct_fal.disclosure, Some(DIRECT_FAL_DISCLOSURE));

        let openrouter = local_media_upload_plan(&ProviderId::openrouter(), true);
        assert!(openrouter.requires_consent);
        assert_eq!(openrouter.staging_provider_id, Some(ProviderId::fal()));
        assert_eq!(openrouter.disclosure, Some(CROSS_PROVIDER_DISCLOSURE));

        let remote_only = local_media_upload_plan(&ProviderId::fal(), false);
        assert!(!remote_only.requires_consent);
        assert_eq!(remote_only.staging_provider_id, None);
        assert_eq!(remote_only.disclosure, None);
    }

    fn command_set(source: &str, start: &str, end: &str, quoted: bool) -> HashSet<String> {
        // This test's own source contains the invoke-handler marker as a
        // string literal. Select the final occurrence so we parse the real
        // application registration below, not this helper call.
        let section = source
            .rsplit_once(start)
            .unwrap_or_else(|| panic!("missing command-list start marker: {start}"))
            .1
            .split_once(end)
            .unwrap_or_else(|| panic!("missing command-list end marker: {end}"))
            .0;
        section
            .lines()
            .filter_map(|line| {
                let item = line.trim().trim_end_matches(',');
                if item.is_empty() || item.starts_with("//") {
                    return None;
                }
                let item = if quoted {
                    item.strip_prefix('"')?.strip_suffix('"')?
                } else {
                    item
                };
                Some(item.to_owned())
            })
            .collect()
    }

    #[test]
    fn ipc_handler_build_manifest_and_permission_allowlist_stay_in_lockstep() {
        let handler = command_set(
            include_str!("lib.rs"),
            ".invoke_handler(tauri::generate_handler![",
            "])",
            false,
        );
        let build_manifest = command_set(
            include_str!("../build.rs"),
            "const IPC_COMMANDS: &[&str] = &[",
            "];",
            true,
        );
        let permission = command_set(
            include_str!("../permissions/service-facade.toml"),
            "commands.allow = [",
            "]",
            true,
        );

        assert_eq!(
            handler, build_manifest,
            "invoke handler and build ACL differ"
        );
        assert_eq!(handler, permission, "invoke handler and permission differ");
    }

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
    fn close_request_event_is_explicitly_serialized_for_the_renderer() {
        let value = serde_json::to_value(UiEvent::CloseRequested { request_id: 42 })
            .expect("serialize close request");
        assert_eq!(value["type"], "close_requested");
        assert_eq!(value["requestId"], 42);
        assert!(value.get("request_id").is_none());
    }

    #[test]
    fn close_flush_requires_the_current_request_and_can_be_cancelled() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let first = shared.begin_close_flush().expect("begin close request");
        assert_eq!(
            shared.begin_close_flush(),
            None,
            "do not replace an in-flight request"
        );
        assert!(!shared.acknowledge_close_flush(first.saturating_add(1)));
        assert!(shared.acknowledge_close_flush(first));
        let first_save = shared.begin_close_save(first).expect("arm close save");
        assert!(shared.suspend_failed_close_save(first, first_save));
        assert!(
            shared.close_flush_watchdog_generation != first_save,
            "a failed save must invalidate its forced-close watchdog"
        );
        assert!(
            !shared.begin_close_timeout_shutdown(first, Some(first_save), false),
            "a failed save must leave the acknowledged close request open for retry"
        );
        assert!(!shared.cancel_close_flush(first.saturating_add(1)));
        assert!(shared.cancel_close_flush(first));

        let second = shared
            .begin_close_flush()
            .expect("begin replacement request");
        assert_ne!(second, first);
        let second_save = shared
            .begin_close_save(second)
            .expect("arm replacement save");
        assert!(!shared.finish_close_flush(first, first_save));
        assert!(shared.finish_close_flush(second, second_save));
    }

    #[test]
    fn a_later_window_close_reissues_a_pending_request_with_a_fresh_watchdog() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let request_id = shared.begin_close_flush().expect("begin close request");
        assert!(shared.acknowledge_close_flush(request_id));
        let failed_save = shared
            .begin_close_save(request_id)
            .expect("arm failed save");
        assert!(shared.suspend_failed_close_save(request_id, failed_save));
        let suspended_generation = shared.close_flush_watchdog_generation;

        let (reissued_id, reissued_generation) =
            shared.issue_close_flush().expect("reissue close request");
        assert_eq!(reissued_id, request_id);
        assert_ne!(reissued_generation, suspended_generation);
        assert!(shared.close_flush_acknowledged);
    }

    #[test]
    fn a_failed_save_invalidates_a_watchdog_rearmed_while_it_was_running() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let request_id = shared.begin_close_flush().expect("begin close request");
        assert!(shared.acknowledge_close_flush(request_id));
        let failed_save = shared
            .begin_close_save(request_id)
            .expect("arm failed save");

        let (reissued_id, reissued_generation) =
            shared.issue_close_flush().expect("reissue close request");
        assert_eq!(reissued_id, request_id);
        assert_ne!(reissued_generation, failed_save);
        assert!(shared.suspend_failed_close_save(request_id, failed_save));
        assert!(
            !shared.begin_close_timeout_shutdown(request_id, Some(reissued_generation), false),
            "no watchdog may close the application after the save failed"
        );
    }

    #[test]
    fn shutdown_disables_close_flush_state_transitions() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let request_id = shared.begin_close_flush().expect("begin close request");
        assert!(shared.begin_shutdown());

        assert!(!shared.acknowledge_close_flush(request_id));
        assert!(!shared.cancel_close_flush(request_id));
        assert!(!shared.finish_close_flush(request_id, 0));
        assert_eq!(shared.begin_close_flush(), None);
        assert_eq!(shared.close_flush_pending, None);
        assert!(!shared.begin_shutdown());
    }

    fn record_fixture(status: &str, output_path: Option<PathBuf>) -> JobRecord {
        JobRecord {
            provider_id: ProviderId::openrouter(),
            job_id: "job-recovery-fixture".into(),
            polling_url: "/api/v1/videos/job-recovery-fixture".into(),
            locator: video_harness::domain::JobLocator::OpenRouter {
                polling_url: "/api/v1/videos/job-recovery-fixture".into(),
            },
            status: status.into(),
            request: None,
            generation_id: None,
            output_path,
            cost: None,
            currency: None,
            error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn history_jobs_expose_truthful_monitor_capabilities_after_restart() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let pending = record_fixture("pending", None);
        let pending_summary = job_from_record(&mut shared, &pending);
        assert_eq!(pending_summary.monitor_state, MonitorState::Recoverable);
        assert!(pending_summary.can_resume);
        assert!(!pending_summary.can_pause);
        assert!(!pending_summary.deletable);

        shared.active_monitors.insert(pending.key());
        let active_summary = job_from_record(&mut shared, &pending);
        assert_eq!(active_summary.monitor_state, MonitorState::Active);
        assert!(!active_summary.can_resume);
        assert!(active_summary.can_pause);

        shared.active_monitors.clear();
        let completed_without_output = record_fixture("completed", None);
        let completed_summary = job_from_record(&mut shared, &completed_without_output);
        assert_eq!(completed_summary.monitor_state, MonitorState::Recoverable);
        assert!(completed_summary.can_resume);
        assert!(!completed_summary.deletable);

        let completed_with_output = record_fixture(
            "completed",
            Some(std::env::current_exe().expect("test executable path")),
        );
        let downloaded_summary = job_from_record(&mut shared, &completed_with_output);
        assert_eq!(downloaded_summary.monitor_state, MonitorState::Terminal);
        assert!(!downloaded_summary.can_resume);
        assert!(downloaded_summary.deletable);
    }

    #[test]
    fn final_monitor_cancellation_restores_pause_after_a_late_active_update() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let record = record_fixture("processing", None);
        shared.active_monitors.insert(record.key());
        let mut job = job_from_record(&mut shared, &record);
        job.next_poll_seconds = Some(3);
        job.progress = Some(0.5);

        set_pause_pending_projection(&mut job, true);

        assert_eq!(job.monitor_state, MonitorState::Paused);
        assert!(!job.can_resume);
        assert!(!job.can_pause);
        assert_eq!(job.status_label, "Pausing monitoring");
        assert_eq!(job.remote_continues, Some(true));
        assert!(job.next_poll_seconds.is_none());
        assert!(job.progress.is_none());

        set_paused_projection(&mut job, true);

        assert_eq!(job.monitor_state, MonitorState::Paused);
        assert!(job.can_resume);
        assert!(!job.can_pause);
        assert_eq!(job.status, "paused");
        assert_eq!(job.remote_continues, Some(true));
        assert!(job.next_poll_seconds.is_none());
        assert!(job.progress.is_none());

        job.status = "attention".into();
        job.status_label = "Generation needs attention".into();
        set_recovery_stop_pending(&mut job, true);
        assert!(!job.can_resume);
        set_monitor_state(&mut job, MonitorState::Recoverable);
        assert!(job.can_resume);

        set_recovery_stop_pending(&mut job, false);
        assert_eq!(job.remote_continues, Some(false));

        let completed = record_fixture(
            "completed",
            Some(std::env::current_exe().expect("test executable path")),
        );
        let terminal = job_from_record(&mut shared, &completed);
        assert_eq!(terminal.monitor_state, MonitorState::Terminal);
        assert!(
            !pause_projection_allowed(&terminal),
            "a late pause acknowledgement must not regress a terminal job"
        );
    }

    #[test]
    fn pause_all_projection_waits_for_actor_stop_before_enabling_resume() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let record = record_fixture("processing", None);
        let key = record.key();
        shared.active_monitors.insert(key.clone());
        let job = job_from_record(&mut shared, &record);
        shared.snapshot.jobs.push(job);

        assert_eq!(apply_pause_all_projection(&mut shared, true), 1);
        assert!(!shared.active_monitors.contains(&key));
        assert_eq!(shared.pausing_monitors.get(&key), Some(&true));
        let projected = &shared.snapshot.jobs[0];
        assert_eq!(projected.status_label, "Pausing monitoring");
        assert_eq!(projected.monitor_state, MonitorState::Paused);
        assert!(!projected.can_pause);
        assert!(
            !projected.can_resume,
            "only MonitorStopped may make a paused job resumable"
        );
    }

    #[test]
    fn sidecar_only_jobs_are_visible_and_resumable() {
        let mut shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let resumable = ResumableJob {
            key: ProviderJobKey::new(ProviderId::openrouter(), "sidecar-only")
                .expect("provider key"),
            locator: video_harness::domain::JobLocator::OpenRouter {
                polling_url: "/api/v1/videos/sidecar-only".into(),
            },
            accepted_at: Utc::now(),
            monitoring_paused: false,
            completed_output_path: None,
        };

        let summary = job_from_resumable(&mut shared, &resumable);
        assert_eq!(summary.provider_job_id.as_deref(), Some("sidecar-only"));
        assert_eq!(summary.monitor_state, MonitorState::Recoverable);
        assert!(summary.can_resume);
        assert!(shared.jobs.contains_key(&summary.id));
    }

    #[test]
    fn model_summary_exposes_native_constraints_to_the_renderer() {
        let model = VideoModel::from_provider_api(
            ProviderId::fal(),
            &serde_json::json!({
                "id": "fal/constraint-fixture",
                "name": "Constraint fixture",
                "input_modalities": ["image"],
                "supported_frame_images": ["first_frame", "last_frame"],
                "supported_sizes": ["1280x720"],
                "seed": false,
                "field_map": {
                    "first_frame": "image_urls",
                    "last_frame": "end_image_url"
                },
                "input_schema": {
                    "type": "object",
                    "required": ["image_urls"],
                    "allOf": [{"required": ["end_image_url"]}]
                },
                "media_bindings": [{
                    "kind": "image",
                    "property_name": "image_urls",
                    "cardinality": "list",
                    "required": true,
                    "min_items": 1,
                    "max_items": 2
                }]
            }),
        )
        .expect("model fixture");

        let summary = model_summary(&model);
        assert!(!summary.capabilities.seed);
        assert_eq!(summary.size_options, vec!["1280x720"]);
        assert_eq!(
            summary.supported_image_roles,
            vec![
                MediaRole::Reference,
                MediaRole::StartFrame,
                MediaRole::EndFrame
            ]
        );
        assert_eq!(
            summary.required_image_roles,
            vec![MediaRole::StartFrame, MediaRole::EndFrame]
        );
        assert_eq!(summary.media_constraints.len(), 1);
        assert_eq!(
            summary.media_constraints[0].roles,
            vec![MediaRole::Reference, MediaRole::StartFrame]
        );
        assert!(summary.media_constraints[0].required);
        assert_eq!(summary.media_constraints[0].min_items, Some(1));
        assert_eq!(summary.media_constraints[0].min_items_when_present, None);
        assert_eq!(summary.media_constraints[0].max_items, Some(2));
        assert_eq!(summary.max_media_items, Some(MAX_MEDIA_INPUTS_TOTAL));
        assert!(!summary.audio_requires_visual);
        assert!(!summary.frames_exclusive_with_references);

        let optional = VideoModel::from_provider_api(
            ProviderId::fal(),
            &serde_json::json!({
                "id": "fal/optional-media-fixture",
                "name": "Optional media fixture",
                "input_modalities": ["image"],
                "media_bindings": [{
                    "kind": "image",
                    "property_name": "image_urls",
                    "cardinality": "list",
                    "required": false,
                    "min_items": 2,
                    "max_items": 3
                }]
            }),
        )
        .expect("optional model fixture");
        let optional_summary = model_summary(&optional);
        assert!(!optional_summary.media_constraints[0].required);
        assert_eq!(optional_summary.media_constraints[0].min_items, None);
        assert_eq!(
            optional_summary.media_constraints[0].min_items_when_present,
            Some(2)
        );
        assert_eq!(optional_summary.media_constraints[0].max_items, Some(3));
        let serialized = serde_json::to_value(&optional_summary).expect("serialize model summary");
        assert_eq!(
            serialized["mediaConstraints"][0]["minItemsWhenPresent"],
            serde_json::json!(2)
        );

        let frame_only = VideoModel::from_provider_api(
            ProviderId::fal(),
            &serde_json::json!({
                "id": "fal/frame-only-fixture",
                "name": "Frame-only fixture",
                "input_modalities": [],
                "supported_frame_images": ["first_frame"],
                "field_map": { "first_frame": "start_image_url" }
            }),
        )
        .expect("frame-only model fixture");
        let frame_only_summary = model_summary(&frame_only);
        assert!(frame_only_summary.capabilities.images);
        assert!(!frame_only_summary.capabilities.seed);
        assert_eq!(
            frame_only_summary.supported_image_roles,
            vec![MediaRole::StartFrame]
        );

        let openrouter = VideoModel::from_provider_api(
            ProviderId::openrouter(),
            &serde_json::json!({
                "id": "bytedance/seedance-2.0",
                "name": "OpenRouter policy fixture",
                "input_modalities": ["image", "video", "audio"],
                "supported_frame_images": ["first_frame"]
            }),
        )
        .expect("OpenRouter policy fixture");
        let openrouter_summary = model_summary(&openrouter);
        assert_eq!(
            openrouter_summary.max_media_items,
            Some(MAX_MEDIA_INPUTS_TOTAL)
        );
        assert!(openrouter_summary.audio_requires_visual);
        assert!(openrouter_summary.frames_exclusive_with_references);
        assert_eq!(
            openrouter_summary
                .media_constraints
                .iter()
                .find(|constraint| constraint.kind == MediaKind::Image)
                .and_then(|constraint| constraint.max_items),
            Some(MAX_IMAGE_INPUTS)
        );

        let explicit_high = VideoModel::from_provider_api(
            ProviderId::fal(),
            &serde_json::json!({
                "id": "fal/custom-many-images",
                "name": "High schema maximum fixture",
                "input_modalities": ["image"],
                "media_bindings": [{
                    "kind": "image",
                    "property_name": "image_urls",
                    "cardinality": "list",
                    "max_items": 20
                }]
            }),
        )
        .expect("high maximum fixture");
        let explicit_high_summary = model_summary(&explicit_high);
        assert_eq!(explicit_high_summary.max_media_items, None);
        assert_eq!(
            explicit_high_summary.media_constraints[0].max_items,
            Some(20)
        );
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
            adapter_options: Some(serde_json::json!({"guidance": 4})),
            typed_seed: Some(23),
            editor_state,
        });
        draft.settings.size = "1280x720".into();
        draft.settings.advanced_json = r#"{"guidance": 4}"#.into();

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

        draft.settings.seed = "23".into();
        draft.settings.advanced_json = "{}".into();
        let cleared = core_draft(&shared, &draft, DraftPurpose::Review)
            .expect("visible advanced settings can be cleared");
        assert_eq!(cleared.size.as_deref(), Some("1280x720"));
        assert!(cleared.adapter_options.is_none());

        draft.settings.advanced_json = "{half-written".into();
        let autosaved = core_draft(&shared, &draft, DraftPurpose::Autosave)
            .expect("autosave retains the last typed advanced object");
        assert_eq!(
            autosaved.adapter_options,
            Some(serde_json::json!({"guidance": 4}))
        );
        assert_eq!(
            editor_state_for_draft(&shared, &draft).advanced_json_text,
            "{half-written"
        );
        assert!(core_draft(&shared, &draft, DraftPurpose::Review).is_err());
    }

    #[test]
    fn review_rejects_credential_fields_in_advanced_settings() {
        let shared = Shared::new(PathBuf::from("videos"), PathBuf::from("playback"));
        let mut draft = GenerationDraft {
            model_id: "fixture/model".into(),
            prompt: "A fixture prompt".into(),
            ..GenerationDraft::default()
        };
        draft.settings.advanced_json =
            r#"{"nested":{"client_secret":"must-never-enter-a-review"}}"#.into();

        let error = core_draft(&shared, &draft, DraftPurpose::Review)
            .expect_err("credential-like fields must fail before preparation");
        assert!(error.contains("credential"));
        assert!(!error.contains("must-never-enter-a-review"));
        assert!(core_draft(&shared, &draft, DraftPurpose::Autosave).is_err());

        draft.settings.advanced_json = r#"{"max_tokens":128,"token_count":4}"#.into();
        assert!(core_draft(&shared, &draft, DraftPurpose::Review).is_ok());
    }

    #[test]
    fn playback_copy_fallback_is_complete_and_never_overwrites() {
        let root = std::env::temp_dir().join(format!("video-harness-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create fixture directory");
        let source = root.join("finished.mp4");
        let target = root.join("playback-copy.mp4");
        let occupied = root.join("playback-owned-by-someone-else.mp4");
        std::fs::write(&source, b"cross-volume-video-fixture").expect("create source video");

        copy_playback_file(&source, &target).expect("copy playback fallback");
        assert_eq!(
            std::fs::read(&target).expect("read playback copy"),
            b"cross-volume-video-fixture"
        );

        std::fs::write(&occupied, b"keep-this-file").expect("create occupied path");
        assert_eq!(
            copy_playback_file(&source, &occupied)
                .expect_err("an existing path must not be overwritten")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read(&occupied).expect("read occupied path"),
            b"keep-this-file"
        );

        std::fs::remove_dir_all(root).expect("remove fixture directory");
    }

    #[test]
    fn playback_urls_match_the_tauri_asset_protocol_for_this_platform() {
        let path = Path::new("Video Harness/test clip.mp4");
        let url = playback_url(path);
        if cfg!(windows) {
            assert_eq!(
                url,
                "http://asset.localhost/Video%20Harness%2Ftest%20clip%2Emp4"
            );
        } else {
            assert_eq!(url, "asset://localhost/Video%20Harness%2Ftest%20clip%2Emp4");
        }
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

fn dispatch_safe_shutdown(app: &AppHandle, state: &DesktopState) {
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

fn request_safe_shutdown(app: &AppHandle) {
    let Some(state) = app.try_state::<DesktopState>() else {
        return;
    };
    let should_request = state
        .shared
        .lock()
        .ok()
        .is_some_and(|mut shared| shared.begin_shutdown());
    if should_request {
        dispatch_safe_shutdown(app, &state);
    }
}

fn request_close_timeout_shutdown(
    app: &AppHandle,
    request_id: u64,
    watchdog_generation: Option<u64>,
    require_unacknowledged: bool,
) -> bool {
    let Some(state) = app.try_state::<DesktopState>() else {
        return false;
    };
    let should_request = state.shared.lock().ok().is_some_and(|mut shared| {
        shared.begin_close_timeout_shutdown(request_id, watchdog_generation, require_unacknowledged)
    });
    if should_request {
        dispatch_safe_shutdown(app, &state);
    }
    should_request
}

fn spawn_close_flush_watchdog(
    app: AppHandle,
    request_id: u64,
    watchdog_generation: u64,
    timeout: std::time::Duration,
) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(timeout).await;
        request_close_timeout_shutdown(&app, request_id, Some(watchdog_generation), false);
    });
}

fn request_close_flush(app: &AppHandle) {
    let Some(state) = app.try_state::<DesktopState>() else {
        return;
    };
    let close_request = state.shared.lock().ok().and_then(|mut shared| {
        (shared.opened && !shared.channels.is_empty() && !shared.shutdown_requested)
            .then(|| shared.issue_close_flush())
            .flatten()
    });
    let has_renderer_or_pending_request = state.shared.lock().ok().is_some_and(|shared| {
        shared.opened
            && !shared.channels.is_empty()
            && !shared.shutdown_requested
            && (close_request.is_some() || shared.close_flush_pending.is_some())
    });
    let Some((request_id, watchdog_generation)) = close_request else {
        if !has_renderer_or_pending_request {
            request_safe_shutdown(app);
        }
        return;
    };
    if !broadcast(&state.shared, UiEvent::CloseRequested { request_id }) {
        request_safe_shutdown(app);
        return;
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(CLOSE_ACK_TIMEOUT).await;
        // Claim shutdown atomically with respect to a late renderer
        // acknowledgement or Keep-working cancellation.
        if request_close_timeout_shutdown(&app, request_id, None, true) {
            return;
        }
        tokio::time::sleep(CLOSE_FLUSH_TIMEOUT.saturating_sub(CLOSE_ACK_TIMEOUT)).await;
        request_close_timeout_shutdown(&app, request_id, Some(watchdog_generation), false);
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
            // Linux releases keep their historical XDG storage identity so
            // history, drafts, upload receipts, and keyring records remain
            // available across desktop-shell upgrades.
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
            acknowledge_close_request,
            cancel_close_request,
            save_draft_and_close,
            pause_job,
            resume_job,
            pause_all_jobs,
            resume_all_jobs,
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
            request_close_flush(app);
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
                request_close_flush(app);
            }
        }
        _ => {}
    });
}
