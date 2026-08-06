//! Reducer, state, and runtime orchestration for the native terminal UI.
//!
//! The reducer is intentionally independent of HTTP, SQLite, and keyring code.
//! It emits typed effects which the service bridge at the bottom of this module
//! executes. This makes paid-submission invariants directly testable.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::domain::{
    CostEstimate, FrameImage, FrameType, InputReference, JobLocator, ProviderId, ProviderJobKey,
    VideoModel, VideoRequest,
};
use crate::history::JobRecord;
use crate::ui_input::{SecretEditor, TextEditor};

pub const WIDE_BREAKPOINT: u16 = 80;
pub const NARROW_BREAKPOINT: u16 = 60;
pub const SHORT_BREAKPOINT: u16 = 28;
pub const MINIMUM_WIDTH: u16 = 40;
pub const MINIMUM_HEIGHT: u16 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Onboarding,
    Compose,
    Progress,
    Complete,
    History,
    Providers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Stacked,
    Compact,
    TooSmall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderConnectionKind {
    Connected,
    SessionOnly,
    #[default]
    NeedsKey,
}

impl ProviderConnectionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::SessionOnly => "Session-only",
            Self::NeedsKey => "Needs key",
        }
    }

    pub const fn has_key(self) -> bool {
        !matches!(self, Self::NeedsKey)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiProvider {
    pub id: ProviderId,
    pub name: String,
    pub connection: ProviderConnectionKind,
    pub storage_note: String,
}

impl UiProvider {
    pub fn openrouter() -> Self {
        Self {
            id: ProviderId::openrouter(),
            name: "OpenRouter".into(),
            connection: ProviderConnectionKind::NeedsKey,
            storage_note: String::new(),
        }
    }

    pub fn fal() -> Self {
        Self {
            id: ProviderId::fal(),
            name: "fal.ai".into(),
            connection: ProviderConnectionKind::NeedsKey,
            storage_note: String::new(),
        }
    }
}

pub fn provider_name(provider_id: &ProviderId) -> &'static str {
    match provider_id.as_str() {
        "fal" => "fal.ai",
        "openrouter" => "OpenRouter",
        _ => "Video provider",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub title: String,
    pub message: String,
    pub severity: Severity,
    pub expires_at: Option<Instant>,
}

impl Toast {
    pub fn timed(
        title: impl Into<String>,
        message: impl Into<String>,
        severity: Severity,
        now: Instant,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            severity,
            expires_at: Some(now + Duration::from_secs(7)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub unicode: bool,
    pub color: bool,
    pub reduced_motion: bool,
}

impl TerminalCapabilities {
    pub fn detect() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let encoding = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_CTYPE"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default();
        Self {
            unicode: term != "dumb"
                && encoding.to_ascii_lowercase().contains("utf")
                && std::env::var_os("OPENROUTER_VIDEO_ASCII").is_none(),
            color: term != "dumb" && std::env::var_os("NO_COLOR").is_none(),
            reduced_motion: std::env::var_os("OPENROUTER_VIDEO_REDUCED_MOTION").is_some(),
        }
    }
}

/// Secret passed across the reducer boundary. It cannot be formatted and zeros
/// its owned bytes on drop.
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(value.into_bytes())
    }

    pub fn expose(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.0)
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSettings {
    pub duration: Option<u32>,
    pub resolution: Option<String>,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    pub generate_audio: Option<bool>,
    pub seed: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiModel {
    pub provider_id: ProviderId,
    pub id: String,
    pub name: String,
    pub description: String,
    pub durations: Vec<u32>,
    pub resolutions: Vec<String>,
    pub aspect_ratios: Vec<String>,
    pub sizes: Vec<String>,
    pub frame_types: Vec<String>,
    pub generate_audio: Option<bool>,
    pub seed: Option<bool>,
    pub passthrough_parameters: Vec<String>,
    pub pricing: BTreeMap<String, String>,
    pub input_schema: Option<serde_json::Value>,
    pub field_map: BTreeMap<String, String>,
}

impl UiModel {
    pub fn supports_audio(&self) -> bool {
        self.generate_audio == Some(true)
    }

    pub fn supports_frame(&self, frame_type: &str) -> bool {
        self.frame_types.iter().any(|value| value == frame_type)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CostView {
    pub amount: Option<String>,
    pub currency: String,
    pub basis: String,
    pub exact: bool,
    pub raw_pricing: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestDraft {
    pub provider_id: ProviderId,
    pub model: String,
    pub prompt: String,
    pub duration: Option<u32>,
    pub resolution: Option<String>,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    pub generate_audio: Option<bool>,
    pub seed: Option<i64>,
    pub first_frame: Option<String>,
    pub last_frame: Option<String>,
    pub references: Vec<String>,
    pub adapter_options: Option<String>,
}

impl RequestDraft {
    pub fn settings(&self) -> ModelSettings {
        ModelSettings {
            duration: self.duration,
            resolution: self.resolution.clone(),
            aspect_ratio: self.aspect_ratio.clone(),
            size: self.size.clone(),
            generate_audio: self.generate_audio,
            seed: self.seed,
        }
    }

    pub fn into_domain(self) -> Result<VideoRequest, String> {
        let mut request = VideoRequest::for_provider(self.provider_id, self.model, self.prompt)
            .map_err(|error| error.to_string())?;
        request.duration = self.duration;
        request.resolution = self.resolution;
        request.aspect_ratio = self.aspect_ratio;
        request.size = self.size;
        request.generate_audio = self.generate_audio;
        request.seed = self.seed;
        if let Some(url) = self.first_frame {
            request.frame_images.push(
                FrameImage::new(url, FrameType::FirstFrame).map_err(|error| error.to_string())?,
            );
        }
        if let Some(url) = self.last_frame {
            request.frame_images.push(
                FrameImage::new(url, FrameType::LastFrame).map_err(|error| error.to_string())?,
            );
        }
        request.input_references = self
            .references
            .into_iter()
            .map(|url| InputReference::new(url).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        request.adapter_options = self
            .adapter_options
            .map(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))
            .transpose()?;
        request.validate().map_err(|error| error.to_string())?;
        Ok(request)
    }

    pub fn from_domain(request: &VideoRequest) -> Self {
        let first_frame = request
            .frame_images
            .iter()
            .find(|frame| frame.frame_type == FrameType::FirstFrame)
            .map(|frame| frame.url.clone());
        let last_frame = request
            .frame_images
            .iter()
            .find(|frame| frame.frame_type == FrameType::LastFrame)
            .map(|frame| frame.url.clone());
        Self {
            provider_id: request.provider_id.clone(),
            model: request.model.clone(),
            prompt: request.prompt.clone(),
            duration: request.duration,
            resolution: request.resolution.clone(),
            aspect_ratio: request.aspect_ratio.clone(),
            size: request.size.clone(),
            generate_audio: request.generate_audio,
            seed: request.seed,
            first_frame,
            last_frame,
            references: request
                .input_references
                .iter()
                .map(|reference| reference.url.clone())
                .collect(),
            adapter_options: request
                .adapter_options
                .as_ref()
                .and_then(|value| serde_json::to_string(value).ok()),
        }
    }
}

impl From<&VideoModel> for UiModel {
    fn from(model: &VideoModel) -> Self {
        Self {
            provider_id: model.provider_id.clone(),
            id: model.id.clone(),
            name: model.name.clone(),
            description: model.description.clone(),
            durations: model.supported_durations.clone(),
            resolutions: model.supported_resolutions.clone(),
            aspect_ratios: model.supported_aspect_ratios.clone(),
            sizes: model.supported_sizes.clone(),
            frame_types: model.supported_frame_images.clone(),
            generate_audio: model.generate_audio,
            seed: model.seed,
            passthrough_parameters: model.allowed_passthrough_parameters.clone(),
            pricing: model
                .pricing_skus
                .iter()
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect(),
            input_schema: model.input_schema.clone(),
            field_map: model.field_map.clone(),
        }
    }
}

impl From<&UiModel> for VideoModel {
    fn from(model: &UiModel) -> Self {
        Self {
            provider_id: model.provider_id.clone(),
            id: model.id.clone(),
            name: model.name.clone(),
            description: model.description.clone(),
            canonical_slug: None,
            created: None,
            supported_resolutions: model.resolutions.clone(),
            supported_aspect_ratios: model.aspect_ratios.clone(),
            supported_sizes: model.sizes.clone(),
            supported_durations: model.durations.clone(),
            supported_frame_images: model.frame_types.clone(),
            generate_audio: model.generate_audio,
            seed: model.seed,
            allowed_passthrough_parameters: model.passthrough_parameters.clone(),
            pricing_skus: model
                .pricing
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .parse::<rust_decimal::Decimal>()
                        .ok()
                        .map(|value| (key.clone(), value))
                })
                .collect(),
            input_schema: model.input_schema.clone(),
            field_map: model.field_map.clone(),
            raw: serde_json::Value::Null,
        }
    }
}

impl From<&CostEstimate> for CostView {
    fn from(estimate: &CostEstimate) -> Self {
        Self {
            amount: estimate.amount.map(|amount| amount.to_string()),
            currency: estimate.currency.clone(),
            basis: estimate.basis.clone(),
            exact: estimate.exact,
            raw_pricing: estimate
                .raw_pricing
                .iter()
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect(),
        }
    }
}

impl From<&JobRecord> for HistoryItem {
    fn from(record: &JobRecord) -> Self {
        let request = record.request.as_ref().map(RequestDraft::from_domain);
        Self {
            provider_id: record.provider_id.clone(),
            job_id: record.job_id.clone(),
            created: record
                .created_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            status: record.status.clone(),
            model: record
                .request
                .as_ref()
                .map(|request| request.model.clone())
                .unwrap_or_else(|| "Imported job".into()),
            prompt: record
                .request
                .as_ref()
                .map(|request| request.prompt.clone())
                .unwrap_or_default(),
            cost: record.cost.map(|cost| cost.to_string()),
            currency: record.currency.clone(),
            output_path: record.output_path.clone(),
            error: record.error.clone(),
            request,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryItem {
    pub provider_id: ProviderId,
    pub job_id: String,
    pub created: String,
    pub status: String,
    pub model: String,
    pub prompt: String,
    pub cost: Option<String>,
    pub currency: Option<String>,
    pub output_path: Option<PathBuf>,
    pub error: Option<String>,
    pub request: Option<RequestDraft>,
}

impl HistoryItem {
    pub fn key(&self) -> Option<ProviderJobKey> {
        ProviderJobKey::new(self.provider_id.clone(), self.job_id.clone()).ok()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub provider_id: ProviderId,
    pub job_id: String,
    pub path: PathBuf,
    pub cost: Option<String>,
    pub currency: Option<String>,
    pub request: Option<RequestDraft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposeFocus {
    #[default]
    Prompt,
    Provider,
    Model,
    Duration,
    Resolution,
    AspectRatio,
    Size,
    Audio,
    Seed,
    AdvancedToggle,
    FirstFrame,
    LastFrame,
    References,
    AdapterOptions,
    Generate,
    History,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPicker {
    pub provider_id: ProviderId,
    pub query: TextEditor,
    pub filtered: Vec<usize>,
    pub selected: usize,
}

impl Default for ModelPicker {
    fn default() -> Self {
        Self {
            provider_id: ProviderId::openrouter(),
            query: TextEditor::line(),
            filtered: Vec::new(),
            selected: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCatalogState {
    pub models: Vec<UiModel>,
    pub loading: bool,
    pub stale: bool,
    pub message: String,
}

pub struct OnboardingState {
    pub key: SecretEditor,
    pub validating: bool,
    pub status: String,
    pub severity: Severity,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            key: SecretEditor::default(),
            validating: false,
            status: String::new(),
            severity: Severity::Info,
        }
    }
}

pub struct ProviderManagementState {
    pub providers: Vec<UiProvider>,
    pub selected: usize,
    pub key: SecretEditor,
    pub editing_key: bool,
    pub validating: Option<ProviderId>,
    pub status: String,
    pub severity: Severity,
    pub return_route: Route,
}

impl Default for ProviderManagementState {
    fn default() -> Self {
        Self {
            providers: vec![UiProvider::openrouter(), UiProvider::fal()],
            selected: 0,
            key: SecretEditor::default(),
            editing_key: false,
            validating: None,
            status: "Connect either provider, or keep both available.".into(),
            severity: Severity::Info,
            return_route: Route::Compose,
        }
    }
}

impl ProviderManagementState {
    pub fn selected(&self) -> Option<&UiProvider> {
        self.providers.get(self.selected)
    }

    pub fn selected_mut(&mut self) -> Option<&mut UiProvider> {
        self.providers.get_mut(self.selected)
    }

    pub fn get(&self, provider_id: &ProviderId) -> Option<&UiProvider> {
        self.providers
            .iter()
            .find(|provider| &provider.id == provider_id)
    }

    pub fn get_mut(&mut self, provider_id: &ProviderId) -> Option<&mut UiProvider> {
        self.providers
            .iter_mut()
            .find(|provider| &provider.id == provider_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeState {
    pub prompt: TextEditor,
    pub provider_id: ProviderId,
    pub models: Vec<UiModel>,
    pub catalogs: HashMap<ProviderId, ProviderCatalogState>,
    pub model_index: Option<usize>,
    pub picker: Option<ModelPicker>,
    pub duration_index: Option<usize>,
    pub resolution_index: Option<usize>,
    pub aspect_index: Option<usize>,
    pub size_index: Option<usize>,
    pub audio: bool,
    pub seed: TextEditor,
    pub advanced: bool,
    pub first_frame: TextEditor,
    pub last_frame: TextEditor,
    pub references: TextEditor,
    pub adapter_options: TextEditor,
    pub focus: ComposeFocus,
    pub scroll: u16,
    pub catalog_loading: bool,
    pub catalog_stale: bool,
    pub catalog_message: String,
    pub estimate: CostView,
    pub remembered: HashMap<(ProviderId, String), ModelSettings>,
}

impl Default for ComposeState {
    fn default() -> Self {
        Self {
            prompt: TextEditor::multiline(),
            provider_id: ProviderId::openrouter(),
            models: Vec::new(),
            catalogs: HashMap::new(),
            model_index: None,
            picker: None,
            duration_index: None,
            resolution_index: None,
            aspect_index: None,
            size_index: None,
            audio: false,
            seed: TextEditor::line(),
            advanced: false,
            first_frame: TextEditor::line(),
            last_frame: TextEditor::line(),
            references: TextEditor::multiline(),
            adapter_options: TextEditor::multiline(),
            focus: ComposeFocus::Prompt,
            scroll: 0,
            catalog_loading: false,
            catalog_stale: false,
            catalog_message: String::new(),
            estimate: CostView::default(),
            remembered: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenerationState {
    pub provider_id: ProviderId,
    pub request: Option<RequestDraft>,
    pub job_id: Option<String>,
    pub status: String,
    pub detail: String,
    pub countdown: Option<u64>,
    pub poll_deadline: Option<Instant>,
    pub started_at: Instant,
    pub phase: u64,
    pub download_received: u64,
    pub download_total: Option<u64>,
    pub error: Option<String>,
    pub submitting: bool,
    pub monitoring: bool,
}

impl Default for GenerationState {
    fn default() -> Self {
        Self {
            provider_id: ProviderId::openrouter(),
            request: None,
            job_id: None,
            status: "Preparing".into(),
            detail: "Warming up the projector…".into(),
            countdown: None,
            poll_deadline: None,
            started_at: Instant::now(),
            phase: 0,
            download_received: 0,
            download_total: None,
            error: None,
            submitting: false,
            monitoring: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryState {
    pub items: Vec<HistoryItem>,
    pub selected: usize,
    pub loading: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImportFocus {
    #[default]
    Locator,
    RequestId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDraft {
    pub provider_id: ProviderId,
    pub locator: TextEditor,
    pub request_id: TextEditor,
    pub focus: ImportFocus,
}

impl ImportDraft {
    pub fn new(provider_id: ProviderId) -> Self {
        Self {
            provider_id,
            locator: TextEditor::line(),
            request_id: TextEditor::line(),
            focus: ImportFocus::Locator,
        }
    }

    pub fn into_locator(&self) -> Result<JobLocator, String> {
        let primary = self.locator.trimmed();
        if primary.is_empty() {
            return Err(if self.provider_id == ProviderId::fal() {
                "Enter a fal queue URL or endpoint ID.".into()
            } else {
                "Enter an OpenRouter job ID or polling URL.".into()
            });
        }
        let locator = if self.provider_id == ProviderId::fal() {
            if primary.starts_with("https://") {
                fal_locator_from_url(primary)?
            } else {
                let request_id = self.request_id.trimmed();
                if request_id.is_empty() {
                    return Err("Enter the fal request ID.".into());
                }
                JobLocator::Fal {
                    endpoint_id: primary.to_owned(),
                    request_id: request_id.to_owned(),
                    status_url: None,
                    response_url: None,
                }
            }
        } else {
            JobLocator::OpenRouter {
                polling_url: primary.to_owned(),
            }
        };
        locator.validate().map_err(|error| error.to_string())?;
        Ok(locator)
    }
}

// Reducer callers construct and pattern-match these payloads directly. Keeping
// them inline preserves the small, ergonomic public state-machine API.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    Confirmation {
        request: RequestDraft,
        estimate: CostView,
    },
    PauseMonitoring {
        pause_selected: bool,
    },
    ImportJob {
        draft: ImportDraft,
    },
    Help,
}

#[derive(Debug)]
pub enum Effect {
    ConnectKey {
        provider_id: ProviderId,
        key: SecretValue,
    },
    ForgetKey(ProviderId),
    LoadCatalog(ProviderId),
    PersistDefaultProvider(ProviderId),
    Quote(RequestDraft),
    SubmitOnce(RequestDraft),
    Resume(ProviderJobKey),
    Import {
        provider_id: ProviderId,
        locator: JobLocator,
    },
    CancelCurrent,
    LoadHistory(usize),
    OpenVideo(PathBuf),
    PersistSettings {
        provider_id: ProviderId,
        model: String,
        settings: ModelSettings,
    },
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskScope {
    Credential,
    Catalog,
    Quote,
    Generation,
    Import,
    History,
    OpenVideo,
    General,
}

// Task events cross a single in-process channel and are intentionally shaped
// for direct construction in reducer tests and alternate frontends.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    Ready {
        providers: Vec<UiProvider>,
        default_provider: ProviderId,
    },
    KeyValidated {
        provider_id: ProviderId,
        connection: ProviderConnectionKind,
        storage_note: String,
    },
    KeyForgotten {
        provider_id: ProviderId,
        storage_note: String,
    },
    CatalogLoaded {
        provider_id: ProviderId,
        models: Vec<UiModel>,
        stale: bool,
        remembered: HashMap<String, ModelSettings>,
    },
    QuoteLoaded {
        provider_id: ProviderId,
        model_id: String,
        quote: CostView,
    },
    SubmissionStarted {
        provider_id: ProviderId,
    },
    JobAccepted {
        provider_id: ProviderId,
        job_id: String,
        status: String,
    },
    JobUpdated {
        provider_id: ProviderId,
        job_id: String,
        status: String,
        detail: String,
    },
    PollWaiting {
        provider_id: ProviderId,
        seconds: u64,
    },
    DownloadProgress {
        provider_id: ProviderId,
        received: u64,
        total: Option<u64>,
    },
    Completed(Completion),
    HistoryLoaded(Vec<HistoryItem>),
    Imported {
        provider_id: ProviderId,
        job_id: String,
        status: String,
    },
    Cancelled {
        provider_id: Option<ProviderId>,
    },
    Error {
        provider_id: Option<ProviderId>,
        scope: TaskScope,
        message: String,
        recoverable: bool,
    },
}

// Actions use the same direct reducer API as TaskEvent; boxing here would make
// keyboard-independent tests and adapters noisier for negligible benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
    Tick(Instant),
    Task(TaskEvent),
}

pub struct App {
    pub route: Route,
    pub modal: Option<Modal>,
    pub onboarding: OnboardingState,
    pub providers: ProviderManagementState,
    pub compose: ComposeState,
    pub generation: GenerationState,
    pub history: HistoryState,
    pub completion: Option<Completion>,
    pub pending_review: Option<RequestDraft>,
    pub toast: Option<Toast>,
    pub width: u16,
    pub height: u16,
    pub capabilities: TerminalCapabilities,
    pub should_quit: bool,
    pub clock: String,
}

impl Default for App {
    fn default() -> Self {
        Self::new(TerminalCapabilities::detect())
    }
}

impl App {
    pub fn new(capabilities: TerminalCapabilities) -> Self {
        Self {
            route: Route::Onboarding,
            modal: None,
            onboarding: OnboardingState::default(),
            providers: ProviderManagementState::default(),
            compose: ComposeState::default(),
            generation: GenerationState::default(),
            history: HistoryState::default(),
            completion: None,
            pending_review: None,
            toast: None,
            width: 80,
            height: 24,
            capabilities,
            should_quit: false,
            clock: String::new(),
        }
    }

    pub fn layout_mode(&self) -> LayoutMode {
        if self.width < MINIMUM_WIDTH || self.height < MINIMUM_HEIGHT {
            LayoutMode::TooSmall
        } else if self.width < NARROW_BREAKPOINT {
            LayoutMode::Compact
        } else if self.width < WIDE_BREAKPOINT {
            LayoutMode::Stacked
        } else {
            LayoutMode::Wide
        }
    }

    pub fn short(&self) -> bool {
        self.height < SHORT_BREAKPOINT
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.generation.started_at)
    }

    pub fn update(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Resize(width, height) => {
                self.width = width;
                self.height = height;
                Vec::new()
            }
            Action::Tick(now) => {
                if self.route == Route::Progress && !self.capabilities.reduced_motion {
                    self.generation.phase = self.generation.phase.wrapping_add(1);
                }
                if let Some(deadline) = self.generation.poll_deadline {
                    let remaining = deadline.saturating_duration_since(now);
                    self.generation.countdown =
                        Some(remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0));
                }
                if self
                    .toast
                    .as_ref()
                    .and_then(|toast| toast.expires_at)
                    .is_some_and(|expiry| expiry <= now)
                {
                    self.toast = None;
                }
                Vec::new()
            }
            Action::Paste(value) => self.handle_paste(value),
            Action::Task(event) => self.handle_task_event(event),
            Action::Key(key) if key.kind == KeyEventKind::Release => Vec::new(),
            Action::Key(key) => self.handle_key(key),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.request_quit();
        }
        if self.layout_mode() == LayoutMode::TooSmall {
            if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                return self.request_quit();
            }
            return Vec::new();
        }
        if self.modal.is_none()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('p')
            && self.route != Route::Progress
        {
            self.open_provider_management();
            return Vec::new();
        }
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        match self.route {
            Route::Onboarding => self.handle_onboarding_key(key),
            Route::Compose => self.handle_compose_key(key),
            Route::Progress => self.handle_progress_key(key),
            Route::Complete => self.handle_complete_key(key),
            Route::History => self.handle_history_key(key),
            Route::Providers => self.handle_provider_key(key),
        }
    }

    fn handle_onboarding_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return self.request_quit();
        }
        if key.code == KeyCode::Enter && !self.onboarding.validating {
            if self.onboarding.key.is_empty() {
                self.onboarding.status = "Enter an API key to continue.".into();
                self.onboarding.severity = Severity::Error;
                return Vec::new();
            }
            let exposed = self.onboarding.key.expose_once();
            self.onboarding.key.clear();
            self.onboarding.validating = true;
            self.onboarding.status = "Validating securely with OpenRouter…".into();
            self.onboarding.severity = Severity::Info;
            return vec![Effect::ConnectKey {
                provider_id: ProviderId::openrouter(),
                key: SecretValue::new(exposed),
            }];
        }
        self.onboarding.key.handle_key(key);
        Vec::new()
    }

    fn open_provider_management(&mut self) {
        if self.route != Route::Providers {
            self.providers.return_route = self.route;
        }
        self.providers.editing_key = false;
        self.providers.key.clear();
        if let Some(index) = self
            .providers
            .providers
            .iter()
            .position(|provider| provider.id == self.compose.provider_id)
        {
            self.providers.selected = index;
        }
        self.route = Route::Providers;
    }

    fn handle_provider_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return self.request_quit();
        }
        if self.providers.editing_key {
            match key.code {
                KeyCode::Esc => {
                    self.providers.key.clear();
                    self.providers.editing_key = false;
                }
                KeyCode::Enter if self.providers.validating.is_none() => {
                    if self.providers.key.is_empty() {
                        self.providers.status = "Enter an API key first.".into();
                        self.providers.severity = Severity::Error;
                    } else if let Some(provider_id) = self
                        .providers
                        .selected()
                        .map(|provider| provider.id.clone())
                    {
                        let exposed = self.providers.key.expose_once();
                        self.providers.key.clear();
                        self.providers.editing_key = false;
                        self.providers.validating = Some(provider_id.clone());
                        self.providers.status =
                            format!("Validating securely with {}…", provider_name(&provider_id));
                        self.providers.severity = Severity::Info;
                        return vec![Effect::ConnectKey {
                            provider_id,
                            key: SecretValue::new(exposed),
                        }];
                    }
                }
                _ => {
                    self.providers.key.handle_key(key);
                }
            }
            return Vec::new();
        }

        match key.code {
            KeyCode::Esc => {
                self.route = self.providers.return_route;
            }
            KeyCode::Up => {
                self.providers.selected = self.providers.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                self.providers.selected = (self.providers.selected + 1)
                    .min(self.providers.providers.len().saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                self.providers.editing_key = true;
                self.providers.key.clear();
                self.providers.status = self
                    .providers
                    .selected()
                    .map(|provider| format!("Enter a {} API key.", provider.name))
                    .unwrap_or_default();
                self.providers.severity = Severity::Info;
            }
            KeyCode::Char('f') | KeyCode::Delete => {
                if let Some(provider_id) = self
                    .providers
                    .selected()
                    .map(|provider| provider.id.clone())
                {
                    self.providers.status =
                        format!("Forgetting the {} key…", provider_name(&provider_id));
                    self.providers.severity = Severity::Info;
                    return vec![Effect::ForgetKey(provider_id)];
                }
            }
            KeyCode::Char(' ') | KeyCode::Char('u') => {
                if let Some(provider_id) = self
                    .providers
                    .selected()
                    .map(|provider| provider.id.clone())
                {
                    let effects = self.select_provider(provider_id);
                    self.route = Route::Compose;
                    return effects;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn cycle_provider(&mut self, key: KeyEvent) -> Vec<Effect> {
        if !matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Enter
                | KeyCode::Char(' ')
        ) {
            return Vec::new();
        }
        let providers = &self.providers.providers;
        if providers.is_empty() {
            return Vec::new();
        }
        let current = providers
            .iter()
            .position(|provider| provider.id == self.compose.provider_id)
            .unwrap_or(0);
        let next = if matches!(key.code, KeyCode::Left | KeyCode::Up) {
            current.checked_sub(1).unwrap_or(providers.len() - 1)
        } else {
            (current + 1) % providers.len()
        };
        let provider_id = providers[next].id.clone();
        self.select_provider(provider_id)
    }

    fn select_provider(&mut self, provider_id: ProviderId) -> Vec<Effect> {
        if self.compose.provider_id == provider_id && !self.compose.models.is_empty() {
            return Vec::new();
        }
        let provider_changed = self.compose.provider_id != provider_id;
        self.compose.provider_id = provider_id.clone();
        self.pending_review = None;
        self.compose.model_index = None;
        self.compose.picker = None;
        self.compose.duration_index = None;
        self.compose.resolution_index = None;
        self.compose.aspect_index = None;
        self.compose.size_index = None;
        self.compose.audio = false;
        self.compose.seed.clear();
        self.compose.adapter_options.clear();
        self.compose.estimate = CostView::default();
        if let Some(catalog) = self.compose.catalogs.get(&provider_id) {
            self.compose.models = catalog.models.clone();
            self.compose.catalog_loading = catalog.loading;
            self.compose.catalog_stale = catalog.stale;
            self.compose.catalog_message = catalog.message.clone();
            if !self.compose.models.is_empty() {
                self.select_preferred_model();
            }
            provider_changed
                .then(|| Effect::PersistDefaultProvider(provider_id))
                .into_iter()
                .collect()
        } else {
            self.compose.models.clear();
            self.compose.catalog_loading = true;
            self.compose.catalog_stale = false;
            self.compose.catalog_message =
                format!("Loading {} video models…", provider_name(&provider_id));
            self.compose.catalogs.insert(
                provider_id.clone(),
                ProviderCatalogState {
                    loading: true,
                    message: self.compose.catalog_message.clone(),
                    ..ProviderCatalogState::default()
                },
            );
            let mut effects = vec![Effect::LoadCatalog(provider_id.clone())];
            if provider_changed {
                effects.push(Effect::PersistDefaultProvider(provider_id));
            }
            effects
        }
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if self.compose.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Enter => return self.review_request(),
                KeyCode::Char('h') => {
                    self.route = Route::History;
                    self.history.loading = true;
                    return vec![Effect::LoadHistory(500)];
                }
                KeyCode::Char('k') => {
                    self.open_provider_management();
                    return Vec::new();
                }
                KeyCode::Char('q') => return self.request_quit(),
                _ => {}
            }
        }
        if key.code == KeyCode::Tab {
            self.move_focus(!key.modifiers.contains(KeyModifiers::SHIFT));
            return Vec::new();
        }
        if key.code == KeyCode::BackTab {
            self.move_focus(false);
            return Vec::new();
        }

        let changed = match self.compose.focus {
            ComposeFocus::Prompt => self.compose.prompt.handle_key(key),
            ComposeFocus::Provider => return self.cycle_provider(key),
            ComposeFocus::Model if key.code == KeyCode::Enter => {
                self.open_model_picker();
                false
            }
            ComposeFocus::Model => self.cycle_model(key),
            ComposeFocus::Duration => {
                let length = self
                    .selected_model()
                    .map(|model| model.durations.len())
                    .unwrap_or(0);
                cycle_option(&mut self.compose.duration_index, length, key)
            }
            ComposeFocus::Resolution => {
                let length = self
                    .selected_model()
                    .map(|model| model.resolutions.len())
                    .unwrap_or(0);
                let changed = cycle_option(&mut self.compose.resolution_index, length, key);
                if changed && self.compose.resolution_index.is_some() {
                    self.compose.size_index = None;
                }
                changed
            }
            ComposeFocus::AspectRatio => {
                let length = self
                    .selected_model()
                    .map(|model| model.aspect_ratios.len())
                    .unwrap_or(0);
                let changed = cycle_option(&mut self.compose.aspect_index, length, key);
                if changed && self.compose.aspect_index.is_some() {
                    self.compose.size_index = None;
                }
                changed
            }
            ComposeFocus::Size => {
                let length = self
                    .selected_model()
                    .map(|model| model.sizes.len())
                    .unwrap_or(0);
                let changed = cycle_option(&mut self.compose.size_index, length, key);
                if changed && self.compose.size_index.is_some() {
                    self.compose.resolution_index = None;
                    self.compose.aspect_index = None;
                }
                changed
            }
            ComposeFocus::Audio if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) => {
                self.compose.audio = !self.compose.audio;
                true
            }
            ComposeFocus::Seed => self.compose.seed.handle_key(key),
            ComposeFocus::AdvancedToggle
                if matches!(key.code, KeyCode::Enter | KeyCode::Char(' ')) =>
            {
                self.compose.advanced = !self.compose.advanced;
                true
            }
            ComposeFocus::FirstFrame => self.compose.first_frame.handle_key(key),
            ComposeFocus::LastFrame => self.compose.last_frame.handle_key(key),
            ComposeFocus::References => self.compose.references.handle_key(key),
            ComposeFocus::AdapterOptions => self.compose.adapter_options.handle_key(key),
            ComposeFocus::Generate if key.code == KeyCode::Enter => return self.review_request(),
            ComposeFocus::History if key.code == KeyCode::Enter => {
                self.route = Route::History;
                self.history.loading = true;
                return vec![Effect::LoadHistory(500)];
            }
            _ => false,
        };
        if changed {
            self.pending_review = None;
            self.recalculate_estimate();
        }
        Vec::new()
    }

    fn handle_progress_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if !self.generation.submitting && !self.generation.monitoring {
                    if key.code == KeyCode::Char('q') {
                        return self.request_quit();
                    }
                    self.route = Route::Compose;
                } else if self.generation.job_id.is_none() {
                    self.toast = Some(Toast::timed(
                        "Please wait",
                        "Wait for a recoverable job ID before leaving this paid submission.",
                        Severity::Warning,
                        Instant::now(),
                    ));
                } else {
                    self.modal = Some(Modal::PauseMonitoring {
                        pause_selected: false,
                    });
                }
            }
            KeyCode::Char('r') if self.generation.error.is_some() => {
                if let Some(job_id) = self.generation.job_id.clone() {
                    self.generation.error = None;
                    self.generation.monitoring = true;
                    let key = ProviderJobKey::new(self.generation.provider_id.clone(), job_id);
                    if let Ok(key) = key {
                        return vec![Effect::Resume(key)];
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn handle_complete_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('o') | KeyCode::Enter => self
                .completion
                .as_ref()
                .map(|outcome| vec![Effect::OpenVideo(outcome.path.clone())])
                .unwrap_or_default(),
            KeyCode::Char('n') => {
                self.route = Route::Compose;
                self.compose.prompt.clear();
                self.completion = None;
                Vec::new()
            }
            KeyCode::Char('r') => {
                if let Some(request) = self
                    .completion
                    .as_ref()
                    .and_then(|outcome| outcome.request.clone())
                {
                    let effects = self.select_provider(request.provider_id.clone());
                    self.pending_review = Some(request.clone());
                    self.compose.estimate = CostView {
                        basis: format!(
                            "Refreshing the current {} quote…",
                            provider_name(&request.provider_id)
                        ),
                        ..CostView::default()
                    };
                    let mut effects = effects;
                    effects.push(Effect::Quote(request));
                    return effects;
                } else {
                    self.route = Route::Compose;
                }
                Vec::new()
            }
            KeyCode::Char('h') => {
                self.route = Route::History;
                self.history.loading = true;
                vec![Effect::LoadHistory(500)]
            }
            KeyCode::Char('q') => self.request_quit(),
            _ => Vec::new(),
        }
    }

    fn handle_history_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => {
                self.route = Route::Compose;
                Vec::new()
            }
            KeyCode::Char('i') => {
                self.modal = Some(Modal::ImportJob {
                    draft: ImportDraft::new(self.compose.provider_id.clone()),
                });
                Vec::new()
            }
            KeyCode::Up => {
                self.history.selected = self.history.selected.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down => {
                self.history.selected =
                    (self.history.selected + 1).min(self.history.items.len().saturating_sub(1));
                Vec::new()
            }
            KeyCode::PageUp => {
                self.history.selected = self.history.selected.saturating_sub(10);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.history.selected =
                    (self.history.selected + 10).min(self.history.items.len().saturating_sub(1));
                Vec::new()
            }
            KeyCode::Enter => self.activate_history_item(),
            _ => Vec::new(),
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(modal) = self.modal.take() else {
            return Vec::new();
        };
        match modal {
            Modal::Confirmation { request, estimate } => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Enter => {
                    let provider_id = request.provider_id.clone();
                    self.generation = GenerationState {
                        provider_id: provider_id.clone(),
                        request: Some(request.clone()),
                        status: "Submitting".into(),
                        detail: format!(
                            "Sending one paid request to {}…",
                            provider_name(&provider_id)
                        ),
                        started_at: Instant::now(),
                        submitting: true,
                        monitoring: true,
                        ..GenerationState::default()
                    };
                    self.route = Route::Progress;
                    self.compose.remembered.insert(
                        (provider_id.clone(), request.model.clone()),
                        request.settings(),
                    );
                    vec![
                        Effect::PersistSettings {
                            provider_id,
                            model: request.model.clone(),
                            settings: request.settings(),
                        },
                        Effect::SubmitOnce(request),
                    ]
                }
                _ => {
                    self.modal = Some(Modal::Confirmation { request, estimate });
                    Vec::new()
                }
            },
            Modal::PauseMonitoring { mut pause_selected } => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    pause_selected = !pause_selected;
                    self.modal = Some(Modal::PauseMonitoring { pause_selected });
                    Vec::new()
                }
                KeyCode::Enter if pause_selected => {
                    self.route = Route::Compose;
                    self.generation.monitoring = false;
                    vec![Effect::CancelCurrent]
                }
                KeyCode::Enter => Vec::new(),
                _ => {
                    self.modal = Some(Modal::PauseMonitoring { pause_selected });
                    Vec::new()
                }
            },
            Modal::ImportJob { mut draft } => match key.code {
                KeyCode::Esc => Vec::new(),
                KeyCode::Tab | KeyCode::BackTab if draft.provider_id == ProviderId::fal() => {
                    draft.focus = match draft.focus {
                        ImportFocus::Locator => ImportFocus::RequestId,
                        ImportFocus::RequestId => ImportFocus::Locator,
                    };
                    self.modal = Some(Modal::ImportJob { draft });
                    Vec::new()
                }
                KeyCode::Enter => match draft.into_locator() {
                    Ok(locator) => {
                        let provider_id = draft.provider_id.clone();
                        let job_id = locator.remote_job_id().to_owned();
                        self.route = Route::Progress;
                        self.generation = GenerationState {
                            provider_id: provider_id.clone(),
                            job_id: Some(job_id),
                            status: "Importing".into(),
                            detail: format!(
                                "Looking up the existing {} job…",
                                provider_name(&provider_id)
                            ),
                            started_at: Instant::now(),
                            monitoring: true,
                            ..GenerationState::default()
                        };
                        vec![Effect::Import {
                            provider_id,
                            locator,
                        }]
                    }
                    Err(message) => {
                        self.toast = Some(Toast::timed(
                            "Check import details",
                            message,
                            Severity::Error,
                            Instant::now(),
                        ));
                        self.modal = Some(Modal::ImportJob { draft });
                        Vec::new()
                    }
                },
                _ => {
                    match draft.focus {
                        ImportFocus::Locator => draft.locator.handle_key(key),
                        ImportFocus::RequestId => draft.request_id.handle_key(key),
                    };
                    self.modal = Some(Modal::ImportJob { draft });
                    Vec::new()
                }
            },
            Modal::Help => {
                if !matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.modal = Some(Modal::Help);
                }
                Vec::new()
            }
        }
    }

    fn handle_paste(&mut self, value: String) -> Vec<Effect> {
        match self.modal.as_mut() {
            Some(Modal::ImportJob { draft }) => match draft.focus {
                ImportFocus::Locator => draft.locator.insert_str(&value),
                ImportFocus::RequestId => draft.request_id.insert_str(&value),
            },
            Some(_) => {}
            None if self.route == Route::Onboarding => self.onboarding.key.insert_str(&value),
            None if self.route == Route::Providers && self.providers.editing_key => {
                self.providers.key.insert_str(&value)
            }
            None if self.route == Route::Compose => match self.compose.focus {
                ComposeFocus::Prompt => self.compose.prompt.insert_str(&value),
                ComposeFocus::Seed => self.compose.seed.insert_str(&value),
                ComposeFocus::FirstFrame => self.compose.first_frame.insert_str(&value),
                ComposeFocus::LastFrame => self.compose.last_frame.insert_str(&value),
                ComposeFocus::References => self.compose.references.insert_str(&value),
                ComposeFocus::AdapterOptions => self.compose.adapter_options.insert_str(&value),
                _ => {}
            },
            _ => {}
        }
        if self.route == Route::Compose {
            self.pending_review = None;
        }
        self.recalculate_estimate();
        Vec::new()
    }

    fn handle_task_event(&mut self, event: TaskEvent) -> Vec<Effect> {
        match event {
            TaskEvent::Ready {
                providers,
                default_provider,
            } => {
                if !providers.is_empty() {
                    self.providers.providers = providers;
                }
                let connected = self
                    .providers
                    .providers
                    .iter()
                    .any(|provider| provider.connection.has_key());
                if connected {
                    self.route = Route::Compose;
                    // Startup is applying the already-persisted preference, not
                    // creating a new selection that needs to be written back.
                    self.compose.provider_id = default_provider.clone();
                    self.select_provider(default_provider)
                } else {
                    self.route = Route::Onboarding;
                    Vec::new()
                }
            }
            TaskEvent::KeyValidated {
                provider_id,
                connection,
                storage_note,
            } => {
                self.onboarding.validating = false;
                self.onboarding.status = format!("Connected. {storage_note}");
                self.onboarding.severity = Severity::Success;
                self.providers.validating = None;
                self.providers.status =
                    format!("{} connected. {storage_note}", provider_name(&provider_id));
                self.providers.severity = Severity::Success;
                if let Some(provider) = self.providers.get_mut(&provider_id) {
                    provider.connection = connection;
                    provider.storage_note = storage_note;
                }
                if self.route == Route::Onboarding {
                    self.route = Route::Compose;
                    self.select_provider(provider_id)
                } else {
                    vec![Effect::LoadCatalog(provider_id)]
                }
            }
            TaskEvent::KeyForgotten {
                provider_id,
                storage_note,
            } => {
                self.providers.validating = None;
                if let Some(provider) = self.providers.get_mut(&provider_id) {
                    provider.connection = ProviderConnectionKind::NeedsKey;
                    provider.storage_note = storage_note.clone();
                }
                self.providers.status = format!(
                    "{} disconnected. {storage_note}",
                    provider_name(&provider_id)
                );
                self.providers.severity = Severity::Success;
                Vec::new()
            }
            TaskEvent::CatalogLoaded {
                provider_id,
                mut models,
                stale,
                remembered,
            } => {
                for model in &mut models {
                    model.provider_id = provider_id.clone();
                }
                self.compose.remembered.extend(
                    remembered
                        .into_iter()
                        .map(|(model_id, settings)| ((provider_id.clone(), model_id), settings)),
                );
                let message = if stale {
                    "Using cached catalog — settings may be stale.".into()
                } else {
                    format!("{} video models available.", models.len())
                };
                self.compose.catalogs.insert(
                    provider_id.clone(),
                    ProviderCatalogState {
                        models: models.clone(),
                        loading: false,
                        stale,
                        message: message.clone(),
                    },
                );
                if self.compose.provider_id == provider_id {
                    self.compose.models = models;
                    self.compose.catalog_loading = false;
                    self.compose.catalog_stale = stale;
                    self.compose.catalog_message = message;
                    self.select_preferred_model();
                }
                Vec::new()
            }
            TaskEvent::QuoteLoaded {
                provider_id,
                model_id,
                quote,
            } => {
                if self.compose.provider_id != provider_id {
                    if self
                        .pending_review
                        .as_ref()
                        .is_some_and(|request| request.provider_id == provider_id)
                    {
                        self.pending_review = None;
                    }
                    return Vec::new();
                }
                if let Some(request) = self.pending_review.take() {
                    if request.provider_id == provider_id && request.model == model_id {
                        self.compose.estimate = quote.clone();
                        self.modal = Some(Modal::Confirmation {
                            request,
                            estimate: quote,
                        });
                    } else {
                        self.pending_review = Some(request);
                    }
                }
                Vec::new()
            }
            TaskEvent::SubmissionStarted { provider_id } => {
                if self.generation.provider_id != provider_id {
                    return Vec::new();
                }
                self.generation.submitting = true;
                self.generation.status = "Submitting".into();
                Vec::new()
            }
            TaskEvent::JobAccepted {
                provider_id,
                job_id,
                status,
            } => {
                if self.generation.provider_id != provider_id {
                    return Vec::new();
                }
                self.generation.job_id = Some(job_id);
                self.generation.status = human_status(&status);
                self.generation.detail = format!(
                    "{} accepted the job and saved it to local history.",
                    provider_name(&provider_id)
                );
                self.generation.submitting = false;
                self.generation.monitoring = true;
                Vec::new()
            }
            TaskEvent::JobUpdated {
                provider_id,
                job_id,
                status,
                detail,
            } => {
                if self.generation.provider_id == provider_id
                    && self
                        .generation
                        .job_id
                        .as_ref()
                        .is_none_or(|active| active == &job_id)
                {
                    self.generation.job_id = Some(job_id);
                    self.generation.status = human_status(&status);
                    self.generation.detail = detail;
                    self.generation.countdown = None;
                    self.generation.poll_deadline = None;
                }
                Vec::new()
            }
            TaskEvent::PollWaiting {
                provider_id,
                seconds,
            } => {
                if self.generation.provider_id != provider_id {
                    return Vec::new();
                }
                self.generation.countdown = Some(seconds);
                self.generation.poll_deadline = Some(Instant::now() + Duration::from_secs(seconds));
                Vec::new()
            }
            TaskEvent::DownloadProgress {
                provider_id,
                received,
                total,
            } => {
                if self.generation.provider_id != provider_id {
                    return Vec::new();
                }
                self.generation.status = "Downloading".into();
                self.generation.countdown = None;
                self.generation.poll_deadline = None;
                self.generation.download_received = received;
                self.generation.download_total = total;
                Vec::new()
            }
            TaskEvent::Completed(outcome) => {
                if self.generation.provider_id != outcome.provider_id {
                    return Vec::new();
                }
                self.generation.status = "Completed".into();
                self.generation.monitoring = false;
                self.completion = Some(outcome);
                self.route = Route::Complete;
                Vec::new()
            }
            TaskEvent::HistoryLoaded(items) => {
                self.history.items = items;
                self.history.selected = 0;
                self.history.loading = false;
                Vec::new()
            }
            TaskEvent::Imported {
                provider_id,
                job_id,
                status,
            } => {
                self.route = Route::Progress;
                self.generation.provider_id = provider_id;
                self.generation.job_id = Some(job_id);
                self.generation.status = human_status(&status);
                self.generation.detail =
                    "Imported existing job; monitoring without a new submission.".into();
                self.generation.monitoring = true;
                Vec::new()
            }
            TaskEvent::Cancelled { provider_id } => {
                if provider_id
                    .as_ref()
                    .is_some_and(|provider| provider != &self.generation.provider_id)
                {
                    return Vec::new();
                }
                self.generation.monitoring = false;
                self.generation.countdown = None;
                self.generation.poll_deadline = None;
                Vec::new()
            }
            TaskEvent::Error {
                provider_id,
                scope,
                message,
                recoverable: _,
            } => {
                match scope {
                    TaskScope::Credential => {
                        self.onboarding.validating = false;
                        self.onboarding.status = message.clone();
                        self.onboarding.severity = Severity::Error;
                        self.providers.validating = None;
                        self.providers.status = message;
                        self.providers.severity = Severity::Error;
                        if self.route != Route::Providers
                            && provider_id
                                .as_ref()
                                .is_none_or(|provider| provider == &ProviderId::openrouter())
                            && !self
                                .providers
                                .providers
                                .iter()
                                .any(|provider| provider.connection.has_key())
                        {
                            self.route = Route::Onboarding;
                        }
                    }
                    TaskScope::Catalog => {
                        if let Some(provider_id) = provider_id.as_ref()
                            && let Some(catalog) = self.compose.catalogs.get_mut(provider_id)
                        {
                            catalog.loading = false;
                            catalog.message = message.clone();
                        }
                        if provider_id
                            .as_ref()
                            .is_none_or(|provider| provider == &self.compose.provider_id)
                        {
                            self.compose.catalog_loading = false;
                            self.compose.catalog_message = message;
                        }
                    }
                    TaskScope::Quote | TaskScope::Generation if self.route == Route::Compose => {
                        self.pending_review = None;
                        self.toast = Some(Toast::timed(
                            "Could not quote request",
                            message,
                            Severity::Error,
                            Instant::now(),
                        ));
                    }
                    TaskScope::Generation | TaskScope::Import | TaskScope::History
                        if self.route == Route::Progress =>
                    {
                        if provider_id
                            .as_ref()
                            .is_some_and(|provider| provider != &self.generation.provider_id)
                        {
                            return Vec::new();
                        }
                        self.generation.submitting = false;
                        self.generation.countdown = None;
                        self.generation.poll_deadline = None;
                        self.generation.error = Some(message);
                        self.generation.status = "Monitoring paused".into();
                        self.generation.monitoring = false;
                        self.route = Route::Progress;
                    }
                    TaskScope::History => {
                        self.history.loading = false;
                        self.toast = Some(Toast::timed(
                            "History",
                            message,
                            Severity::Error,
                            Instant::now(),
                        ));
                    }
                    _ => {
                        self.toast = Some(Toast::timed(
                            "Error",
                            message,
                            Severity::Error,
                            Instant::now(),
                        ));
                    }
                }
                Vec::new()
            }
        }
    }

    fn review_request(&mut self) -> Vec<Effect> {
        if !self
            .providers
            .get(&self.compose.provider_id)
            .is_some_and(|provider| provider.connection.has_key())
        {
            self.toast = Some(Toast::timed(
                "Connect provider",
                format!(
                    "Connect {} in provider management (Ctrl+P) first.",
                    provider_name(&self.compose.provider_id)
                ),
                Severity::Warning,
                Instant::now(),
            ));
            return Vec::new();
        }
        match self.build_request() {
            Ok(request) => {
                self.compose.estimate = CostView {
                    basis: format!(
                        "Refreshing the current {} quote…",
                        provider_name(&request.provider_id)
                    ),
                    ..CostView::default()
                };
                self.pending_review = Some(request.clone());
                return vec![Effect::Quote(request)];
            }
            Err(message) => {
                self.toast = Some(Toast::timed(
                    "Check your request",
                    message,
                    Severity::Error,
                    Instant::now(),
                ));
            }
        }
        Vec::new()
    }

    pub fn build_request(&self) -> Result<RequestDraft, String> {
        let model = self
            .selected_model()
            .ok_or_else(|| "Choose a video model.".to_owned())?;
        let prompt = self.compose.prompt.trimmed();
        if prompt.is_empty() {
            return Err("Write a video prompt first.".into());
        }
        let seed = if self.compose.seed.trimmed().is_empty() {
            None
        } else {
            Some(
                self.compose
                    .seed
                    .trimmed()
                    .parse::<i64>()
                    .map_err(|_| "Seed must be an integer.".to_owned())?,
            )
        };
        let first_frame = if model.supports_frame("first_frame") {
            optional_https(self.compose.first_frame.trimmed(), "First-frame URL")?
        } else {
            None
        };
        let last_frame = if model.supports_frame("last_frame") {
            optional_https(self.compose.last_frame.trimmed(), "Last-frame URL")?
        } else {
            None
        };
        let references = self
            .compose
            .references
            .text()
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| require_https(value, "Reference URL"))
            .collect::<Result<Vec<_>, _>>()?;
        let adapter_options = if self.compose.adapter_options.trimmed().is_empty() {
            None
        } else {
            let value: serde_json::Value =
                serde_json::from_str(self.compose.adapter_options.text())
                    .map_err(|error| format!("Advanced options are not valid JSON: {error}."))?;
            let object = value
                .as_object()
                .ok_or_else(|| "Advanced options must be a JSON object.".to_owned())?;
            if self.compose.provider_id == ProviderId::openrouter()
                && let Some(parameters) = object.get("parameters")
            {
                let parameters = parameters
                    .as_object()
                    .ok_or_else(|| "provider.parameters must be a JSON object.".to_owned())?;
                let unsupported: Vec<&str> = parameters
                    .keys()
                    .map(String::as_str)
                    .filter(|name| {
                        !model
                            .passthrough_parameters
                            .iter()
                            .any(|allowed| allowed == name)
                    })
                    .collect();
                if !unsupported.is_empty() {
                    return Err(format!(
                        "Unsupported provider option(s): {}",
                        unsupported.join(", ")
                    ));
                }
            }
            if self.compose.provider_id == ProviderId::fal() {
                let common = [
                    "model",
                    "prompt",
                    "duration",
                    "resolution",
                    "aspect_ratio",
                    "size",
                    "seed",
                    "audio",
                    "generate_audio",
                    "first_frame",
                    "last_frame",
                    "references",
                ];
                let overrides = object
                    .keys()
                    .filter(|key| common.contains(&key.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                if !overrides.is_empty() {
                    return Err(format!(
                        "Advanced options cannot override common field(s): {}.",
                        overrides.join(", ")
                    ));
                }
            }
            Some(serde_json::to_string(&value).map_err(|error| error.to_string())?)
        };

        let size = selected(&model.sizes, self.compose.size_index);
        let (resolution, aspect_ratio) = if size.is_some() {
            (None, None)
        } else {
            (
                selected(&model.resolutions, self.compose.resolution_index),
                selected(&model.aspect_ratios, self.compose.aspect_index),
            )
        };
        Ok(RequestDraft {
            provider_id: self.compose.provider_id.clone(),
            model: model.id.clone(),
            prompt: prompt.to_owned(),
            duration: selected_copy(&model.durations, self.compose.duration_index),
            resolution,
            aspect_ratio,
            size,
            generate_audio: model.supports_audio().then_some(self.compose.audio),
            seed,
            first_frame,
            last_frame,
            references,
            adapter_options,
        })
    }

    fn recalculate_estimate(&mut self) {
        if let Ok(request) = self.build_request() {
            self.compose.estimate = if request.provider_id == ProviderId::openrouter() {
                estimate_request(self.selected_model(), &request)
            } else {
                CostView {
                    basis: "Review to load the current account-aware fal.ai quote.".into(),
                    ..CostView::default()
                }
            };
        }
    }

    fn selected_model(&self) -> Option<&UiModel> {
        self.compose
            .model_index
            .and_then(|index| self.compose.models.get(index))
    }

    fn select_model(&mut self, index: usize) {
        if index >= self.compose.models.len() {
            return;
        }
        self.compose.model_index = Some(index);
        let model = self.compose.models[index].clone();
        self.compose.duration_index = (!model.durations.is_empty()).then_some(0);
        self.compose.resolution_index = model
            .resolutions
            .iter()
            .position(|value| value.eq_ignore_ascii_case("720p"))
            .or((!model.resolutions.is_empty()).then_some(0));
        self.compose.aspect_index = model
            .aspect_ratios
            .iter()
            .position(|value| value == "16:9")
            .or((!model.aspect_ratios.is_empty()).then_some(0));
        self.compose.size_index = if self.compose.resolution_index.is_none()
            && self.compose.aspect_index.is_none()
            && !model.sizes.is_empty()
        {
            Some(0)
        } else {
            None
        };
        self.compose.audio = false;
        self.compose.seed.clear();
        if let Some(saved) = self
            .compose
            .remembered
            .get(&(self.compose.provider_id.clone(), model.id.clone()))
            .cloned()
        {
            self.compose.duration_index = saved
                .duration
                .and_then(|value| {
                    model
                        .durations
                        .iter()
                        .position(|candidate| *candidate == value)
                })
                .or(self.compose.duration_index);
            if let Some(size) = saved.size {
                if let Some(position) = model.sizes.iter().position(|candidate| candidate == &size)
                {
                    self.compose.size_index = Some(position);
                    self.compose.resolution_index = None;
                    self.compose.aspect_index = None;
                }
            } else {
                self.compose.resolution_index = saved
                    .resolution
                    .and_then(|value| {
                        model
                            .resolutions
                            .iter()
                            .position(|candidate| candidate == &value)
                    })
                    .or(self.compose.resolution_index);
                self.compose.aspect_index = saved
                    .aspect_ratio
                    .and_then(|value| {
                        model
                            .aspect_ratios
                            .iter()
                            .position(|candidate| candidate == &value)
                    })
                    .or(self.compose.aspect_index);
            }
            self.compose.audio = saved.generate_audio.unwrap_or(false);
            if let Some(seed) = saved.seed {
                self.compose.seed.set_text(seed.to_string());
            }
        }
        self.recalculate_estimate();
    }

    fn select_preferred_model(&mut self) {
        if self.compose.models.is_empty() {
            self.compose.model_index = None;
            return;
        }
        let preferred = if self.compose.provider_id == ProviderId::openrouter() {
            self.compose
                .models
                .iter()
                .position(|model| model.id == "black-forest-labs/flux-3-video")
                .or_else(|| {
                    self.compose
                        .models
                        .iter()
                        .position(|model| model.id.to_ascii_lowercase().contains("flux"))
                })
                .unwrap_or(0)
        } else {
            0
        };
        self.select_model(preferred);
    }

    fn cycle_model(&mut self, key: KeyEvent) -> bool {
        let changed = cycle_option(
            &mut self.compose.model_index,
            self.compose.models.len(),
            key,
        );
        if changed && let Some(index) = self.compose.model_index {
            self.select_model(index);
        }
        changed
    }

    fn open_model_picker(&mut self) {
        if self.compose.models.is_empty() {
            return;
        }
        self.compose.picker = Some(ModelPicker {
            provider_id: self.compose.provider_id.clone(),
            query: TextEditor::line(),
            filtered: (0..self.compose.models.len()).collect(),
            selected: self.compose.model_index.unwrap_or(0),
        });
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(mut picker) = self.compose.picker.take() else {
            return Vec::new();
        };
        if picker.provider_id != self.compose.provider_id {
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc => return Vec::new(),
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                picker.selected = (picker.selected + 1).min(picker.filtered.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(index) = picker.filtered.get(picker.selected).copied() {
                    self.select_model(index);
                }
                return Vec::new();
            }
            _ => {
                picker.query.handle_key(key);
                let query = picker.query.text().to_ascii_lowercase();
                picker.filtered = self
                    .compose
                    .models
                    .iter()
                    .enumerate()
                    .filter(|(_, model)| {
                        model.name.to_ascii_lowercase().contains(&query)
                            || model.id.to_ascii_lowercase().contains(&query)
                    })
                    .map(|(index, _)| index)
                    .collect();
                picker.selected = 0;
            }
        }
        self.compose.picker = Some(picker);
        Vec::new()
    }

    fn move_focus(&mut self, forward: bool) {
        let mut fields = vec![
            ComposeFocus::Prompt,
            ComposeFocus::Provider,
            ComposeFocus::Model,
        ];
        if let Some(model) = self.selected_model() {
            if !model.durations.is_empty() {
                fields.push(ComposeFocus::Duration);
            }
            if !model.resolutions.is_empty() {
                fields.push(ComposeFocus::Resolution);
            }
            if !model.aspect_ratios.is_empty() {
                fields.push(ComposeFocus::AspectRatio);
            }
            if !model.sizes.is_empty() {
                fields.push(ComposeFocus::Size);
            }
            if model.supports_audio() {
                fields.push(ComposeFocus::Audio);
            }
            if model.seed.unwrap_or(false) {
                fields.push(ComposeFocus::Seed);
            }
        }
        fields.push(ComposeFocus::AdvancedToggle);
        if self.compose.advanced {
            if self
                .selected_model()
                .is_some_and(|model| model.supports_frame("first_frame"))
            {
                fields.push(ComposeFocus::FirstFrame);
            }
            if self
                .selected_model()
                .is_some_and(|model| model.supports_frame("last_frame"))
            {
                fields.push(ComposeFocus::LastFrame);
            }
            fields.extend([ComposeFocus::References, ComposeFocus::AdapterOptions]);
        }
        fields.extend([ComposeFocus::Generate, ComposeFocus::History]);
        let current = fields
            .iter()
            .position(|field| *field == self.compose.focus)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % fields.len()
        } else {
            current.checked_sub(1).unwrap_or(fields.len() - 1)
        };
        self.compose.focus = fields[next];
    }

    fn activate_history_item(&mut self) -> Vec<Effect> {
        let Some(item) = self.history.items.get(self.history.selected).cloned() else {
            self.toast = Some(Toast::timed(
                "History",
                "Select a job first.",
                Severity::Warning,
                Instant::now(),
            ));
            return Vec::new();
        };
        if let Some(path) = item.output_path.as_ref().filter(|path| path.is_file()) {
            return vec![Effect::OpenVideo(path.clone())];
        }
        if matches!(
            item.status.as_str(),
            "failed" | "cancelled" | "canceled" | "expired"
        ) {
            self.toast = Some(Toast::timed(
                "History",
                "This terminal job has no downloadable video.",
                Severity::Error,
                Instant::now(),
            ));
            return Vec::new();
        }
        let key = item.key();
        self.route = Route::Progress;
        self.generation = GenerationState {
            provider_id: item.provider_id.clone(),
            request: item.request,
            job_id: Some(item.job_id.clone()),
            status: human_status(&item.status),
            detail: "Resuming existing job; no new generation will be submitted.".into(),
            started_at: Instant::now(),
            monitoring: true,
            ..GenerationState::default()
        };
        key.map(Effect::Resume).into_iter().collect()
    }

    fn request_quit(&mut self) -> Vec<Effect> {
        if self.route == Route::Progress
            && (self.generation.submitting || self.generation.monitoring)
        {
            if self.generation.job_id.is_none() {
                self.toast = Some(Toast::timed(
                    "Please wait",
                    format!(
                        "Wait for {} to return a recoverable job ID.",
                        provider_name(&self.generation.provider_id)
                    ),
                    Severity::Warning,
                    Instant::now(),
                ));
            } else {
                self.modal = Some(Modal::PauseMonitoring {
                    pause_selected: false,
                });
            }
            return Vec::new();
        }
        self.should_quit = true;
        vec![Effect::Quit]
    }
}

fn cycle_option(selected: &mut Option<usize>, length: usize, key: KeyEvent) -> bool {
    if length == 0
        || !matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Enter
                | KeyCode::Char(' ')
        )
    {
        return false;
    }
    let current = selected.unwrap_or(0);
    *selected = Some(if matches!(key.code, KeyCode::Left | KeyCode::Up) {
        current.checked_sub(1).unwrap_or(length - 1)
    } else {
        (current + 1) % length
    });
    true
}

fn fal_locator_from_url(value: &str) -> Result<JobLocator, String> {
    let parsed = url::Url::parse(value).map_err(|_| "The fal queue URL is invalid.".to_owned())?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("queue.fal.run") {
        return Err("The fal queue URL must use https://queue.fal.run.".into());
    }
    let segments = parsed
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let requests = segments
        .iter()
        .position(|segment| *segment == "requests")
        .ok_or_else(|| "The fal queue URL must contain /requests/{request_id}.".to_owned())?;
    let endpoint_id = segments[..requests].join("/");
    let request_id = segments
        .get(requests + 1)
        .copied()
        .unwrap_or_default()
        .to_owned();
    if endpoint_id.is_empty() || request_id.is_empty() {
        return Err("The fal queue URL is missing its endpoint or request ID.".into());
    }
    let is_response = segments
        .get(requests + 2)
        .is_some_and(|value| *value == "response");
    Ok(JobLocator::Fal {
        endpoint_id,
        request_id,
        status_url: (!is_response).then(|| value.to_owned()),
        response_url: is_response.then(|| value.to_owned()),
    })
}

fn selected(values: &[String], index: Option<usize>) -> Option<String> {
    index.and_then(|index| values.get(index)).cloned()
}

fn selected_copy<T: Copy>(values: &[T], index: Option<usize>) -> Option<T> {
    index.and_then(|index| values.get(index)).copied()
}

fn optional_https(value: &str, label: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        Ok(None)
    } else {
        require_https(value, label).map(Some)
    }
}

fn require_https(value: &str, label: &str) -> Result<String, String> {
    if value.to_ascii_lowercase().starts_with("https://") && value.len() > 8 {
        Ok(value.to_owned())
    } else {
        Err(format!("{label} must be a public HTTPS URL."))
    }
}

fn human_status(value: &str) -> String {
    value
        .split('_')
        .map(|word| {
            let mut characters = word.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().chain(characters).collect())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

fn estimate_request(model: Option<&UiModel>, request: &RequestDraft) -> CostView {
    let Some(model) = model else {
        return CostView {
            basis: "Model pricing is unavailable.".into(),
            ..CostView::default()
        };
    };
    let Ok(request) = request.clone().into_domain() else {
        return CostView {
            basis: "Complete the request to estimate its cost.".into(),
            raw_pricing: model.pricing.clone(),
            ..CostView::default()
        };
    };
    let model = VideoModel::from(model);
    CostView::from(&crate::domain::estimate_cost(&model, &request))
}

/// Entry point used by the binary. The service bridge is completed against the
/// backend's typed command/event contract in this module, keeping `main.rs` tiny.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    run_native().await
}

async fn run_native() -> Result<(), Box<dyn std::error::Error>> {
    use crate::config::AppPaths;
    use crate::workflow::{ServiceConfig, ServiceHandle, spawn_service};
    use crossterm::{execute, terminal};
    use std::io::stdout;

    let paths = AppPaths::discover()?;
    let ServiceHandle {
        commands,
        mut events,
    } = spawn_service(paths, ServiceConfig::default())?;

    let mut output = stdout();
    terminal::enable_raw_mode()?;
    let guard = TerminalGuard::new();
    execute!(
        output,
        terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        event::EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::default();
    let initial_area = terminal.size()?;
    app.update(Action::Resize(initial_area.width, initial_area.height));
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Action>();
    let input_running = Arc::new(AtomicBool::new(true));
    spawn_input_thread(input_tx, Arc::clone(&input_running));
    let mut animation = tokio::time::interval(Duration::from_millis(240));
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    let mut next_op_id = 1u64;
    let mut shutdown_requested = false;

    loop {
        terminal.draw(|frame| crate::ui::render(frame, &app))?;
        let mut effects = Vec::new();
        tokio::select! {
            _ = animation.tick() => { app.update(Action::Tick(Instant::now())); }
            _ = clock.tick() => { app.clock = current_clock(); }
            Some(action) = input_rx.recv() => {
                effects = app.update(action);
            }
            _ = tokio::signal::ctrl_c() => {
                effects = app.request_quit();
            }
            event = events.recv() => {
                match event {
                    Some(crate::workflow::ServiceEvent::ShutdownComplete) => break,
                    Some(event) => {
                        if let Some(event) = service_event_to_task(event) {
                            effects = app.update(Action::Task(event));
                        }
                    }
                    None => {
                        if !shutdown_requested {
                            app.update(Action::Task(TaskEvent::Error {
                                provider_id: None,
                                scope: TaskScope::General,
                                message: "The background service stopped unexpectedly.".into(),
                                recoverable: false,
                            }));
                            terminal.draw(|frame| crate::ui::render(frame, &app))?;
                        }
                        break;
                    }
                }
            }
        }
        if !effects.is_empty()
            && dispatch_effects(&mut app, effects, &commands, &mut next_op_id).await
        {
            shutdown_requested = true;
            app.should_quit = false;
        }
    }
    input_running.store(false, Ordering::Relaxed);
    drop(terminal);
    drop(guard);
    Ok(())
}

async fn dispatch_effects(
    app: &mut App,
    effects: Vec<Effect>,
    commands: &tokio::sync::mpsc::Sender<crate::workflow::ServiceCommand>,
    next_op_id: &mut u64,
) -> bool {
    use crate::workflow::ServiceCommand;

    let mut shutdown = false;
    for effect in effects {
        let command = match effect {
            Effect::ConnectKey { provider_id, key } => match String::from_utf8(key.into_bytes()) {
                Ok(value) => Some(ServiceCommand::ConnectApiKey {
                    op_id: take_op_id(next_op_id),
                    provider_id: provider_id.clone(),
                    key: secrecy::SecretString::from(value),
                    persist_on_success: true,
                }),
                Err(error) => {
                    let mut invalid_bytes = error.into_bytes();
                    invalid_bytes.fill(0);
                    app.update(Action::Task(TaskEvent::Error {
                        provider_id: Some(provider_id),
                        scope: TaskScope::Credential,
                        message: "The API key contained invalid text.".into(),
                        recoverable: true,
                    }));
                    None
                }
            },
            Effect::ForgetKey(provider_id) => Some(ServiceCommand::ForgetApiKey {
                op_id: take_op_id(next_op_id),
                provider_id,
            }),
            Effect::LoadCatalog(provider_id) => Some(ServiceCommand::RefreshCatalog {
                op_id: take_op_id(next_op_id),
                provider_id,
            }),
            Effect::PersistDefaultProvider(provider_id) => {
                Some(ServiceCommand::SaveDefaultProvider {
                    op_id: take_op_id(next_op_id),
                    provider_id,
                })
            }
            Effect::Quote(request) => {
                let provider_id = request.provider_id.clone();
                match request.into_domain() {
                    Ok(request) => Some(ServiceCommand::Quote {
                        op_id: take_op_id(next_op_id),
                        provider_id,
                        request,
                    }),
                    Err(message) => {
                        app.update(Action::Task(TaskEvent::Error {
                            provider_id: Some(provider_id),
                            scope: TaskScope::Quote,
                            message,
                            recoverable: false,
                        }));
                        None
                    }
                }
            }
            Effect::SubmitOnce(request) => {
                let provider_id = request.provider_id.clone();
                match request.into_domain() {
                    Ok(request) => Some(ServiceCommand::Generate {
                        op_id: take_op_id(next_op_id),
                        provider_id,
                        request,
                    }),
                    Err(message) => {
                        app.update(Action::Task(TaskEvent::Error {
                            provider_id: Some(provider_id),
                            scope: TaskScope::Generation,
                            message,
                            recoverable: false,
                        }));
                        None
                    }
                }
            }
            Effect::Resume(key) => Some(ServiceCommand::Resume {
                op_id: take_op_id(next_op_id),
                key,
            }),
            Effect::Import {
                provider_id,
                locator,
            } => Some(ServiceCommand::Import {
                op_id: take_op_id(next_op_id),
                provider_id,
                locator,
            }),
            Effect::CancelCurrent => Some(ServiceCommand::CancelCurrent {
                op_id: take_op_id(next_op_id),
            }),
            Effect::LoadHistory(limit) => Some(ServiceCommand::LoadHistory {
                op_id: take_op_id(next_op_id),
                limit,
            }),
            Effect::OpenVideo(path) => Some(ServiceCommand::OpenVideo {
                op_id: take_op_id(next_op_id),
                path,
            }),
            Effect::PersistSettings {
                provider_id,
                model,
                settings,
            } => Some(ServiceCommand::SaveModelSettings {
                op_id: take_op_id(next_op_id),
                provider_id,
                model_id: model,
                settings_json: model_settings_to_json(&settings),
            }),
            Effect::Quit => {
                shutdown = true;
                Some(ServiceCommand::Shutdown)
            }
        };
        if let Some(command) = command
            && commands.send(command).await.is_err()
        {
            app.update(Action::Task(TaskEvent::Error {
                provider_id: None,
                scope: TaskScope::General,
                message: "The background service is unavailable.".into(),
                recoverable: false,
            }));
            return true;
        }
    }
    shutdown
}

fn take_op_id(next: &mut u64) -> u64 {
    let operation = *next;
    *next = next.wrapping_add(1).max(1);
    operation
}

fn model_settings_to_json(settings: &ModelSettings) -> serde_json::Value {
    serde_json::json!({
        "duration": settings.duration,
        "resolution": settings.resolution,
        "aspect_ratio": settings.aspect_ratio,
        "size": settings.size,
        "generate_audio": settings.generate_audio,
        "seed": settings.seed,
    })
}

fn model_settings_from_json(value: &serde_json::Value) -> Option<ModelSettings> {
    let object = value.as_object()?;
    Some(ModelSettings {
        duration: object
            .get("duration")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        resolution: object
            .get("resolution")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        aspect_ratio: object
            .get("aspect_ratio")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        size: object
            .get("size")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        generate_audio: object
            .get("generate_audio")
            .and_then(serde_json::Value::as_bool),
        seed: object.get("seed").and_then(serde_json::Value::as_i64),
    })
}

fn service_event_to_task(event: crate::workflow::ServiceEvent) -> Option<TaskEvent> {
    use crate::workflow::{ServiceEvent, ServiceScope};

    match event {
        ServiceEvent::Ready {
            providers,
            default_provider,
        } => Some(TaskEvent::Ready {
            providers: providers.into_iter().map(ui_provider_connection).collect(),
            default_provider,
        }),
        ServiceEvent::ApiKeyConnected {
            provider_id,
            credential_status,
            ..
        } => Some(TaskEvent::KeyValidated {
            provider_id,
            connection: if credential_status.persistent {
                ProviderConnectionKind::Connected
            } else {
                ProviderConnectionKind::SessionOnly
            },
            storage_note: credential_status.message,
        }),
        ServiceEvent::ApiKeyForgotten {
            provider_id,
            credential_status,
            ..
        } => Some(TaskEvent::KeyForgotten {
            provider_id,
            storage_note: credential_status.message,
        }),
        ServiceEvent::CatalogLoaded {
            provider_id,
            catalog,
            remembered_settings,
            ..
        } => Some(TaskEvent::CatalogLoaded {
            provider_id,
            models: catalog.models.iter().map(UiModel::from).collect(),
            stale: catalog.stale,
            remembered: remembered_settings
                .iter()
                .filter_map(|(model, settings)| {
                    model_settings_from_json(settings).map(|settings| (model.clone(), settings))
                })
                .collect(),
        }),
        ServiceEvent::QuoteReady {
            provider_id,
            model_id,
            quote,
            ..
        } => Some(TaskEvent::QuoteLoaded {
            provider_id,
            model_id,
            quote: CostView::from(&quote),
        }),
        ServiceEvent::SettingsSaved { .. }
        | ServiceEvent::DefaultProviderSaved { .. }
        | ServiceEvent::PreparationStarted { .. }
        | ServiceEvent::MediaUploadStarted { .. }
        | ServiceEvent::MediaUploadProgress { .. }
        | ServiceEvent::MediaUploadCompleted { .. }
        | ServiceEvent::ReviewReady { .. }
        | ServiceEvent::PreparedInvalidated { .. }
        | ServiceEvent::DraftSaved { .. }
        | ServiceEvent::DraftLoaded { .. }
        | ServiceEvent::UncertainSubmissionSaved { .. }
        | ServiceEvent::UncertainSubmissionCleared { .. }
        | ServiceEvent::UncertainSubmissionsLoaded { .. }
        | ServiceEvent::MonitorPaused { .. }
        | ServiceEvent::MonitorsPaused { .. }
        | ServiceEvent::ResumeAllStarted { .. }
        | ServiceEvent::ResumableJobsLoaded { .. }
        | ServiceEvent::JobRecoverySaved { .. }
        | ServiceEvent::ShutdownBlocked { .. }
        | ServiceEvent::VideoOpened { .. } => None,
        ServiceEvent::JobRecoveryWarning {
            provider_id,
            message,
            ..
        } => Some(TaskEvent::Error {
            provider_id: Some(provider_id),
            scope: TaskScope::General,
            message,
            recoverable: true,
        }),
        ServiceEvent::JobRecoveryFailed {
            provider_id,
            key,
            message,
            ..
        } => Some(TaskEvent::Error {
            provider_id: Some(provider_id),
            scope: TaskScope::Generation,
            message: format!("{message} Remote job id: {}", key.remote_job_id),
            recoverable: false,
        }),
        ServiceEvent::SubmissionUncertain {
            provider_id,
            message,
            ..
        } => Some(TaskEvent::Error {
            provider_id: Some(provider_id),
            scope: TaskScope::Generation,
            message,
            recoverable: false,
        }),
        ServiceEvent::UncertainSubmissionBlocked { record, .. } => Some(TaskEvent::Error {
            provider_id: Some(record.provider_id),
            scope: TaskScope::Generation,
            message: record.message,
            recoverable: false,
        }),
        ServiceEvent::SubmissionStarted { provider_id, .. } => {
            Some(TaskEvent::SubmissionStarted { provider_id })
        }
        ServiceEvent::JobAccepted {
            provider_id, job, ..
        } => Some(TaskEvent::JobAccepted {
            provider_id,
            job_id: job.id,
            status: job.status.as_str().to_owned(),
        }),
        ServiceEvent::JobUpdated {
            provider_id,
            job,
            record,
            ..
        } => {
            let status = job.status.as_str().to_owned();
            let detail = job
                .error
                .or(record.error)
                .unwrap_or_else(|| status_detail(&provider_id, &status));
            Some(TaskEvent::JobUpdated {
                provider_id,
                job_id: job.id,
                status,
                detail,
            })
        }
        ServiceEvent::PollWaiting {
            provider_id,
            next_in,
            ..
        } => Some(TaskEvent::PollWaiting {
            provider_id,
            seconds: next_in.as_secs().max(1),
        }),
        ServiceEvent::DownloadProgress {
            provider_id,
            written,
            total,
            ..
        } => Some(TaskEvent::DownloadProgress {
            provider_id,
            received: written,
            total,
        }),
        ServiceEvent::Downloaded {
            provider_id,
            job,
            record,
            path,
            ..
        } => {
            let cost = record
                .cost
                .or_else(|| job.cost())
                .map(|amount| amount.to_string());
            let currency = record.currency.clone();
            let request = record.request.as_ref().map(RequestDraft::from_domain);
            Some(TaskEvent::Completed(Completion {
                provider_id,
                job_id: job.id,
                path,
                cost,
                currency,
                request,
            }))
        }
        ServiceEvent::HistoryLoaded { records, .. } => Some(TaskEvent::HistoryLoaded(
            records.iter().map(HistoryItem::from).collect(),
        )),
        ServiceEvent::Imported {
            provider_id, job, ..
        } => Some(TaskEvent::Imported {
            provider_id,
            job_id: job.id,
            status: job.status.as_str().to_owned(),
        }),
        ServiceEvent::Cancelled { provider_id, .. } => Some(TaskEvent::Cancelled { provider_id }),
        ServiceEvent::Error {
            provider_id,
            scope,
            message,
            recoverable,
            ..
        } => Some(TaskEvent::Error {
            provider_id,
            scope: match scope {
                ServiceScope::Credential => TaskScope::Credential,
                ServiceScope::Catalog => TaskScope::Catalog,
                ServiceScope::Quote => TaskScope::Quote,
                ServiceScope::Generation => TaskScope::Generation,
                ServiceScope::Import => TaskScope::Import,
                ServiceScope::History => TaskScope::History,
                ServiceScope::OpenVideo => TaskScope::OpenVideo,
                ServiceScope::Preparation => TaskScope::Generation,
                ServiceScope::Startup | ServiceScope::Settings | ServiceScope::Draft => {
                    TaskScope::General
                }
            },
            message,
            recoverable,
        }),
        ServiceEvent::ShutdownComplete => None,
    }
}

fn ui_provider_connection(connection: crate::workflow::ProviderConnection) -> UiProvider {
    let connection_kind = if !connection.connected {
        ProviderConnectionKind::NeedsKey
    } else if connection.credential_status.persistent {
        ProviderConnectionKind::Connected
    } else {
        ProviderConnectionKind::SessionOnly
    };
    UiProvider {
        id: connection.descriptor.id,
        name: connection.descriptor.display_name,
        connection: connection_kind,
        storage_note: connection.credential_status.message,
    }
}

fn status_detail(provider_id: &ProviderId, status: &str) -> String {
    let provider = provider_name(provider_id);
    match status {
        "pending" => format!("{provider} queued the video generation job."),
        "in_progress" => format!("{provider} is rendering the video."),
        "completed" => "Render complete; preparing the download.".into(),
        value => format!("{provider} reported status: {}.", human_status(value)),
    }
}

fn spawn_input_thread(
    sender: tokio::sync::mpsc::UnboundedSender<Action>,
    running: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) => {
                        let _ = sender.send(Action::Key(key));
                    }
                    Ok(Event::Paste(value)) => {
                        let _ = sender.send(Action::Paste(value));
                    }
                    Ok(Event::Resize(width, height)) => {
                        let _ = sender.send(Action::Resize(width, height));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
}

fn current_clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn new() -> Self {
        Self { active: true }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            event::DisableBracketedPaste,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        );
    }
}
