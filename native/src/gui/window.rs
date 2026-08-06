use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use secrecy::SecretString;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use crate::config::AppPaths;
use crate::domain::{
    CostQuote, DraftMedia, GenerationDraft, MediaCardinality, MediaKind, MediaRole, MediaSource,
    ProviderId, ProviderJobKey, VideoCatalog, VideoModel,
};
use crate::gui_state::{
    DraftEditorState, UncertainSubmissionRecord, generation_draft_fingerprint_candidates,
};
use crate::history::JobRecord;
use crate::workflow::{
    PreparedGenerationId, ProviderConnection, RecoveryStore, ServiceCommand, ServiceConfig,
    ServiceEvent, ServiceHandle, spawn_service,
};

const PROVIDERS: [(&str, &str); 2] = [("openrouter", "OpenRouter"), ("fal", "fal.ai")];
const ANIMATION_FRAMES: [&str; 8] = ["·", "✦", "· ✧", "✦ ·", "✧", "· ✦", "✧ ·", "✦"];

pub fn install_style() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn present(
    application: &adw::Application,
    runtime: Arc<Runtime>,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths = AppPaths::discover()?.ensure()?;
    let handle = {
        let _guard = runtime.enter();
        spawn_service(paths, ServiceConfig::default())?
    };
    let window = HarnessWindow::new(application, runtime, handle);
    // GTK owns the visible widgets, but signal handlers intentionally keep
    // only weak controller references. Retain the controller until the
    // service confirms its local state is safe, then break this cycle in the
    // ShutdownComplete arm.
    window.keep_alive.replace(Some(Rc::clone(&window)));
    window.present();
    Ok(())
}

pub fn present_startup_error(application: &adw::Application, message: &str) {
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Video Harness")
        .default_width(620)
        .default_height(420)
        .build();
    let page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Video Harness could not start")
        .description(message)
        .build();
    window.set_content(Some(&page));
    window.present();
}

#[derive(Debug, Clone)]
struct MediaItem {
    source: MediaSource,
    role: MediaRole,
}

struct OptionWidgets {
    duration: gtk::DropDown,
    durations: RefCell<Vec<Option<u32>>>,
    resolution: gtk::DropDown,
    resolutions: RefCell<Vec<Option<String>>>,
    aspect: gtk::DropDown,
    aspects: RefCell<Vec<Option<String>>>,
    size: gtk::DropDown,
    sizes: RefCell<Vec<Option<String>>>,
    audio: gtk::Switch,
    seed: gtk::Entry,
    schema_box: gtk::Box,
    schema_controls: RefCell<BTreeMap<String, SchemaControl>>,
    advanced: gtk::TextView,
}

enum SchemaControl {
    Choice {
        widget: gtk::DropDown,
        values: Vec<Option<serde_json::Value>>,
    },
    Text {
        widget: gtk::Entry,
        kind: SchemaTextKind,
    },
}

enum SchemaUiValue {
    Choice(Option<serde_json::Value>),
    Text(String),
}

struct ModelOptionsSnapshot {
    duration: Option<u32>,
    resolution: Option<String>,
    aspect_ratio: Option<String>,
    size: Option<String>,
    audio: bool,
    seed: String,
    schema: BTreeMap<String, SchemaUiValue>,
}

#[derive(Clone, Copy)]
enum SchemaTextKind {
    String,
    Integer,
    Number,
}

struct ProviderWidgets {
    status: gtk::Label,
    storage: gtk::Label,
    key: gtk::PasswordEntry,
    remember: gtk::CheckButton,
    connect: gtk::Button,
    forget: gtk::Button,
}

struct JobWidgets {
    _key: ProviderJobKey,
    _root: gtk::ListBoxRow,
    title: gtk::Label,
    status: gtk::Label,
    detail: gtk::Label,
    animation: gtk::Label,
    progress: gtk::ProgressBar,
    video: gtk::Video,
    pause: gtk::Button,
    resume: gtk::Button,
    open: gtk::Button,
    local_path: RefCell<Option<PathBuf>>,
    active: Cell<bool>,
}

struct PreparedReview {
    id: PreparedGenerationId,
    revision: u64,
    draft_fingerprint: String,
}

struct HarnessWindow {
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    _runtime: Arc<Runtime>,
    commands: mpsc::Sender<ServiceCommand>,
    events: RefCell<mpsc::UnboundedReceiver<ServiceEvent>>,
    next_op: Cell<u64>,
    revision: Cell<u64>,
    loading_draft: Cell<bool>,
    save_timer: RefCell<Option<glib::SourceId>>,
    prepared: RefCell<Option<PreparedReview>>,
    pending_submit_op: Cell<Option<u64>>,
    pending_submit_revision: Cell<Option<u64>>,
    pending_submit_provider: RefCell<Option<ProviderId>>,
    uncertain_revision: Cell<Option<u64>>,
    uncertain_submissions: RefCell<BTreeMap<(ProviderId, String), UncertainSubmissionRecord>>,
    pending_uncertain_clears: RefCell<HashMap<u64, (ProviderId, String)>>,
    pause_before_shutdown: Cell<bool>,
    pending_close_draft_op: Cell<Option<u64>>,
    shutdown_requested: Cell<bool>,
    service_disconnected: Cell<bool>,
    allow_close: Cell<bool>,
    keep_alive: RefCell<Option<Rc<HarnessWindow>>>,

    view_stack: adw::ViewStack,
    prompt: gtk::TextView,
    provider: gtk::DropDown,
    model: gtk::DropDown,
    model_ids: RefCell<Vec<String>>,
    model_provider: RefCell<Option<ProviderId>>,
    model_description: gtk::Label,
    catalogs: RefCell<BTreeMap<ProviderId, VideoCatalog>>,
    pending_draft: RefCell<Option<(GenerationDraft, DraftEditorState, u64)>>,
    options: OptionWidgets,
    media: RefCell<Vec<MediaItem>>,
    media_list: gtk::ListBox,
    media_empty: gtk::Label,
    add_files: gtk::Button,
    add_url: gtk::Button,
    drop_zone: gtk::Box,
    drop_title: gtk::Label,
    drop_hint: gtk::Label,
    remote_url: gtk::Entry,
    remote_kind: gtk::DropDown,
    remote_role: gtk::DropDown,
    compatibility: gtk::Label,
    review: gtk::Button,

    jobs_list: gtk::ListBox,
    jobs_stack: gtk::Stack,
    jobs: RefCell<HashMap<ProviderJobKey, Rc<JobWidgets>>>,
    active_jobs: RefCell<HashSet<ProviderJobKey>>,
    latest_video: RefCell<Option<PathBuf>>,
    animation_frame: Cell<usize>,

    provider_widgets: BTreeMap<ProviderId, ProviderWidgets>,
    default_provider: gtk::DropDown,
}

impl HarnessWindow {
    fn new(
        application: &adw::Application,
        runtime: Arc<Runtime>,
        handle: ServiceHandle,
    ) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Video Harness")
            .default_width(1100)
            .default_height(790)
            .build();
        let toast_overlay = adw::ToastOverlay::new();
        let view_stack = adw::ViewStack::new();

        let compose = ComposeWidgets::build();
        let jobs = JobsWidgets::build();
        let providers = ProvidersWidgets::build();

        let compose_page = view_stack.add_titled(&compose.page, Some("compose"), "New Generation");
        compose_page.set_icon_name(Some("document-new-symbolic"));
        let jobs_page = view_stack.add_titled(&jobs.page, Some("jobs"), "Jobs");
        jobs_page.set_icon_name(Some("media-playlist-video-symbolic"));
        let providers_page =
            view_stack.add_titled(&providers.page, Some("providers"), "Providers & Settings");
        providers_page.set_icon_name(Some("preferences-system-symbolic"));

        let title = adw::ViewSwitcher::new();
        title.set_stack(Some(&view_stack));
        title.set_policy(adw::ViewSwitcherPolicy::Wide);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let switcher = adw::ViewSwitcherBar::new();
        switcher.set_stack(Some(&view_stack));
        switcher.set_reveal(true);

        toast_overlay.set_child(Some(&view_stack));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toast_overlay));
        toolbar.add_bottom_bar(&switcher);
        window.set_content(Some(&toolbar));

        let this = Rc::new(Self {
            window,
            toast_overlay,
            _runtime: runtime,
            commands: handle.commands,
            events: RefCell::new(handle.events),
            next_op: Cell::new(1),
            revision: Cell::new(0),
            loading_draft: Cell::new(false),
            save_timer: RefCell::new(None),
            prepared: RefCell::new(None),
            pending_submit_op: Cell::new(None),
            pending_submit_revision: Cell::new(None),
            pending_submit_provider: RefCell::new(None),
            uncertain_revision: Cell::new(None),
            uncertain_submissions: RefCell::new(BTreeMap::new()),
            pending_uncertain_clears: RefCell::new(HashMap::new()),
            pause_before_shutdown: Cell::new(false),
            pending_close_draft_op: Cell::new(None),
            shutdown_requested: Cell::new(false),
            service_disconnected: Cell::new(false),
            allow_close: Cell::new(false),
            keep_alive: RefCell::new(None),
            view_stack,
            prompt: compose.prompt,
            provider: compose.provider,
            model: compose.model,
            model_ids: RefCell::new(Vec::new()),
            model_provider: RefCell::new(None),
            model_description: compose.model_description,
            catalogs: RefCell::new(BTreeMap::new()),
            pending_draft: RefCell::new(None),
            options: compose.options,
            media: RefCell::new(Vec::new()),
            media_list: compose.media_list,
            media_empty: compose.media_empty,
            add_files: compose.add_files,
            add_url: compose.add_url,
            drop_zone: compose.drop_zone,
            drop_title: compose.drop_title,
            drop_hint: compose.drop_hint,
            remote_url: compose.remote_url,
            remote_kind: compose.remote_kind,
            remote_role: compose.remote_role,
            compatibility: compose.compatibility,
            review: compose.review,
            jobs_list: jobs.list,
            jobs_stack: jobs.stack,
            jobs: RefCell::new(HashMap::new()),
            active_jobs: RefCell::new(HashSet::new()),
            latest_video: RefCell::new(None),
            animation_frame: Cell::new(0),
            provider_widgets: providers.providers,
            default_provider: providers.default_provider,
        });
        this.connect(jobs.resume_all, jobs.pause_all);
        this.update_media_input_availability();
        this.start_event_pump();
        this.start_animation();
        this
    }

    fn present(&self) {
        self.window.present();
    }

    fn weak(this: &Rc<Self>) -> Weak<Self> {
        Rc::downgrade(this)
    }

    fn op_id(&self) -> u64 {
        let id = self.next_op.get();
        self.next_op.set(id.saturating_add(1));
        id
    }

    fn send(&self, command: ServiceCommand) -> bool {
        if self.commands.try_send(command).is_err() {
            self.toast(
                "The background service is busy. Please try again.",
                "dialog-warning-symbolic",
            );
            false
        } else {
            true
        }
    }

    fn clear_pending_submission(&self, op_id: u64) -> Option<(u64, Option<ProviderId>)> {
        if self.pending_submit_op.get() != Some(op_id) {
            return None;
        }
        self.pending_submit_op.set(None);
        let revision = self
            .pending_submit_revision
            .take()
            .unwrap_or_else(|| self.revision.get());
        let provider = self.pending_submit_provider.borrow_mut().take();
        Some((revision, provider))
    }

    fn toast(&self, message: &str, _icon: &str) {
        let toast = adw::Toast::builder().title(message).timeout(5).build();
        self.toast_overlay.add_toast(toast);
    }

    fn connect(self: &Rc<Self>, resume_all: gtk::Button, pause_all: gtk::Button) {
        let weak = Self::weak(self);
        self.prompt.buffer().connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.draft_changed();
            }
        });

        let weak = Self::weak(self);
        self.provider.connect_selected_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                if this.loading_draft.get() {
                    return;
                }
                this.refresh_models();
                this.update_media_input_availability();
                this.rebuild_media();
                this.draft_changed();
            }
        });
        let weak = Self::weak(self);
        self.model.connect_selected_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                if this.loading_draft.get() {
                    return;
                }
                this.refresh_model_controls();
                this.draft_changed();
            }
        });

        for dropdown in [
            &self.options.duration,
            &self.options.resolution,
            &self.options.aspect,
            &self.options.size,
        ] {
            let weak = Self::weak(self);
            dropdown.connect_selected_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.draft_changed();
                }
            });
        }
        let weak = Self::weak(self);
        self.options.audio.connect_active_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.draft_changed();
            }
        });
        let weak = Self::weak(self);
        self.options.seed.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.draft_changed();
            }
        });
        let weak = Self::weak(self);
        self.options.advanced.buffer().connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.draft_changed();
            }
        });

        let weak = Self::weak(self);
        self.add_files.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.choose_files();
            }
        });
        let weak = Self::weak(self);
        self.add_url.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.add_from_entry();
            }
        });
        let weak = Self::weak(self);
        self.remote_url.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.add_from_entry();
            }
        });
        let weak = Self::weak(self);
        self.remote_kind.connect_selected_notify(move |dropdown| {
            if let Some(this) = weak.upgrade() {
                this.configure_remote_role(remote_kind_for_index(dropdown.selected()));
            }
        });

        let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Self::weak(self);
        drop_target.connect_drop(move |_, value, _, _| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            let Some(this) = weak.upgrade() else {
                return false;
            };
            if this.selected_provider() == ProviderId::openrouter() {
                this.toast(
                    "OpenRouter accepts reference media by public HTTPS URL only.",
                    "dialog-warning-symbolic",
                );
                return false;
            }
            let mut added = 0usize;
            let mut rejected = 0usize;
            for file in files.files() {
                if let Some(path) = file.path() {
                    let Some(kind) = classify_local_reference(&path) else {
                        rejected += 1;
                        continue;
                    };
                    this.media.borrow_mut().push(MediaItem {
                        source: MediaSource::local(path),
                        role: default_role_for_kind(kind),
                    });
                    added += 1;
                }
            }
            if added > 0 {
                this.rebuild_media();
                this.draft_changed();
                if rejected > 0 {
                    this.toast(
                        &format!("Skipped {rejected} unsupported file(s)."),
                        "dialog-warning-symbolic",
                    );
                }
                true
            } else {
                if rejected > 0 {
                    this.toast(
                        "Reference media must be an image, MP4/MOV video, or MP3/WAV audio file.",
                        "dialog-warning-symbolic",
                    );
                }
                false
            }
        });
        self.drop_zone.add_controller(drop_target);

        let weak = Self::weak(self);
        self.review.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.review_or_resolve();
            }
        });

        let weak = Self::weak(self);
        resume_all.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                for (key, widgets) in this.jobs.borrow().iter() {
                    if widgets.resume.is_sensitive() {
                        widgets.active.set(true);
                        widgets.pause.set_sensitive(true);
                        widgets.resume.set_sensitive(false);
                        widgets.status.set_text("resuming");
                        this.active_jobs.borrow_mut().insert(key.clone());
                    }
                }
                this.send(ServiceCommand::ResumeAll {
                    op_id: this.op_id(),
                });
                this.toast(
                    "Resuming saved remote jobs…",
                    "media-playback-start-symbolic",
                );
            }
        });
        let weak = Self::weak(self);
        pause_all.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send(ServiceCommand::PauseAll {
                    op_id: this.op_id(),
                });
            }
        });

        for (provider_id, widgets) in &self.provider_widgets {
            let weak = Self::weak(self);
            let id = provider_id.clone();
            widgets.connect.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                let Some(widgets) = this.provider_widgets.get(&id) else {
                    return;
                };
                let raw = widgets.key.text().trim().to_owned();
                if raw.is_empty() {
                    this.toast("Paste an API key first.", "dialog-warning-symbolic");
                    return;
                }
                // Clear the visible widget before ownership crosses into the service.
                widgets.key.set_text("");
                this.send(ServiceCommand::ConnectApiKey {
                    op_id: this.op_id(),
                    provider_id: id.clone(),
                    key: SecretString::from(raw),
                    persist_on_success: widgets.remember.is_active(),
                });
                widgets.status.set_text("Checking credentials…");
            });
            let weak = Self::weak(self);
            let id = provider_id.clone();
            widgets.forget.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.send(ServiceCommand::ForgetApiKey {
                        op_id: this.op_id(),
                        provider_id: id.clone(),
                    });
                }
            });
        }
        let weak = Self::weak(self);
        self.default_provider
            .connect_selected_notify(move |dropdown| {
                let Some(this) = weak.upgrade() else { return };
                let provider_id = provider_id_for_index(dropdown.selected());
                this.send(ServiceCommand::SaveDefaultProvider {
                    op_id: this.op_id(),
                    provider_id,
                });
            });

        let open_action = gio::SimpleAction::new("open-video", None);
        let weak = Self::weak(self);
        open_action.connect_activate(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.open_latest_video();
            }
        });
        self.window.add_action(&open_action);
        if let Some(application) = self.window.application().and_downcast::<adw::Application>() {
            application.set_accels_for_action("win.open-video", &["<Primary>o"]);
        }

        let weak = Self::weak(self);
        self.window.connect_close_request(move |_| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if this.allow_close.get() {
                this.keep_alive.borrow_mut().take();
                return glib::Propagation::Proceed;
            }
            if this.pending_submit_op.get().is_some() {
                this.toast(
                    "Please keep Video Harness open until the paid request outcome and recovery record are safe.",
                    "dialog-warning-symbolic",
                );
                return glib::Propagation::Stop;
            }
            if this.pause_before_shutdown.get()
                || this.pending_close_draft_op.get().is_some()
                || this.shutdown_requested.get()
            {
                this.toast(
                    "Finishing local state safely before closing…",
                    "document-save-symbolic",
                );
                return glib::Propagation::Stop;
            }
            if !this.active_jobs.borrow().is_empty() {
                this.confirm_quit();
                return glib::Propagation::Stop;
            }
            this.request_shutdown();
            glib::Propagation::Stop
        });
    }

    fn selected_provider(&self) -> ProviderId {
        provider_id_for_index(self.provider.selected())
    }

    fn selected_model_id(&self) -> Option<String> {
        self.model_ids
            .borrow()
            .get(self.model.selected() as usize)
            .cloned()
    }

    fn selected_model(&self) -> Option<VideoModel> {
        let provider = self.selected_provider();
        let model = self.selected_model_id()?;
        self.catalogs
            .borrow()
            .get(&provider)
            .and_then(|catalog| catalog.find(&model))
            .cloned()
    }

    fn current_uncertain_submission(&self) -> Option<UncertainSubmissionRecord> {
        let draft = self.build_draft(true).ok()?;
        let fingerprints = generation_draft_fingerprint_candidates(&draft).ok()?;
        let records = self.uncertain_submissions.borrow();
        fingerprints.into_iter().find_map(|fingerprint| {
            records
                .get(&(draft.provider_id.clone(), fingerprint))
                .cloned()
        })
    }

    fn review_or_resolve(self: &Rc<Self>) {
        if let Some(record) = self.current_uncertain_submission() {
            self.show_uncertain_resolution(record);
        } else {
            self.prepare_review();
        }
    }

    fn draft_changed(self: &Rc<Self>) {
        if self.loading_draft.get() {
            return;
        }
        let revision = self.revision.get().saturating_add(1);
        self.revision.set(revision);
        self.prepared.borrow_mut().take();
        self.send(ServiceCommand::InvalidatePrepared {
            op_id: self.op_id(),
            revision,
        });
        self.update_compatibility();
        if let Some(source) = self.save_timer.borrow_mut().take() {
            source.remove();
        }
        let weak = Self::weak(self);
        let source = glib::timeout_add_local_once(Duration::from_millis(650), move || {
            if let Some(this) = weak.upgrade() {
                this.save_draft();
            }
        });
        self.save_timer.replace(Some(source));
    }

    fn build_draft(&self, strict: bool) -> Result<GenerationDraft, String> {
        let model = match self.selected_model_id() {
            Some(model) => model,
            None if !strict => String::new(),
            None => return Err("Choose a model".to_owned()),
        };
        let mut draft = GenerationDraft::new(self.selected_provider(), model, text(&self.prompt))
            .map_err(|error| error.to_string())?;
        draft.duration = selected_copy(&self.options.durations, self.options.duration.selected());
        draft.resolution = selected_clone(
            &self.options.resolutions,
            self.options.resolution.selected(),
        );
        draft.aspect_ratio = selected_clone(&self.options.aspects, self.options.aspect.selected());
        draft.size = selected_clone(&self.options.sizes, self.options.size.selected());
        draft.generate_audio = self
            .options
            .audio
            .is_sensitive()
            .then_some(self.options.audio.is_active());
        let seed = self.options.seed.text();
        draft.seed = if !self.options.seed.is_sensitive() || seed.trim().is_empty() {
            None
        } else {
            match seed.trim().parse::<i64>() {
                Ok(seed) => Some(seed),
                Err(_) if !strict => None,
                Err(_) => return Err("Seed must be a whole number".into()),
            }
        };
        let advanced = text(&self.options.advanced);
        let mut adapter = if advanced.trim().is_empty() {
            serde_json::Map::new()
        } else {
            match serde_json::from_str::<serde_json::Value>(&advanced) {
                Ok(serde_json::Value::Object(object)) => object,
                Ok(_) if !strict => serde_json::Map::new(),
                Ok(_) => return Err("Advanced provider JSON must be an object".into()),
                Err(_) if !strict => serde_json::Map::new(),
                Err(error) => return Err(format!("Advanced provider JSON is invalid: {error}")),
            }
        };
        for (name, control) in self.options.schema_controls.borrow().iter() {
            match schema_control_value(control) {
                Ok(Some(value)) => {
                    adapter.insert(name.clone(), value);
                }
                Ok(None) => {}
                Err(_) if !strict => {}
                Err(message) => return Err(format!("{name}: {message}")),
            }
        }
        draft.adapter_options = (!adapter.is_empty()).then_some(serde_json::Value::Object(adapter));
        draft.media = self
            .media
            .borrow()
            .iter()
            .map(|item| DraftMedia {
                source: item.source.clone(),
                role: item.role,
            })
            .collect();
        if strict {
            draft.validate().map_err(|error| error.to_string())?;
            if draft.provider_id == ProviderId::openrouter()
                && draft
                    .media
                    .iter()
                    .any(|media| matches!(media.source, MediaSource::LocalFile { .. }))
            {
                return Err(
                    "OpenRouter reference media must use public HTTPS URLs. Local rows are kept in your draft, but cannot be reviewed with OpenRouter; switch to fal.ai or replace them with URLs."
                        .into(),
                );
            }
            self.validate_model_media(&draft)?;
        }
        Ok(draft)
    }

    fn validate_model_media(&self, draft: &GenerationDraft) -> Result<(), String> {
        let Some(model) = self.selected_model() else {
            return Err("The selected model is not in the provider catalog".into());
        };
        let has_video_or_audio = draft
            .media
            .iter()
            .any(|media| media.role.kind() != MediaKind::Image);
        if has_video_or_audio
            && self
                .catalogs
                .borrow()
                .get(&draft.provider_id)
                .is_some_and(|catalog| catalog.stale)
        {
            return Err(
                "Refresh this provider's model catalog before reviewing video or audio references. Cached capabilities may be out of date."
                    .into(),
            );
        }
        for kind in [MediaKind::Image, MediaKind::Video, MediaKind::Audio] {
            if !draft.media.iter().any(|media| media.role.kind() == kind) {
                continue;
            }
            match &model.input_modalities {
                Some(modalities) if !modalities.contains(&kind) => {
                    return Err(format!(
                        "{} does not support {} references. Choose another model or remove the incompatible media.",
                        model.name,
                        media_kind_plural(kind)
                    ));
                }
                None if kind != MediaKind::Image => {
                    return Err(format!(
                        "{} does not publish confirmed {} input support. Refresh the catalog or choose a model that explicitly supports it.",
                        model.name,
                        media_kind_plural(kind)
                    ));
                }
                _ => {}
            }
        }
        if draft.provider_id == ProviderId::fal() {
            for (role, kind) in [
                (MediaRole::Reference, MediaKind::Image),
                (MediaRole::VideoInput, MediaKind::Video),
                (MediaRole::AudioInput, MediaKind::Audio),
            ] {
                let count = draft
                    .media
                    .iter()
                    .filter(|media| media.role == role)
                    .count();
                if count == 0 {
                    continue;
                }
                let bindings = model
                    .media_bindings
                    .iter()
                    .filter(|binding| binding.kind == kind)
                    .collect::<Vec<_>>();
                if bindings.is_empty() {
                    return Err(format!(
                        "{} cannot bind {} references to a fal.ai input field. Choose another model or remove them.",
                        model.name,
                        media_kind_plural(kind)
                    ));
                }
                let capacity = bindings.iter().fold(0usize, |capacity, binding| {
                    let binding_capacity = match binding.cardinality {
                        MediaCardinality::Scalar => 1,
                        MediaCardinality::List => binding.max_items.unwrap_or(usize::MAX),
                    };
                    capacity.saturating_add(binding_capacity)
                });
                if count > capacity {
                    return Err(format!(
                        "{} accepts at most {capacity} {} reference(s); this draft has {count}.",
                        model.name,
                        kind.as_str()
                    ));
                }
            }
        }
        for media in &draft.media {
            let required = match media.role {
                MediaRole::StartFrame => Some("first_frame"),
                MediaRole::EndFrame => Some("last_frame"),
                MediaRole::Reference | MediaRole::VideoInput | MediaRole::AudioInput => None,
            };
            if let Some(required) = required
                && !model
                    .supported_frame_images
                    .iter()
                    .any(|item| item == required)
            {
                return Err(format!(
                    "{} does not support a {required} input",
                    model.name
                ));
            }
        }
        Ok(())
    }

    fn draft_editor_state(&self) -> DraftEditorState {
        let schema_text = self
            .options
            .schema_controls
            .borrow()
            .iter()
            .filter_map(|(name, control)| match control {
                SchemaControl::Text { widget, .. } => {
                    Some((name.clone(), widget.text().to_string()))
                }
                SchemaControl::Choice { .. } => None,
            })
            .collect();
        DraftEditorState {
            seed_text: self.options.seed.text().to_string(),
            advanced_json_text: text(&self.options.advanced),
            schema_text,
        }
    }

    fn queue_draft_save(&self) -> Result<Option<u64>, ()> {
        self.save_timer.borrow_mut().take();
        let Ok(draft) = self.build_draft(false) else {
            return Ok(None);
        };
        let editor_state = self.draft_editor_state();
        let op_id = self.op_id();
        self.send(ServiceCommand::SaveDraft {
            op_id,
            draft,
            editor_state,
            revision: self.revision.get(),
        })
        .then_some(Some(op_id))
        .ok_or(())
    }

    fn save_draft(&self) {
        let _ = self.queue_draft_save();
    }

    fn prepare_review(&self) {
        if self.close_in_progress() {
            self.toast(
                "Finish or cancel closing before preparing another generation.",
                "dialog-warning-symbolic",
            );
            return;
        }
        let draft = match self.build_draft(true) {
            Ok(draft) => draft,
            Err(message) => {
                self.show_compatibility(&message, "harness-error");
                return;
            }
        };
        self.review.set_sensitive(false);
        self.review.set_label("Preparing review…");
        self.show_compatibility(
            if draft
                .media
                .iter()
                .any(|item| matches!(item.source, MediaSource::LocalFile { .. }))
            {
                "Checking and uploading local reference media…"
            } else {
                "Validating controls and fetching a fresh price…"
            },
            "harness-warning",
        );
        self.send(ServiceCommand::PrepareGeneration {
            op_id: self.op_id(),
            draft,
            revision: self.revision.get(),
        });
    }

    fn update_compatibility(&self) {
        self.view_stack.set_sensitive(!self.close_in_progress());
        if self.close_in_progress() {
            self.review.set_sensitive(false);
            self.review.set_label("Closing safely…");
            self.show_compatibility(
                "Finishing local state before closing Video Harness.",
                "harness-warning",
            );
            return;
        }
        if self.service_disconnected.get() {
            self.review.set_sensitive(false);
            self.review.set_label("Background service stopped");
            self.show_compatibility(
                "The background service stopped. Close and reopen Video Harness before preparing another generation.",
                "harness-error",
            );
            return;
        }
        if self.pending_submit_op.get().is_some() {
            self.review.set_sensitive(false);
            self.review.set_label("Submitting…");
            self.show_compatibility(
                "Submitting one paid request. Keep Video Harness open until its remote job ID is safely stored.",
                "harness-warning",
            );
            return;
        }
        if let Some(record) = self.current_uncertain_submission() {
            let key = (record.provider_id.clone(), record.draft_fingerprint.clone());
            let clearing = self
                .pending_uncertain_clears
                .borrow()
                .values()
                .any(|pending| pending == &key);
            self.review.set_sensitive(!clearing);
            self.review.set_label(if clearing {
                "Clearing safety hold…"
            } else {
                "Resolve uncertain submission"
            });
            self.show_compatibility(
                "This exact draft may already have been accepted. Check the provider dashboard before allowing another paid request.",
                "harness-error",
            );
            return;
        }
        if self.uncertain_revision.get() == Some(self.revision.get()) {
            self.review.set_sensitive(false);
            self.review.set_label("Submission outcome uncertain");
            self.show_compatibility(
                "The provider may already be generating this exact draft. Check its dashboard before retrying, or edit the draft to create a distinct request.",
                "harness-error",
            );
            return;
        }
        let message = match self.build_draft(true) {
            Ok(draft)
                if draft.provider_id == ProviderId::fal()
                    && draft
                        .media
                        .iter()
                        .any(|item| matches!(item.source, MediaSource::LocalFile { .. })) =>
            {
                self.review
                    .set_sensitive(self.pending_submit_op.get().is_none());
                self.review.set_label("Review generation");
                self.show_compatibility(
                    "Ready to review. Local reference media uploads to a public fal CDN at Review and expires after 24 hours.",
                    "harness-good",
                );
                return;
            }
            Ok(_) => {
                self.review
                    .set_sensitive(self.pending_submit_op.get().is_none());
                self.review.set_label("Review generation");
                self.show_compatibility(
                    "Ready to review — nothing has been submitted.",
                    "harness-good",
                );
                return;
            }
            Err(message) => message,
        };
        self.review.set_sensitive(false);
        self.review.set_label("Review generation");
        let class = if message.contains("OpenRouter") || message.contains("does not support") {
            "harness-warning"
        } else {
            "harness-muted"
        };
        self.show_compatibility(&message, class);
    }

    fn show_compatibility(&self, message: &str, class: &str) {
        for candidate in [
            "harness-muted",
            "harness-good",
            "harness-warning",
            "harness-error",
        ] {
            self.compatibility.remove_css_class(candidate);
        }
        self.compatibility.add_css_class(class);
        self.compatibility.set_text(message);
    }

    fn close_in_progress(&self) -> bool {
        self.pause_before_shutdown.get()
            || self.pending_close_draft_op.get().is_some()
            || self.shutdown_requested.get()
    }

    fn update_media_input_availability(&self) {
        let local_allowed = self.selected_provider() != ProviderId::openrouter();
        self.add_files.set_sensitive(local_allowed);
        self.drop_zone.set_sensitive(local_allowed);
        if local_allowed {
            self.add_files
                .set_tooltip_text(Some("Choose local image, video, or audio reference files"));
            self.drop_title.set_text("Drop reference media here");
            self.drop_hint
                .set_text("Files stay local until you press Review");
        } else {
            self.add_files
                .set_tooltip_text(Some("OpenRouter accepts public HTTPS reference URLs only"));
            self.drop_title.set_text("OpenRouter uses URL references");
            self.drop_hint
                .set_text("Choose a media type and add a public HTTPS URL below");
        }
    }

    fn configure_remote_role(&self, kind: Option<MediaKind>) {
        let labels: &[&str] = match kind {
            Some(MediaKind::Image) => &["Reference", "Start frame", "End frame"],
            Some(MediaKind::Video) => &["Video input"],
            Some(MediaKind::Audio) => &["Audio input"],
            None => &["Image role"],
        };
        self.remote_role
            .set_model(Some(&gtk::StringList::new(labels)));
        self.remote_role.set_selected(0);
        self.remote_role
            .set_sensitive(kind == Some(MediaKind::Image));
    }

    fn choose_files(self: &Rc<Self>) {
        if self.selected_provider() == ProviderId::openrouter() {
            self.toast(
                "OpenRouter accepts reference media by public HTTPS URL only.",
                "dialog-warning-symbolic",
            );
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Choose reference media")
            .modal(true)
            .build();
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Supported reference media"));
        for mime in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "image/avif",
            "image/bmp",
            "image/tiff",
            "video/mp4",
            "video/quicktime",
            "audio/mpeg",
            "audio/wav",
            "audio/x-wav",
        ] {
            filter.add_mime_type(mime);
        }
        for suffix in [
            "png", "jpg", "jpeg", "webp", "gif", "avif", "bmp", "tif", "tiff", "mp4", "mov", "mp3",
            "wav",
        ] {
            filter.add_suffix(suffix);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        let weak = Self::weak(self);
        dialog.open_multiple(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                let Ok(files) = result else { return };
                let Some(this) = weak.upgrade() else { return };
                let mut media = this.media.borrow_mut();
                for index in 0..files.n_items() {
                    let Some(file) = files.item(index).and_downcast::<gio::File>() else {
                        continue;
                    };
                    if let Some(path) = file.path() {
                        let Some(kind) = classify_local_reference(&path) else {
                            continue;
                        };
                        media.push(MediaItem {
                            source: MediaSource::local(path),
                            role: default_role_for_kind(kind),
                        });
                    }
                }
                drop(media);
                this.rebuild_media();
                this.draft_changed();
            },
        );
    }

    fn add_from_entry(self: &Rc<Self>) {
        let value = self.remote_url.text().trim().to_owned();
        if value.is_empty() {
            self.toast(
                "Paste a public HTTPS URL or file URI first.",
                "dialog-warning-symbolic",
            );
            return;
        }
        let Some(kind) = remote_kind_for_index(self.remote_kind.selected()) else {
            self.toast(
                "Choose whether the URL contains an image, video, or audio reference.",
                "dialog-warning-symbolic",
            );
            return;
        };
        let role = role_for_kind(kind, self.remote_role.selected());
        let source = if value.starts_with("file:") {
            if self.selected_provider() == ProviderId::openrouter() {
                self.toast(
                    "OpenRouter accepts reference media by public HTTPS URL only. The local file was not added.",
                    "dialog-error-symbolic",
                );
                return;
            }
            let file = gio::File::for_uri(&value);
            match file.path() {
                Some(path) => match classify_local_reference(&path) {
                    Some(actual) if actual == kind => MediaSource::local(path),
                    Some(_) => {
                        self.toast(
                            "The selected media type does not match that local file.",
                            "dialog-error-symbolic",
                        );
                        return;
                    }
                    None => {
                        self.toast(
                            "Reference media must be a valid image, MP4/MOV video, or MP3/WAV audio file.",
                            "dialog-error-symbolic",
                        );
                        return;
                    }
                },
                None => {
                    self.toast(
                        "That file URI does not point to a local file.",
                        "dialog-error-symbolic",
                    );
                    return;
                }
            }
        } else {
            match MediaSource::remote(value) {
                Ok(source) => source,
                Err(error) => {
                    self.toast(&error.to_string(), "dialog-error-symbolic");
                    return;
                }
            }
        };
        self.media.borrow_mut().push(MediaItem { source, role });
        self.remote_url.set_text("");
        self.remote_kind.set_selected(0);
        self.remote_role.set_selected(0);
        self.rebuild_media();
        self.draft_changed();
    }

    fn rebuild_media(self: &Rc<Self>) {
        while let Some(child) = self.media_list.first_child() {
            self.media_list.remove(&child);
        }
        let items = self.media.borrow().clone();
        self.media_empty.set_visible(items.is_empty());
        self.media_list.set_visible(!items.is_empty());
        let mut ordinals = [0usize; 3];

        for (index, item) in items.into_iter().enumerate() {
            let kind = item.role.kind();
            let kind_index = media_kind_index(kind);
            ordinals[kind_index] += 1;
            let typed_ordinal = format!("{} {}", media_kind_label(kind), ordinals[kind_index]);
            let row = gtk::ListBoxRow::new();
            row.set_activatable(false);
            row.set_selectable(false);
            row.set_tooltip_text(Some(&format!(
                "{typed_ordinal}: {}",
                media_name(&item.source)
            )));
            let body = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            body.add_css_class("harness-story-row");

            let preview = gtk::Button::new();
            preview.add_css_class("flat");
            preview.set_tooltip_text(Some(&format!("Preview {typed_ordinal}")));
            match &item.source {
                MediaSource::LocalFile { path } if kind == MediaKind::Image => {
                    let picture = gtk::Picture::for_file(&gio::File::for_path(path));
                    picture.set_size_request(72, 72);
                    picture.set_content_fit(gtk::ContentFit::Cover);
                    preview.set_child(Some(&picture));
                    let weak = Self::weak(self);
                    let path = path.clone();
                    preview.connect_clicked(move |_| {
                        if let Some(this) = weak.upgrade() {
                            this.preview_file(&path, kind);
                        }
                    });
                }
                MediaSource::LocalFile { path } => {
                    let icon = gtk::Image::from_icon_name(media_kind_icon(kind));
                    icon.set_pixel_size(32);
                    preview.set_child(Some(&icon));
                    let weak = Self::weak(self);
                    let path = path.clone();
                    preview.connect_clicked(move |_| {
                        if let Some(this) = weak.upgrade() {
                            this.preview_file(&path, kind);
                        }
                    });
                }
                MediaSource::RemoteUrl { .. } => {
                    let icon = gtk::Image::from_icon_name(media_kind_icon(kind));
                    icon.set_pixel_size(32);
                    preview.set_child(Some(&icon));
                    preview.set_sensitive(false);
                    preview.set_tooltip_text(Some("Remote media is previewed by the provider"));
                }
            }
            body.append(&preview);

            let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
            labels.set_hexpand(true);
            let name = gtk::Label::new(Some(&media_name(&item.source)));
            name.set_halign(gtk::Align::Start);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            name.add_css_class("heading");
            let type_label = gtk::Label::new(Some(&typed_ordinal));
            type_label.set_halign(gtk::Align::Start);
            type_label.add_css_class("harness-muted");
            let source_text = match &item.source {
                MediaSource::LocalFile { path } => path.to_string_lossy().into_owned(),
                MediaSource::RemoteUrl { url } => url.clone(),
            };
            let source = gtk::Label::new(Some(&source_text));
            source.set_halign(gtk::Align::Start);
            source.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            source.add_css_class("harness-muted");
            labels.append(&name);
            labels.append(&type_label);
            labels.append(&source);
            if self.selected_provider() == ProviderId::openrouter()
                && matches!(item.source, MediaSource::LocalFile { .. })
            {
                let blocked = gtk::Label::new(Some(
                    "Blocked for OpenRouter — retained locally; switch provider or use a public HTTPS URL",
                ));
                blocked.set_halign(gtk::Align::Start);
                blocked.set_wrap(true);
                blocked.add_css_class("harness-warning");
                labels.append(&blocked);
            }
            body.append(&labels);

            if kind == MediaKind::Image {
                let role = dropdown(&["Reference", "Start frame", "End frame"]);
                role.set_selected(index_for_role(item.role));
                role.set_tooltip_text(Some("Image role in the generated video"));
                let weak = Self::weak(self);
                role.connect_selected_notify(move |dropdown| {
                    let Some(this) = weak.upgrade() else { return };
                    if let Some(item) = this.media.borrow_mut().get_mut(index) {
                        item.role = role_for_index(dropdown.selected());
                    }
                    this.draft_changed();
                });
                body.append(&role);
            } else {
                let role = gtk::Label::new(Some(media_role_label(item.role)));
                role.set_tooltip_text(Some("Video and audio references use a fixed input role"));
                role.add_css_class("harness-muted");
                body.append(&role);
            }

            let up = gtk::Button::from_icon_name("go-up-symbolic");
            up.set_tooltip_text(Some("Move earlier"));
            up.set_sensitive(index > 0);
            let weak = Self::weak(self);
            up.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                if index > 0 && index < this.media.borrow().len() {
                    this.media.borrow_mut().swap(index, index - 1);
                    this.rebuild_media();
                    this.draft_changed();
                }
            });
            body.append(&up);
            let down = gtk::Button::from_icon_name("go-down-symbolic");
            down.set_tooltip_text(Some("Move later"));
            down.set_sensitive(index + 1 < self.media.borrow().len());
            let weak = Self::weak(self);
            down.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                if index + 1 < this.media.borrow().len() {
                    this.media.borrow_mut().swap(index, index + 1);
                    this.rebuild_media();
                    this.draft_changed();
                }
            });
            body.append(&down);
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.set_tooltip_text(Some(&format!("Remove {typed_ordinal}")));
            remove.add_css_class("flat");
            let weak = Self::weak(self);
            remove.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                if index < this.media.borrow().len() {
                    this.media.borrow_mut().remove(index);
                    this.rebuild_media();
                    this.draft_changed();
                }
            });
            body.append(&remove);
            row.set_child(Some(&body));
            self.media_list.append(&row);
        }
    }

    fn preview_file(&self, path: &Path, kind: MediaKind) {
        let close = gtk::Button::with_label("Close");
        close.add_css_class("pill");
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.set_margin_top(16);
        body.set_margin_bottom(16);
        body.set_margin_start(16);
        body.set_margin_end(16);
        match kind {
            MediaKind::Image => {
                let preview = gtk::Picture::for_file(&gio::File::for_path(path));
                preview.set_can_shrink(true);
                preview.set_content_fit(gtk::ContentFit::Contain);
                body.append(&preview);
            }
            MediaKind::Video | MediaKind::Audio => {
                let preview = gtk::Video::new();
                preview.set_file(Some(&gio::File::for_path(path)));
                preview.set_autoplay(false);
                preview.set_loop(false);
                preview.set_hexpand(true);
                preview.set_height_request(if kind == MediaKind::Video { 460 } else { 120 });
                body.append(&preview);
            }
        }
        close.set_halign(gtk::Align::End);
        body.append(&close);
        let window = gtk::Window::builder()
            .title(format!(
                "{} reference — {}",
                media_kind_label(kind),
                media_name(&MediaSource::local(path))
            ))
            .transient_for(&self.window)
            .modal(true)
            .default_width(820)
            .default_height(620)
            .child(&body)
            .build();
        let weak_window = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.close();
            }
        });
        window.present();
    }

    fn refresh_models(self: &Rc<Self>) {
        let provider = self.selected_provider();
        let same_provider = self.model_provider.borrow().as_ref() == Some(&provider);
        let previous_model = same_provider.then(|| self.selected_model_id()).flatten();
        let previous_options = previous_model
            .as_ref()
            .map(|_| self.snapshot_model_options());
        let was_loading = self.loading_draft.replace(true);
        let catalogs = self.catalogs.borrow();
        let Some(catalog) = catalogs.get(&provider) else {
            self.model_ids.borrow_mut().clear();
            self.model_provider.replace(Some(provider));
            self.model
                .set_model(Some(&gtk::StringList::new(&["Loading models…"])));
            self.model.set_sensitive(false);
            self.model_description
                .set_text("Loading the current provider catalog…");
            self.loading_draft.set(was_loading);
            return;
        };
        let labels = catalog
            .models
            .iter()
            .map(|model| model.name.clone())
            .collect::<Vec<_>>();
        let refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
        let model_ids = catalog
            .models
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        let retained = previous_model
            .as_ref()
            .and_then(|previous| model_ids.iter().position(|model| model == previous));
        let selected = retained
            .or_else(|| {
                catalog
                    .preferred()
                    .and_then(|model| catalog.models.iter().position(|item| item.id == model.id))
            })
            .unwrap_or(0);
        *self.model_ids.borrow_mut() = model_ids;
        self.model_provider.replace(Some(provider));
        self.model.set_model(Some(&gtk::StringList::new(&refs)));
        self.model.set_sensitive(!labels.is_empty());
        self.model.set_selected(selected as u32);
        drop(catalogs);
        self.refresh_model_controls();
        if retained.is_some()
            && let Some(options) = previous_options
        {
            self.restore_model_options(options);
        }
        self.loading_draft.set(was_loading);
    }

    fn refresh_model_controls(self: &Rc<Self>) {
        let was_loading = self.loading_draft.replace(true);
        let Some(model) = self.selected_model() else {
            self.model_description
                .set_text("No compatible video models were returned.");
            self.loading_draft.set(was_loading);
            return;
        };
        let mut description = model.description.trim().to_owned();
        if description.is_empty() {
            description = model.id.clone();
        }
        if !model.pricing_skus.is_empty() {
            description.push_str(" • pricing available at Review");
        }
        if let Some(modalities) = &model.input_modalities {
            let accepted = modalities
                .iter()
                .map(|kind| media_kind_plural(*kind))
                .collect::<Vec<_>>()
                .join(", ");
            description.push_str(if accepted.is_empty() {
                " • no reference-media inputs advertised"
            } else {
                " • accepts "
            });
            if !accepted.is_empty() {
                description.push_str(&accepted);
            }
        } else {
            description.push_str(" • media capabilities not advertised");
        }
        if !model.media_bindings.is_empty() {
            let bindings = model
                .media_bindings
                .iter()
                .map(|binding| {
                    let purpose = binding
                        .title
                        .as_deref()
                        .or(binding.description.as_deref())
                        .unwrap_or(&binding.property_name);
                    format!("{} — {purpose}", media_kind_label(binding.kind))
                })
                .collect::<Vec<_>>()
                .join("; ");
            description.push_str(" • media fields: ");
            description.push_str(&bindings);
        }
        if self
            .catalogs
            .borrow()
            .get(&model.provider_id)
            .is_some_and(|catalog| catalog.stale)
        {
            description.push_str(" • cached catalog");
        }
        self.model_description.set_text(&description);

        let durations = std::iter::once(None)
            .chain(model.supported_durations.iter().copied().map(Some))
            .collect::<Vec<_>>();
        let duration_labels = durations
            .iter()
            .map(|value| {
                value.map_or_else(
                    || "Provider default".into(),
                    |value| format!("{value} seconds"),
                )
            })
            .collect::<Vec<String>>();
        set_dropdown_strings(&self.options.duration, &duration_labels);
        *self.options.durations.borrow_mut() = durations;

        set_optional_strings(
            &self.options.resolution,
            &self.options.resolutions,
            &model.supported_resolutions,
        );
        set_optional_strings(
            &self.options.aspect,
            &self.options.aspects,
            &model.supported_aspect_ratios,
        );
        set_optional_strings(
            &self.options.size,
            &self.options.sizes,
            &model.supported_sizes,
        );
        self.options
            .audio
            .set_sensitive(model.generate_audio == Some(true));
        if model.generate_audio != Some(true) {
            self.options.audio.set_active(false);
        }
        self.options.seed.set_sensitive(model.seed == Some(true));
        if model.seed != Some(true) {
            self.options.seed.set_text("");
        }
        self.refresh_schema_controls(&model);
        self.loading_draft.set(was_loading);
    }

    fn snapshot_model_options(&self) -> ModelOptionsSnapshot {
        let schema = self
            .options
            .schema_controls
            .borrow()
            .iter()
            .map(|(name, control)| {
                let value = match control {
                    SchemaControl::Choice { widget, values } => SchemaUiValue::Choice(
                        values.get(widget.selected() as usize).cloned().flatten(),
                    ),
                    SchemaControl::Text { widget, .. } => {
                        SchemaUiValue::Text(widget.text().to_string())
                    }
                };
                (name.clone(), value)
            })
            .collect();
        ModelOptionsSnapshot {
            duration: selected_copy(&self.options.durations, self.options.duration.selected()),
            resolution: selected_clone(
                &self.options.resolutions,
                self.options.resolution.selected(),
            ),
            aspect_ratio: selected_clone(&self.options.aspects, self.options.aspect.selected()),
            size: selected_clone(&self.options.sizes, self.options.size.selected()),
            audio: self.options.audio.is_active(),
            seed: self.options.seed.text().to_string(),
            schema,
        }
    }

    fn restore_model_options(&self, snapshot: ModelOptionsSnapshot) {
        set_selected_copy(
            &self.options.duration,
            &self.options.durations,
            snapshot.duration,
        );
        set_selected_clone(
            &self.options.resolution,
            &self.options.resolutions,
            snapshot.resolution,
        );
        set_selected_clone(
            &self.options.aspect,
            &self.options.aspects,
            snapshot.aspect_ratio,
        );
        set_selected_clone(&self.options.size, &self.options.sizes, snapshot.size);
        if self.options.audio.is_sensitive() {
            self.options.audio.set_active(snapshot.audio);
        }
        if self.options.seed.is_sensitive() {
            self.options.seed.set_text(&snapshot.seed);
        }
        for (name, previous) in snapshot.schema {
            let controls = self.options.schema_controls.borrow();
            let Some(control) = controls.get(&name) else {
                continue;
            };
            match (control, previous) {
                (SchemaControl::Choice { widget, values }, SchemaUiValue::Choice(value)) => {
                    if let Some(index) = values.iter().position(|candidate| candidate == &value) {
                        widget.set_selected(index as u32);
                    }
                }
                (SchemaControl::Text { widget, .. }, SchemaUiValue::Text(value)) => {
                    widget.set_text(&value);
                }
                _ => {}
            }
        }
    }

    fn refresh_schema_controls(self: &Rc<Self>, model: &VideoModel) {
        while let Some(child) = self.options.schema_box.first_child() {
            self.options.schema_box.remove(&child);
        }
        self.options.schema_controls.borrow_mut().clear();
        let Some(properties) = model
            .input_schema
            .as_ref()
            .and_then(|schema| schema.get("properties"))
            .and_then(serde_json::Value::as_object)
        else {
            let label = gtk::Label::new(Some(
                "This catalog does not publish an input schema. Use advanced JSON for provider-specific fields.",
            ));
            label.set_halign(gtk::Align::Start);
            label.set_wrap(true);
            label.add_css_class("harness-muted");
            self.options.schema_box.append(&label);
            return;
        };
        let mut mapped_common = model.field_map.values().cloned().collect::<HashSet<_>>();
        mapped_common.extend(
            model
                .media_bindings
                .iter()
                .map(|binding| binding.property_name.clone()),
        );
        let known = [
            "prompt",
            "duration",
            "resolution",
            "aspect_ratio",
            "size",
            "generate_audio",
            "seed",
            "image_url",
            "start_image_url",
            "end_image_url",
            "input_references",
            "frame_images",
            "video_url",
            "audio_url",
        ];
        let mut rendered = 0usize;
        let mut unsupported = Vec::new();
        for (name, schema) in properties {
            if mapped_common.contains(name) || known.contains(&name.as_str()) {
                continue;
            }
            let label_text = schema
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(name)
                .replace('_', " ");
            let description = schema
                .get("description")
                .and_then(serde_json::Value::as_str);
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let label = field_label(&label_text);
            label.set_width_chars(18);
            if let Some(description) = description {
                label.set_tooltip_text(Some(description));
            }
            row.append(&label);

            let control =
                if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array) {
                    let mut choices = vec![None];
                    choices.extend(values.iter().cloned().map(Some));
                    let labels = choices
                        .iter()
                        .map(|value| match value {
                            None => "Provider default".into(),
                            Some(serde_json::Value::String(value)) => value.clone(),
                            Some(value) => value.to_string(),
                        })
                        .collect::<Vec<String>>();
                    let widget = gtk::DropDown::new(
                        Some(gtk::StringList::new(
                            &labels.iter().map(String::as_str).collect::<Vec<_>>(),
                        )),
                        None::<gtk::Expression>,
                    );
                    widget.set_hexpand(true);
                    if let Some(default) = schema.get("default")
                        && let Some(index) = choices
                            .iter()
                            .position(|value| value.as_ref() == Some(default))
                    {
                        widget.set_selected(index as u32);
                    }
                    let weak = Self::weak(self);
                    widget.connect_selected_notify(move |_| {
                        if let Some(this) = weak.upgrade() {
                            this.draft_changed();
                        }
                    });
                    row.append(&widget);
                    SchemaControl::Choice {
                        widget,
                        values: choices,
                    }
                } else {
                    let kind_name = schema_type(schema);
                    match kind_name {
                        Some("boolean") => {
                            let widget = dropdown(&["Provider default", "True", "False"]);
                            widget.set_hexpand(true);
                            if let Some(default) =
                                schema.get("default").and_then(serde_json::Value::as_bool)
                            {
                                widget.set_selected(if default { 1 } else { 2 });
                            }
                            let weak = Self::weak(self);
                            widget.connect_selected_notify(move |_| {
                                if let Some(this) = weak.upgrade() {
                                    this.draft_changed();
                                }
                            });
                            row.append(&widget);
                            SchemaControl::Choice {
                                widget,
                                values: vec![
                                    None,
                                    Some(serde_json::Value::Bool(true)),
                                    Some(serde_json::Value::Bool(false)),
                                ],
                            }
                        }
                        Some("string") | Some("integer") | Some("number") => {
                            let kind = match kind_name {
                                Some("integer") => SchemaTextKind::Integer,
                                Some("number") => SchemaTextKind::Number,
                                _ => SchemaTextKind::String,
                            };
                            let widget = gtk::Entry::builder()
                                .hexpand(true)
                                .placeholder_text("Provider default")
                                .build();
                            if let Some(default) = schema.get("default") {
                                widget.set_text(
                                    default
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| default.to_string())
                                        .as_str(),
                                );
                            }
                            let weak = Self::weak(self);
                            widget.connect_changed(move |_| {
                                if let Some(this) = weak.upgrade() {
                                    this.draft_changed();
                                }
                            });
                            row.append(&widget);
                            SchemaControl::Text { widget, kind }
                        }
                        _ => {
                            unsupported.push(name.clone());
                            continue;
                        }
                    }
                };
            self.options.schema_box.append(&row);
            self.options
                .schema_controls
                .borrow_mut()
                .insert(name.clone(), control);
            rendered += 1;
        }
        if rendered == 0 {
            let label = gtk::Label::new(Some(
                "Common controls above cover this model's published schema.",
            ));
            label.set_halign(gtk::Align::Start);
            label.add_css_class("harness-muted");
            self.options.schema_box.append(&label);
        }
        if !unsupported.is_empty() {
            let label = gtk::Label::new(Some(&format!(
                "Nested schema fields use the JSON fallback: {}",
                unsupported.join(", ")
            )));
            label.set_halign(gtk::Align::Start);
            label.set_wrap(true);
            label.add_css_class("harness-muted");
            self.options.schema_box.append(&label);
        }
    }

    fn apply_draft(
        self: &Rc<Self>,
        draft: GenerationDraft,
        editor_state: DraftEditorState,
        revision: u64,
    ) {
        if !self.catalogs.borrow().contains_key(&draft.provider_id) {
            self.pending_draft
                .replace(Some((draft, editor_state, revision)));
            return;
        }
        self.loading_draft.set(true);
        self.provider
            .set_selected(index_for_provider(&draft.provider_id));
        self.refresh_models();
        if let Some(index) = self
            .model_ids
            .borrow()
            .iter()
            .position(|model| model == &draft.model)
        {
            self.model.set_selected(index as u32);
            self.refresh_model_controls();
        }
        self.prompt.buffer().set_text(&draft.prompt);
        set_selected_copy(
            &self.options.duration,
            &self.options.durations,
            draft.duration,
        );
        set_selected_clone(
            &self.options.resolution,
            &self.options.resolutions,
            draft.resolution,
        );
        set_selected_clone(
            &self.options.aspect,
            &self.options.aspects,
            draft.aspect_ratio,
        );
        set_selected_clone(&self.options.size, &self.options.sizes, draft.size);
        self.options
            .audio
            .set_active(self.options.audio.is_sensitive() && draft.generate_audio.unwrap_or(false));
        self.options.seed.set_text(
            &draft
                .seed
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        let advanced = draft
            .adapter_options
            .as_ref()
            .and_then(|value| serde_json::to_string_pretty(value).ok())
            .unwrap_or_else(|| "{}".into());
        self.options.advanced.buffer().set_text(&advanced);
        if let Some(adapter) = draft
            .adapter_options
            .as_ref()
            .and_then(serde_json::Value::as_object)
        {
            for (name, control) in self.options.schema_controls.borrow().iter() {
                if let Some(value) = adapter.get(name) {
                    set_schema_control(control, value);
                }
            }
        }
        self.options.seed.set_text(&editor_state.seed_text);
        self.options
            .advanced
            .buffer()
            .set_text(&editor_state.advanced_json_text);
        for (name, value) in editor_state.schema_text {
            if let Some(SchemaControl::Text { widget, .. }) =
                self.options.schema_controls.borrow().get(&name)
            {
                widget.set_text(&value);
            }
        }
        *self.media.borrow_mut() = draft
            .media
            .into_iter()
            .map(|media| MediaItem {
                source: media.source,
                role: media.role,
            })
            .collect();
        self.update_media_input_availability();
        self.rebuild_media();
        self.revision.set(revision);
        self.loading_draft.set(false);
        self.update_compatibility();
        self.toast("Restored your autosaved draft.", "document-revert-symbolic");
    }

    fn start_event_pump(self: &Rc<Self>) {
        let weak = Self::weak(self);
        glib::timeout_add_local(Duration::from_millis(90), move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            loop {
                match this.events.borrow_mut().try_recv() {
                    Ok(event) => this.handle_event(event),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        this.handle_service_disconnected();
                        return glib::ControlFlow::Break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    fn start_animation(self: &Rc<Self>) {
        let weak = Self::weak(self);
        glib::timeout_add_local(Duration::from_millis(420), move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let index = (this.animation_frame.get() + 1) % ANIMATION_FRAMES.len();
            this.animation_frame.set(index);
            for widgets in this.jobs.borrow().values() {
                if widgets.active.get() {
                    widgets.animation.set_text(ANIMATION_FRAMES[index]);
                    widgets.progress.pulse();
                }
            }
            glib::ControlFlow::Continue
        });
    }

    fn handle_event(self: &Rc<Self>, event: ServiceEvent) {
        match event {
            ServiceEvent::Ready {
                providers,
                default_provider,
            } => {
                self.update_connections(&providers);
                let index = index_for_provider(&default_provider);
                self.loading_draft.set(true);
                self.provider.set_selected(index);
                self.default_provider.set_selected(index);
                self.loading_draft.set(false);
                self.update_media_input_availability();
                self.rebuild_media();
                self.send(ServiceCommand::LoadDraft {
                    op_id: self.op_id(),
                });
                self.send(ServiceCommand::LoadHistory {
                    op_id: self.op_id(),
                    limit: 200,
                });
                for provider_id in [ProviderId::openrouter(), ProviderId::fal()] {
                    self.send(ServiceCommand::RefreshCatalog {
                        op_id: self.op_id(),
                        provider_id,
                    });
                }
            }
            ServiceEvent::ApiKeyConnected {
                provider_id,
                credential_status,
                ..
            } => {
                if let Some(widgets) = self.provider_widgets.get(&provider_id) {
                    widgets.status.set_text("Connected");
                    widgets.status.remove_css_class("harness-muted");
                    widgets.status.add_css_class("harness-good");
                    widgets.storage.set_text(&credential_status.message);
                    widgets.forget.set_sensitive(true);
                }
                self.toast("Provider connected.", "emblem-ok-symbolic");
            }
            ServiceEvent::ApiKeyForgotten {
                provider_id,
                credential_status,
                ..
            } => {
                if let Some(widgets) = self.provider_widgets.get(&provider_id) {
                    widgets.status.set_text("Needs API key");
                    widgets.status.remove_css_class("harness-good");
                    widgets.status.add_css_class("harness-muted");
                    widgets.storage.set_text(&credential_status.message);
                    widgets.forget.set_sensitive(false);
                }
            }
            ServiceEvent::CatalogLoaded {
                provider_id,
                catalog,
                ..
            } => {
                self.catalogs
                    .borrow_mut()
                    .insert(provider_id.clone(), catalog);
                if provider_id == self.selected_provider() {
                    self.refresh_models();
                    self.update_compatibility();
                }
                let pending = self.pending_draft.borrow().as_ref().cloned();
                if pending
                    .as_ref()
                    .is_some_and(|(draft, _, _)| draft.provider_id == provider_id)
                {
                    let (draft, editor_state, revision) =
                        self.pending_draft.borrow_mut().take().expect("checked");
                    self.apply_draft(draft, editor_state, revision);
                }
            }
            ServiceEvent::PreparationStarted { media_count, .. } => {
                self.show_compatibility(
                    &format!(
                        "Preparing review and checking {media_count} reference-media item(s)…"
                    ),
                    "harness-warning",
                );
            }
            ServiceEvent::MediaUploadStarted {
                media_index, path, ..
            } => {
                self.show_compatibility(
                    &format!(
                        "Uploading {}: {} (generation has not been submitted)",
                        typed_media_ordinal(&self.media.borrow(), media_index),
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("media")
                    ),
                    "harness-warning",
                );
            }
            ServiceEvent::MediaUploadProgress {
                media_index,
                sent,
                total,
                ..
            } => {
                let percent = sent.saturating_mul(100).checked_div(total).unwrap_or(0);
                self.show_compatibility(
                    &format!(
                        "Uploading {}… {percent}%",
                        typed_media_ordinal(&self.media.borrow(), media_index)
                    ),
                    "harness-warning",
                );
            }
            ServiceEvent::MediaUploadCompleted {
                media_index,
                reused,
                expires_at,
                ..
            } => {
                let source = if reused {
                    "reused secure upload"
                } else {
                    "upload complete"
                };
                let expiry = expires_at
                    .map(|value| format!("; expires {}", value.format("%Y-%m-%d %H:%M UTC")))
                    .unwrap_or_default();
                self.show_compatibility(
                    &format!(
                        "{}: {source}{expiry}. Fetching a fresh quote…",
                        typed_media_ordinal(&self.media.borrow(), media_index)
                    ),
                    "harness-warning",
                );
            }
            ServiceEvent::ReviewReady {
                prepared_id,
                revision,
                provider_id,
                request,
                quote,
                expires_at,
                draft_fingerprint,
                ..
            } => {
                self.review.set_label("Review generation");
                if revision != self.revision.get() {
                    self.update_compatibility();
                    self.toast(
                        "The draft changed, so that review was discarded.",
                        "dialog-warning-symbolic",
                    );
                    return;
                }
                self.prepared.replace(Some(PreparedReview {
                    id: prepared_id,
                    revision,
                    draft_fingerprint,
                }));
                self.review.set_sensitive(true);
                self.show_review(provider_id, request, quote, expires_at);
            }
            ServiceEvent::PreparedInvalidated { revision, .. } => {
                if revision >= self.revision.get() {
                    self.prepared.borrow_mut().take();
                }
                self.review.set_label("Review generation");
                self.update_compatibility();
            }
            ServiceEvent::DraftLoaded {
                draft: Some(draft),
                editor_state,
                revision,
                ..
            } => {
                self.apply_draft(
                    draft,
                    editor_state.unwrap_or_default(),
                    revision.unwrap_or(0),
                );
            }
            ServiceEvent::DraftLoaded { draft: None, .. } => {
                self.update_compatibility();
            }
            ServiceEvent::UncertainSubmissionSaved { record, .. } => {
                self.uncertain_submissions.borrow_mut().insert(
                    (record.provider_id.clone(), record.draft_fingerprint.clone()),
                    record,
                );
                self.update_compatibility();
            }
            ServiceEvent::UncertainSubmissionCleared {
                op_id,
                provider_id,
                draft_fingerprint,
                ..
            } => {
                let explicitly_cleared = self
                    .pending_uncertain_clears
                    .borrow_mut()
                    .remove(&op_id)
                    .is_some();
                self.uncertain_submissions
                    .borrow_mut()
                    .remove(&(provider_id, draft_fingerprint));
                self.update_compatibility();
                if explicitly_cleared {
                    self.toast(
                        "The safety hold was cleared. Review again to obtain a fresh quote before submitting.",
                        "emblem-ok-symbolic",
                    );
                }
            }
            ServiceEvent::UncertainSubmissionsLoaded { records, .. } => {
                let mut submissions = self.uncertain_submissions.borrow_mut();
                submissions.clear();
                for record in records {
                    submissions.insert(
                        (record.provider_id.clone(), record.draft_fingerprint.clone()),
                        record,
                    );
                }
                drop(submissions);
                if self.current_uncertain_submission().is_some() {
                    self.prepared.borrow_mut().take();
                }
                self.update_compatibility();
            }
            ServiceEvent::UncertainSubmissionBlocked { op_id, record } => {
                self.clear_pending_submission(op_id);
                self.prepared.borrow_mut().take();
                self.uncertain_submissions.borrow_mut().insert(
                    (record.provider_id.clone(), record.draft_fingerprint.clone()),
                    record.clone(),
                );
                self.update_compatibility();
                self.show_submission_uncertain(&record.provider_id, &record.message);
            }
            ServiceEvent::SubmissionStarted { op_id, .. } => {
                if self.pending_submit_op.get() == Some(op_id) {
                    self.review.set_sensitive(false);
                    self.show_compatibility(
                        "Submitting one paid generation request. Keep this window open until a job ID appears…",
                        "harness-warning",
                    );
                }
            }
            ServiceEvent::JobAccepted {
                op_id: _,
                provider_id,
                job,
                record,
            } => {
                let key = job.key();
                let title = record
                    .as_ref()
                    .and_then(job_title)
                    .unwrap_or_else(|| format!("{} job", provider_name(&provider_id)));
                let widgets = self.ensure_job(key.clone(), &title);
                widgets.status.set_text(job.status.as_str());
                widgets
                    .detail
                    .set_text("Accepted. Saving the recovery record locally…");
                widgets.active.set(!job.terminal());
                widgets.pause.set_sensitive(!job.terminal());
                widgets.resume.set_sensitive(false);
                if job.terminal() {
                    self.active_jobs.borrow_mut().remove(&key);
                } else {
                    self.active_jobs.borrow_mut().insert(key);
                }
                self.view_stack.set_visible_child_name("jobs");
                self.toast(
                    "The provider accepted the generation. Saving its recovery record…",
                    "document-save-symbolic",
                );
            }
            ServiceEvent::JobRecoverySaved {
                op_id, key, store, ..
            } => {
                self.clear_pending_submission(op_id);
                if let Some(widgets) = self.jobs.borrow().get(&key) {
                    widgets
                        .detail
                        .set_text("Recovery record saved locally. Monitoring provider status…");
                }
                self.update_compatibility();
                let location = match store {
                    RecoveryStore::GuiState => "GUI recovery state",
                    RecoveryStore::History => "compatible history",
                };
                self.toast(
                    &format!("Remote job ID saved in {location}."),
                    "emblem-ok-symbolic",
                );
            }
            ServiceEvent::JobRecoveryWarning { key, message, .. } => {
                if let Some(widgets) = self.jobs.borrow().get(&key) {
                    widgets.detail.set_text(&format!(
                        "The job is recoverable, but some optional metadata was not saved. {message}"
                    ));
                }
                self.toast(&message, "dialog-warning-symbolic");
            }
            ServiceEvent::JobRecoveryFailed {
                op_id,
                key,
                message,
                ..
            } => {
                self.clear_pending_submission(op_id);
                if let Some(widgets) = self.jobs.borrow().get(&key) {
                    widgets.status.set_text("recovery failed");
                    widgets.detail.set_text(
                        "The provider accepted this job, but its recovery record could not be saved. Copy the remote ID now.",
                    );
                    widgets.active.set(false);
                    widgets.pause.set_sensitive(false);
                    widgets.resume.set_sensitive(false);
                }
                self.active_jobs.borrow_mut().insert(key.clone());
                self.update_compatibility();
                self.show_remote_job_warning("Remote job accepted, but not saved", &key, &message);
            }
            ServiceEvent::SubmissionUncertain {
                op_id,
                provider_id,
                message,
                draft_fingerprint,
            } => {
                if let Some((submitted_revision, _)) = self.clear_pending_submission(op_id)
                    && draft_fingerprint.is_none()
                {
                    self.uncertain_revision.set(Some(submitted_revision));
                }
                if let Some(draft_fingerprint) = draft_fingerprint {
                    let record = UncertainSubmissionRecord::new(
                        provider_id.clone(),
                        draft_fingerprint.clone(),
                        chrono::Utc::now(),
                    );
                    self.uncertain_submissions
                        .borrow_mut()
                        .entry((provider_id.clone(), draft_fingerprint))
                        .or_insert(record);
                }
                self.prepared.borrow_mut().take();
                self.update_compatibility();
                self.show_submission_uncertain(&provider_id, &message);
            }
            ServiceEvent::JobUpdated { job, record, .. } => {
                let widgets = self.ensure_job(
                    job.key(),
                    &job_title(&record).unwrap_or_else(|| "Video generation".into()),
                );
                widgets.status.set_text(job.status.as_str());
                widgets.detail.set_text(match job.status.as_str() {
                    "pending" => "Queued by the provider",
                    "in_progress" => "The provider is generating your video",
                    "completed" => "Generation complete; preparing the download",
                    "failed" => job
                        .error
                        .as_deref()
                        .unwrap_or("The provider reported a failure"),
                    other => other,
                });
                let active = !job.terminal();
                widgets.active.set(active);
                widgets.pause.set_sensitive(active);
                if active {
                    self.active_jobs.borrow_mut().insert(job.key());
                } else {
                    self.active_jobs.borrow_mut().remove(&job.key());
                    widgets
                        .animation
                        .set_text(if job.successful() { "✓" } else { "!" });
                    widgets
                        .progress
                        .set_fraction(if job.successful() { 1.0 } else { 0.0 });
                }
            }
            ServiceEvent::PollWaiting {
                provider_id,
                job_id,
                attempt,
                next_in,
                ..
            } => {
                let key = ProviderJobKey {
                    provider_id,
                    remote_job_id: job_id,
                };
                let widgets = self.ensure_job(key.clone(), "Video generation");
                widgets.status.set_text("monitoring");
                widgets.detail.set_text(&format!(
                    "Provider check {attempt} complete; checking again in {} seconds",
                    next_in.as_secs()
                ));
                widgets.active.set(true);
                self.active_jobs.borrow_mut().insert(key);
            }
            ServiceEvent::DownloadProgress {
                provider_id,
                job_id,
                written,
                total,
                ..
            } => {
                let key = ProviderJobKey {
                    provider_id,
                    remote_job_id: job_id,
                };
                let widgets = self.ensure_job(key.clone(), "Video generation");
                widgets.status.set_text("downloading");
                widgets.detail.set_text(&match total {
                    Some(total) => format!(
                        "Saving video… {} / {}",
                        byte_size(written),
                        byte_size(total)
                    ),
                    None => format!("Saving video… {}", byte_size(written)),
                });
                if let Some(total) = total.filter(|value| *value > 0) {
                    widgets.progress.set_fraction(written as f64 / total as f64);
                }
                widgets.active.set(true);
                self.active_jobs.borrow_mut().insert(key);
            }
            ServiceEvent::Downloaded {
                job, record, path, ..
            } => {
                let widgets = self.ensure_job(
                    job.key(),
                    &job_title(&record).unwrap_or_else(|| "Completed video".into()),
                );
                widgets.status.set_text("saved");
                widgets
                    .detail
                    .set_text(&format!("Saved to {}", path.display()));
                widgets.progress.set_fraction(1.0);
                widgets.animation.set_text("✓");
                widgets.active.set(false);
                widgets.pause.set_sensitive(false);
                widgets.resume.set_sensitive(false);
                widgets.open.set_sensitive(true);
                widgets.video.set_file(Some(&gio::File::for_path(&path)));
                widgets.video.set_visible(true);
                widgets.local_path.replace(Some(path));
                self.latest_video
                    .replace(widgets.local_path.borrow().clone());
                self.active_jobs.borrow_mut().remove(&job.key());
                self.toast(
                    "Your video is ready in the Videos folder.",
                    "folder-videos-symbolic",
                );
            }
            ServiceEvent::HistoryLoaded { records, .. } => {
                for record in records {
                    self.restore_record(record);
                }
            }
            ServiceEvent::MonitorPaused {
                key,
                remote_continues,
                ..
            } => {
                if let Some(widgets) = self.jobs.borrow().get(&key) {
                    widgets.active.set(false);
                    widgets.pause.set_sensitive(false);
                    widgets.resume.set_sensitive(true);
                    widgets.status.set_text("monitoring paused");
                    widgets.animation.set_text("Ⅱ");
                    widgets.detail.set_text(if remote_continues {
                        "Local checks paused; the provider continues remotely"
                    } else {
                        "Monitoring paused"
                    });
                }
                self.active_jobs.borrow_mut().remove(&key);
            }
            ServiceEvent::MonitorsPaused {
                count,
                remote_continue,
                ..
            } => {
                for widgets in self.jobs.borrow().values() {
                    if widgets.active.replace(false) {
                        widgets.pause.set_sensitive(false);
                        widgets.resume.set_sensitive(true);
                        widgets.status.set_text("monitoring paused");
                        widgets.animation.set_text("Ⅱ");
                    }
                }
                self.active_jobs.borrow_mut().clear();
                let message = if remote_continue {
                    format!("Paused {count} monitor(s). Remote jobs continue.")
                } else {
                    format!("Paused {count} monitor(s).")
                };
                self.toast(&message, "media-playback-pause-symbolic");
                if self.pause_before_shutdown.replace(false) {
                    self.request_shutdown();
                }
            }
            ServiceEvent::ResumeAllStarted {
                started, skipped, ..
            } => {
                self.toast(
                    &format!("Resumed {started} job(s); skipped {skipped}."),
                    "media-playback-start-symbolic",
                );
            }
            ServiceEvent::ResumableJobsLoaded { jobs, .. } => {
                for job in jobs {
                    let widgets = self.ensure_job(job.key.clone(), "Saved remote job");
                    widgets.status.set_text(if job.monitoring_paused {
                        "monitoring paused"
                    } else {
                        "remote job"
                    });
                    widgets
                        .detail
                        .set_text("Resume to check the provider and download completed output");
                    widgets.resume.set_sensitive(true);
                }
            }
            ServiceEvent::Cancelled {
                provider_id,
                job_id,
                remote_continues,
                ..
            } => {
                if let (Some(provider_id), Some(job_id)) = (provider_id, job_id) {
                    let key = ProviderJobKey {
                        provider_id,
                        remote_job_id: job_id,
                    };
                    if let Some(widgets) = self.jobs.borrow().get(&key) {
                        widgets.active.set(false);
                        widgets.status.set_text("monitoring stopped");
                        widgets.detail.set_text(if remote_continues {
                            "Local monitoring stopped; the provider job continues"
                        } else {
                            "Stopped"
                        });
                    }
                    self.active_jobs.borrow_mut().remove(&key);
                }
            }
            ServiceEvent::DraftSaved {
                op_id, revision, ..
            } => {
                if self.pending_close_draft_op.get() == Some(op_id) {
                    self.pending_close_draft_op.set(None);
                    if revision == self.revision.get() {
                        self.request_service_shutdown();
                    } else {
                        // The user edited while the disk write was in flight;
                        // flush the newer revision before closing.
                        self.request_shutdown();
                    }
                }
            }
            ServiceEvent::Error {
                op_id,
                provider_id,
                message,
                recoverable,
                job_id,
                ..
            } => {
                self.pending_uncertain_clears.borrow_mut().remove(&op_id);
                let closing_save_failed = self.pending_close_draft_op.get() == Some(op_id);
                if closing_save_failed {
                    self.pending_close_draft_op.set(None);
                    self.show_draft_save_failure(&message);
                }
                self.clear_pending_submission(op_id);
                self.review.set_label("Review generation");
                self.update_compatibility();
                if let Some(job_id) = job_id {
                    for (key, widgets) in self.jobs.borrow().iter() {
                        if key.remote_job_id == job_id
                            && provider_id
                                .as_ref()
                                .is_none_or(|provider| provider == &key.provider_id)
                        {
                            widgets.status.set_text("needs attention");
                            widgets.detail.set_text(&message);
                            widgets.active.set(false);
                            widgets.pause.set_sensitive(false);
                            widgets.resume.set_sensitive(recoverable);
                            if recoverable {
                                // The remote job may still be running; retain the close warning.
                                self.active_jobs.borrow_mut().insert(key.clone());
                            }
                        }
                    }
                }
                self.toast(&message, "dialog-error-symbolic");
            }
            ServiceEvent::ShutdownBlocked { reason } => {
                self.pause_before_shutdown.set(false);
                self.shutdown_requested.set(false);
                self.update_compatibility();
                self.toast(&reason, "dialog-warning-symbolic");
            }
            ServiceEvent::ShutdownComplete => {
                self.shutdown_requested.set(false);
                // The event sender is expected to disconnect immediately
                // after this acknowledgement; that is a clean shutdown.
                self.service_disconnected.set(true);
                self.allow_close.set(true);
                self.window.close();
                self.keep_alive.borrow_mut().take();
            }
            ServiceEvent::VideoOpened { .. }
            | ServiceEvent::QuoteReady { .. }
            | ServiceEvent::SettingsSaved { .. }
            | ServiceEvent::DefaultProviderSaved { .. }
            | ServiceEvent::Imported { .. } => {}
        }
    }

    fn update_connections(&self, connections: &[ProviderConnection]) {
        for connection in connections {
            let Some(widgets) = self.provider_widgets.get(&connection.descriptor.id) else {
                continue;
            };
            widgets.status.set_text(if connection.connected {
                "Connected"
            } else {
                "Needs API key"
            });
            widgets.status.remove_css_class("harness-muted");
            widgets.status.remove_css_class("harness-good");
            widgets.status.add_css_class(if connection.connected {
                "harness-good"
            } else {
                "harness-muted"
            });
            widgets
                .storage
                .set_text(&connection.credential_status.message);
            widgets.forget.set_sensitive(connection.connected);
        }
    }

    fn show_review(
        self: &Rc<Self>,
        provider_id: ProviderId,
        request: crate::domain::VideoRequest,
        quote: CostQuote,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) {
        let dialog = adw::AlertDialog::builder()
            .heading("Review generation")
            .prefer_wide_layout(true)
            .build();
        dialog.add_response("cancel", "Keep editing");
        dialog.add_response("generate", "Generate — one paid request");
        dialog.set_close_response("cancel");
        // A stray Enter must never perform the paid action.
        dialog.set_default_response(Some("cancel"));
        dialog.set_response_appearance("generate", adw::ResponseAppearance::Suggested);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
        body.set_margin_top(18);
        body.set_margin_bottom(18);
        body.set_margin_start(18);
        body.set_margin_end(18);
        let title = gtk::Label::new(Some(
            "Everything below is ready. Nothing has been submitted yet.",
        ));
        title.set_halign(gtk::Align::Start);
        title.set_wrap(true);
        title.add_css_class("title-3");
        body.append(&title);
        body.append(&summary_row("Provider", provider_name(&provider_id)));
        body.append(&summary_row("Model", &request.model));
        body.append(&summary_row(
            "Prompt",
            &ellipsize_text(&request.prompt, 180),
        ));
        body.append(&summary_row(
            "Reference media",
            &typed_reference_counts(&request),
        ));
        let reference_details = typed_reference_details(&request);
        if !reference_details.is_empty() {
            body.append(&summary_row(
                "Typed references",
                &reference_details.join("\n"),
            ));
            body.append(&summary_row(
                "Media checks",
                "Counts, URL safety, and supported local file signatures were checked. Duration, dimensions, and remote-media contents are not locally verified; provider validation and final usage remain authoritative.",
            ));
        }
        let settings = format!(
            "Duration: {}\nResolution: {}\nAspect ratio: {}\nExact size: {}\nAudio: {}\nSeed: {}",
            request
                .duration
                .map(|value| format!("{value} seconds"))
                .unwrap_or_else(|| "provider default".into()),
            request.resolution.as_deref().unwrap_or("provider default"),
            request
                .aspect_ratio
                .as_deref()
                .unwrap_or("provider default"),
            request.size.as_deref().unwrap_or("provider default"),
            match request.generate_audio {
                Some(true) => "on",
                Some(false) => "off",
                None => "provider default",
            },
            request
                .seed
                .map(|value| value.to_string())
                .unwrap_or_else(|| "provider default".into()),
        );
        body.append(&summary_row("Settings", &settings));
        if let Some(options) = &request.adapter_options {
            let options =
                serde_json::to_string_pretty(options).unwrap_or_else(|_| options.to_string());
            body.append(&summary_row("Provider options", &options));
        }
        let price = quote
            .amount
            .map(|amount| {
                format!(
                    "{} {amount} {}",
                    if quote.exact { "Exact" } else { "Estimated" },
                    quote.currency
                )
            })
            .unwrap_or_else(|| format!("Unavailable ({})", quote.basis));
        let price_label = summary_row("Fresh price", &price);
        price_label.add_css_class("harness-price");
        body.append(&price_label);
        let expiry = gtk::Label::new(Some(&format!(
            "This review expires at {} or immediately after any edit.",
            expires_at.format("%H:%M:%S UTC")
        )));
        expiry.set_halign(gtk::Align::Start);
        expiry.set_wrap(true);
        expiry.add_css_class("harness-muted");
        body.append(&expiry);
        let paid = gtk::Label::new(Some(
            "Generate performs exactly one paid provider request. Monitoring and downloading happen afterward.",
        ));
        paid.set_halign(gtk::Align::Start);
        paid.set_wrap(true);
        paid.add_css_class("harness-warning");
        body.append(&paid);
        dialog.set_extra_child(Some(&body));

        let weak = Self::weak(self);
        dialog.connect_response(Some("generate"), move |_, _| {
            if let Some(this) = weak.upgrade() {
                let prepared = this.prepared.borrow_mut().take();
                match prepared {
                    Some(prepared) if prepared.revision == this.revision.get() => {
                        if this.close_in_progress() {
                            this.toast(
                                "Video Harness is already closing safely; no generation was submitted.",
                                "dialog-warning-symbolic",
                            );
                            return;
                        }
                        let uncertain_key = (
                            provider_id.clone(),
                            prepared.draft_fingerprint.clone(),
                        );
                        if let Some(record) = this
                            .uncertain_submissions
                            .borrow()
                            .get(&uncertain_key)
                            .cloned()
                        {
                            this.show_uncertain_resolution(record);
                            return;
                        }
                        let op_id = this.op_id();
                        this.pending_submit_op.set(Some(op_id));
                        this.pending_submit_revision.set(Some(prepared.revision));
                        this.pending_submit_provider
                            .replace(Some(provider_id.clone()));
                        this.review.set_sensitive(false);
                        if !this.send(ServiceCommand::SubmitPrepared {
                            op_id,
                            prepared_id: prepared.id,
                        }) {
                            this.clear_pending_submission(op_id);
                            this.update_compatibility();
                        }
                    }
                    _ => this.toast(
                        "That review is no longer current. Review the draft again.",
                        "dialog-warning-symbolic",
                    ),
                }
            }
        });
        dialog.present(Some(&self.window));
    }

    fn ensure_job(self: &Rc<Self>, key: ProviderJobKey, initial_title: &str) -> Rc<JobWidgets> {
        if let Some(widgets) = self.jobs.borrow().get(&key) {
            if !initial_title.trim().is_empty() {
                widgets.title.set_text(initial_title);
            }
            return Rc::clone(widgets);
        }
        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        row.set_activatable(false);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
        body.add_css_class("harness-job-card");

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = gtk::Label::new(Some(initial_title));
        title.set_hexpand(true);
        title.set_halign(gtk::Align::Start);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.add_css_class("title-3");
        let status = gtk::Label::new(Some("saved"));
        status.add_css_class("caption-heading");
        header.append(&title);
        header.append(&status);
        body.append(&header);

        let id = gtk::Label::new(Some(&format!(
            "{} • {}",
            provider_name(&key.provider_id),
            key.remote_job_id
        )));
        id.set_halign(gtk::Align::Start);
        id.set_selectable(true);
        id.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        id.add_css_class("harness-muted");
        body.append(&id);

        let progress_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let animation = gtk::Label::new(Some("·"));
        animation.add_css_class("harness-animation");
        let progress = gtk::ProgressBar::new();
        progress.set_hexpand(true);
        progress.set_pulse_step(0.06);
        progress_row.append(&animation);
        progress_row.append(&progress);
        body.append(&progress_row);
        let detail = gtk::Label::new(Some("Saved remote job"));
        detail.set_halign(gtk::Align::Start);
        detail.set_wrap(true);
        detail.add_css_class("harness-muted");
        body.append(&detail);

        let video = gtk::Video::new();
        video.set_height_request(270);
        video.set_hexpand(true);
        video.set_autoplay(false);
        video.set_loop(false);
        video.set_visible(false);
        body.append(&video);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let pause = gtk::Button::with_label("Pause monitoring");
        pause.set_sensitive(false);
        let resume = gtk::Button::with_label("Resume");
        resume.add_css_class("suggested-action");
        let open = gtk::Button::with_label("Open externally");
        open.set_sensitive(false);
        open.set_tooltip_text(Some("Open the saved video (Ctrl+O)"));
        actions.append(&pause);
        actions.append(&resume);
        actions.append(&open);
        body.append(&actions);
        row.set_child(Some(&body));
        self.jobs_list.append(&row);
        self.jobs_stack.set_visible_child_name("list");

        let widgets = Rc::new(JobWidgets {
            _key: key.clone(),
            _root: row,
            title,
            status,
            detail,
            animation,
            progress,
            video,
            pause,
            resume,
            open,
            local_path: RefCell::new(None),
            active: Cell::new(false),
        });
        let weak = Self::weak(self);
        let key_for_pause = key.clone();
        widgets.pause.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send(ServiceCommand::PauseMonitor {
                    op_id: this.op_id(),
                    key: key_for_pause.clone(),
                });
            }
        });
        let weak = Self::weak(self);
        let key_for_resume = key.clone();
        widgets.resume.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send(ServiceCommand::Resume {
                    op_id: this.op_id(),
                    key: key_for_resume.clone(),
                });
                if let Some(widgets) = this.jobs.borrow().get(&key_for_resume) {
                    widgets.active.set(true);
                    widgets.pause.set_sensitive(true);
                    widgets.resume.set_sensitive(false);
                    widgets.status.set_text("resuming");
                    widgets.detail.set_text("Connecting to the provider…");
                }
                this.active_jobs.borrow_mut().insert(key_for_resume.clone());
            }
        });
        let weak = Self::weak(self);
        let weak_widgets = Rc::downgrade(&widgets);
        widgets.open.connect_clicked(move |_| {
            let (Some(this), Some(widgets)) = (weak.upgrade(), weak_widgets.upgrade()) else {
                return;
            };
            if let Some(path) = widgets.local_path.borrow().clone() {
                this.send(ServiceCommand::OpenVideo {
                    op_id: this.op_id(),
                    path,
                });
            }
        });
        self.jobs.borrow_mut().insert(key, Rc::clone(&widgets));
        widgets
    }

    fn restore_record(self: &Rc<Self>, record: JobRecord) {
        let key = record.key();
        let title = job_title(&record).unwrap_or_else(|| "Saved video generation".into());
        let widgets = self.ensure_job(key.clone(), &title);
        widgets.status.set_text(&record.status.replace('_', " "));
        if let Some(error) = &record.error {
            widgets.detail.set_text(error);
        } else if let Some(path) = &record.output_path {
            widgets
                .detail
                .set_text(&format!("Saved to {}", path.display()));
        } else if record.terminal() {
            widgets
                .detail
                .set_text("Remote job finished without a local video");
        } else {
            widgets
                .detail
                .set_text("Remote job saved — press Resume to continue monitoring");
        }
        if let Some(path) = record.output_path.as_ref().filter(|path| path.exists()) {
            widgets.local_path.replace(Some(path.clone()));
            widgets.open.set_sensitive(true);
            widgets.video.set_file(Some(&gio::File::for_path(path)));
            widgets.video.set_visible(true);
            widgets.progress.set_fraction(1.0);
            widgets.animation.set_text("✓");
            if self.latest_video.borrow().is_none() {
                self.latest_video.replace(Some(path.clone()));
            }
        } else if !record.terminal() {
            widgets.resume.set_sensitive(true);
            widgets.animation.set_text("Ⅱ");
        }
    }

    fn open_latest_video(&self) {
        let path = self.latest_video.borrow().clone();
        match path {
            Some(path) => {
                self.send(ServiceCommand::OpenVideo {
                    op_id: self.op_id(),
                    path,
                });
            }
            None => self.toast(
                "No downloaded video is available yet.",
                "dialog-information-symbolic",
            ),
        }
    }

    fn handle_service_disconnected(&self) {
        if self.service_disconnected.replace(true) {
            return;
        }
        self.pause_before_shutdown.set(false);
        self.pending_close_draft_op.set(None);
        self.shutdown_requested.set(false);
        self.prepared.borrow_mut().take();

        let pending = self.pending_submit_op.get();
        let submitted = pending.and_then(|op_id| self.clear_pending_submission(op_id));
        if let Some((revision, provider)) = submitted {
            self.uncertain_revision.set(Some(revision));
            let provider = provider.unwrap_or_else(|| self.selected_provider());
            self.show_submission_uncertain(
                &provider,
                "The background service stopped before it could confirm the paid request outcome. Treat this submission as potentially accepted.",
            );
        } else {
            self.toast(
                "The background service stopped. Close and reopen Video Harness.",
                "dialog-error-symbolic",
            );
        }
        self.update_compatibility();
        self.allow_close.set(true);
    }

    fn request_shutdown(&self) {
        if self.pending_close_draft_op.get().is_some() || self.shutdown_requested.get() {
            return;
        }
        if let Some(source) = self.save_timer.borrow_mut().take() {
            source.remove();
        }
        match self.queue_draft_save() {
            Ok(Some(op_id)) => {
                self.pending_close_draft_op.set(Some(op_id));
                self.update_compatibility();
                self.toast(
                    "Saving your latest draft before closing…",
                    "document-save-symbolic",
                );
            }
            Ok(None) => self.request_service_shutdown(),
            Err(()) => {}
        }
    }

    fn request_service_shutdown(&self) {
        if self.shutdown_requested.replace(true) {
            return;
        }
        if !self.send(ServiceCommand::Shutdown) {
            self.shutdown_requested.set(false);
            self.update_compatibility();
            return;
        }
        self.update_compatibility();
        self.toast(
            "Finishing local state before closing…",
            "document-save-symbolic",
        );
    }

    fn request_pause_and_shutdown(&self) {
        if self.pause_before_shutdown.replace(true) {
            return;
        }
        if !self.send(ServiceCommand::PauseAll {
            op_id: self.op_id(),
        }) {
            self.pause_before_shutdown.set(false);
            self.update_compatibility();
            return;
        }
        self.update_compatibility();
        self.toast(
            "Pausing local monitors before closing…",
            "media-playback-pause-symbolic",
        );
    }

    fn show_remote_job_warning(&self, heading: &str, key: &ProviderJobKey, message: &str) {
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body("The provider may continue this generation remotely. Copy the ID below and check the provider dashboard; Video Harness cannot reconstruct this job after it closes.")
            .build();
        dialog.add_response("acknowledge", "I copied the ID");
        dialog.set_close_response("acknowledge");
        dialog.set_default_response(Some("acknowledge"));

        let details = gtk::Box::new(gtk::Orientation::Vertical, 10);
        let provider = gtk::Label::new(Some(&format!(
            "Provider: {}",
            provider_name(&key.provider_id)
        )));
        provider.set_halign(gtk::Align::Start);
        let job_id = gtk::Label::new(Some(&key.remote_job_id));
        job_id.set_halign(gtk::Align::Start);
        job_id.set_selectable(true);
        job_id.set_wrap(true);
        job_id.add_css_class("monospace");
        let error = gtk::Label::new(Some(message));
        error.set_halign(gtk::Align::Start);
        error.set_wrap(true);
        error.add_css_class("harness-error");
        details.append(&provider);
        details.append(&job_id);
        details.append(&error);
        dialog.set_extra_child(Some(&details));
        dialog.present(Some(&self.window));
    }

    fn show_draft_save_failure(self: &Rc<Self>, message: &str) {
        let dialog = adw::AlertDialog::builder()
            .heading("Your latest draft could not be saved")
            .body(format!(
                "{message}\n\nKeep Video Harness open to fix or retry this, or close without saving the latest edits."
            ))
            .build();
        dialog.add_response("keep", "Keep open");
        dialog.add_response("discard", "Close without saving");
        dialog.set_close_response("keep");
        dialog.set_default_response(Some("keep"));
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        let weak = Self::weak(self);
        dialog.connect_response(Some("discard"), move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.request_service_shutdown();
            }
        });
        dialog.present(Some(&self.window));
    }

    fn show_submission_uncertain(&self, provider_id: &ProviderId, message: &str) {
        let dialog = adw::AlertDialog::builder()
            .heading("Do not submit this draft again yet")
            .body(format!(
                "{} may have accepted the paid request, but no remote job ID came back. Check the provider dashboard before retrying. Editing the draft will create a distinct request.",
                provider_name(provider_id)
            ))
            .build();
        dialog.add_response("acknowledge", "I understand");
        dialog.set_close_response("acknowledge");
        dialog.set_default_response(Some("acknowledge"));
        let error = gtk::Label::new(Some(message));
        error.set_halign(gtk::Align::Start);
        error.set_wrap(true);
        error.set_selectable(true);
        error.add_css_class("harness-error");
        dialog.set_extra_child(Some(&error));
        dialog.present(Some(&self.window));
    }

    fn show_uncertain_resolution(self: &Rc<Self>, record: UncertainSubmissionRecord) {
        if self.pending_submit_op.get().is_some() || self.close_in_progress() {
            self.toast(
                "Wait for the current operation to finish before changing this safety hold.",
                "dialog-warning-symbolic",
            );
            return;
        }
        let key = (record.provider_id.clone(), record.draft_fingerprint.clone());
        if self
            .pending_uncertain_clears
            .borrow()
            .values()
            .any(|pending| pending == &key)
        {
            return;
        }

        let dialog = adw::AlertDialog::builder()
            .heading("Check the provider before allowing a retry")
            .body(format!(
                "{} may already be generating and charging for this exact draft. Open its dashboard and look for the job first. Only clear this hold after you have confirmed there is no matching generation.",
                provider_name(&record.provider_id)
            ))
            .build();
        dialog.add_response("keep", "Keep blocked");
        dialog.add_response("clear", "I checked — allow a retry");
        dialog.set_close_response("keep");
        dialog.set_default_response(Some("keep"));
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);

        let timestamp = gtk::Label::new(Some(&format!(
            "Safety hold recorded {}",
            record.recorded_at.format("%Y-%m-%d %H:%M UTC")
        )));
        timestamp.set_halign(gtk::Align::Start);
        timestamp.set_wrap(true);
        timestamp.add_css_class("harness-warning");
        dialog.set_extra_child(Some(&timestamp));

        let weak = Self::weak(self);
        dialog.connect_response(Some("clear"), move |_, _| {
            let Some(this) = weak.upgrade() else { return };
            if this.pending_submit_op.get().is_some() || this.close_in_progress() {
                this.toast(
                    "The safety hold was not changed while another operation was active.",
                    "dialog-warning-symbolic",
                );
                return;
            }
            let still_current = this.current_uncertain_submission().is_some_and(|current| {
                current.provider_id == record.provider_id
                    && current.draft_fingerprint == record.draft_fingerprint
            });
            if !still_current {
                this.toast(
                    "The draft or safety state changed. Nothing was cleared.",
                    "dialog-warning-symbolic",
                );
                return;
            }
            let op_id = this.op_id();
            let key = (record.provider_id.clone(), record.draft_fingerprint.clone());
            this.pending_uncertain_clears
                .borrow_mut()
                .insert(op_id, key);
            this.update_compatibility();
            if !this.send(ServiceCommand::ClearUncertainSubmission {
                op_id,
                provider_id: record.provider_id.clone(),
                draft_fingerprint: record.draft_fingerprint.clone(),
            }) {
                this.pending_uncertain_clears.borrow_mut().remove(&op_id);
                this.update_compatibility();
            }
        });
        dialog.present(Some(&self.window));
    }

    fn confirm_quit(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::builder()
            .heading("Leave Video Harness?")
            .body("Accepted provider jobs continue remotely. Video Harness will pause local checks; use Resume all next time to download completed videos.")
            .build();
        dialog.add_response("cancel", "Keep open");
        dialog.add_response("quit", "Pause monitoring and quit");
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("quit", adw::ResponseAppearance::Suggested);
        let weak = Self::weak(self);
        dialog.connect_response(Some("quit"), move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.request_pause_and_shutdown();
            }
        });
        dialog.present(Some(&self.window));
    }
}

struct ComposeWidgets {
    page: gtk::ScrolledWindow,
    prompt: gtk::TextView,
    provider: gtk::DropDown,
    model: gtk::DropDown,
    model_description: gtk::Label,
    options: OptionWidgets,
    media_list: gtk::ListBox,
    media_empty: gtk::Label,
    remote_url: gtk::Entry,
    remote_kind: gtk::DropDown,
    remote_role: gtk::DropDown,
    compatibility: gtk::Label,
    review: gtk::Button,
    add_files: gtk::Button,
    add_url: gtk::Button,
    drop_zone: gtk::Box,
    drop_title: gtk::Label,
    drop_hint: gtk::Label,
}

impl ComposeWidgets {
    fn build() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.add_css_class("harness-page");

        let hero = gtk::Label::new(Some("Create something worth watching"));
        hero.set_halign(gtk::Align::Start);
        hero.add_css_class("harness-hero");
        content.append(&hero);
        let intro = gtk::Label::new(Some(
            "Write a prompt, arrange reference media, then review the exact request before anything is submitted.",
        ));
        intro.set_halign(gtk::Align::Start);
        intro.set_wrap(true);
        intro.add_css_class("harness-muted");
        content.append(&intro);

        let prompt = gtk::TextView::new();
        prompt.set_wrap_mode(gtk::WrapMode::WordChar);
        prompt.set_top_margin(10);
        prompt.set_bottom_margin(10);
        prompt.set_left_margin(10);
        prompt.set_right_margin(10);
        prompt.set_tooltip_text(Some("Describe the video you want to generate"));
        let prompt_scroll = gtk::ScrolledWindow::builder()
            .height_request(150)
            .has_frame(true)
            .child(&prompt)
            .build();
        content.append(&card(
            "Prompt",
            "The prompt is autosaved locally as you type.",
            &prompt_scroll,
        ));

        let provider = dropdown(&["OpenRouter", "fal.ai"]);
        let model = dropdown(&["Loading models…"]);
        model.set_sensitive(false);
        let provider_grid = gtk::Grid::builder()
            .column_spacing(16)
            .row_spacing(10)
            .hexpand(true)
            .build();
        provider_grid.attach(&field_label("Provider"), 0, 0, 1, 1);
        provider_grid.attach(&provider, 1, 0, 1, 1);
        provider_grid.attach(&field_label("Model"), 0, 1, 1, 1);
        provider_grid.attach(&model, 1, 1, 1, 1);
        provider.set_hexpand(true);
        model.set_hexpand(true);
        let model_description = gtk::Label::new(Some("Loading the current provider catalog…"));
        model_description.set_halign(gtk::Align::Start);
        model_description.set_wrap(true);
        model_description.add_css_class("harness-muted");
        provider_grid.attach(&model_description, 1, 2, 1, 1);
        content.append(&card(
            "Provider & model",
            "Capabilities and price data come from the provider catalog.",
            &provider_grid,
        ));

        let media_list = gtk::ListBox::new();
        media_list.set_selection_mode(gtk::SelectionMode::None);
        media_list.add_css_class("boxed-list");
        let media_empty = gtk::Label::new(Some("No reference media yet"));
        media_empty.add_css_class("harness-muted");
        media_empty.set_margin_top(10);
        media_empty.set_margin_bottom(10);

        let drop_zone = gtk::Box::new(gtk::Orientation::Vertical, 5);
        drop_zone.add_css_class("harness-drop-zone");
        let drop_icon = gtk::Image::from_icon_name("mail-attachment-symbolic");
        drop_icon.set_pixel_size(36);
        let drop_title = gtk::Label::new(Some("Drop reference media here"));
        drop_title.add_css_class("heading");
        let drop_hint = gtk::Label::new(Some("Files stay local until you press Review"));
        drop_hint.add_css_class("harness-muted");
        drop_zone.append(&drop_icon);
        drop_zone.append(&drop_title);
        drop_zone.append(&drop_hint);

        let add_files = gtk::Button::with_label("Choose files…");
        add_files.add_css_class("suggested-action");
        let remote_url = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Public https://… URL or file:///…")
            .build();
        remote_url.set_tooltip_text(Some(
            "Add one reference URL after explicitly choosing its media type",
        ));
        let remote_kind = dropdown(&["Choose type…", "Image", "Video", "Audio"]);
        remote_kind.set_tooltip_text(Some("Media type for this URL"));
        let remote_role = dropdown(&["Reference", "Start frame", "End frame"]);
        remote_role.set_sensitive(false);
        remote_role.set_tooltip_text(Some(
            "Image role; video and audio references use fixed input roles",
        ));
        let add_url = gtk::Button::with_label("Add");
        add_url.set_tooltip_text(Some("Add this typed reference URL"));
        let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        add_row.append(&add_files);
        add_row.append(&remote_url);
        add_row.append(&remote_kind);
        add_row.append(&remote_role);
        add_row.append(&add_url);

        let media_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        media_box.append(&drop_zone);
        media_box.append(&add_row);
        media_box.append(&media_empty);
        media_box.append(&media_list);
        content.append(&card(
            "Reference media",
            "Add images, video, or audio. Images can be frames or general references; video and audio use fixed input roles.",
            &media_box,
        ));

        let options = build_options();
        let options_grid = options_grid(&options);
        content.append(&card(
            "Generation controls",
            "Controls update to match the selected model; advanced JSON is validated at Review.",
            &options_grid,
        ));

        let compatibility = gtk::Label::new(Some("Choose a model and write a prompt to continue."));
        compatibility.set_halign(gtk::Align::Start);
        compatibility.set_wrap(true);
        compatibility.add_css_class("harness-muted");
        let review = gtk::Button::with_label("Review generation");
        review.add_css_class("suggested-action");
        review.add_css_class("pill");
        review.set_sensitive(false);
        let action = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        compatibility.set_hexpand(true);
        action.append(&compatibility);
        action.append(&review);
        content.append(&action);

        let clamp = adw::Clamp::builder()
            .maximum_size(960)
            .tightening_threshold(720)
            .child(&content)
            .build();
        let page = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .build();
        page.add_css_class("harness-canvas");
        Self {
            page,
            prompt,
            provider,
            model,
            model_description,
            options,
            media_list,
            media_empty,
            remote_url,
            remote_kind,
            remote_role,
            compatibility,
            review,
            add_files,
            add_url,
            drop_zone,
            drop_title,
            drop_hint,
        }
    }
}

struct JobsWidgets {
    page: gtk::Box,
    list: gtk::ListBox,
    stack: gtk::Stack,
    resume_all: gtk::Button,
    pause_all: gtk::Button,
}

impl JobsWidgets {
    fn build() -> Self {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.add_css_class("harness-canvas");
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bar.set_margin_start(24);
        bar.set_margin_end(24);
        bar.set_margin_top(18);
        bar.set_margin_bottom(12);
        let title = gtk::Label::new(Some("Generation jobs"));
        title.set_halign(gtk::Align::Start);
        title.set_hexpand(true);
        title.add_css_class("title-2");
        let pause_all = gtk::Button::with_label("Pause all");
        let resume_all = gtk::Button::with_label("Resume all");
        resume_all.add_css_class("suggested-action");
        bar.append(&title);
        bar.append(&pause_all);
        bar.append(&resume_all);
        page.append(&bar);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        let list_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_box.set_margin_start(24);
        list_box.set_margin_end(24);
        list_box.set_margin_bottom(24);
        list_box.append(&list);
        let list_scroll = gtk::ScrolledWindow::builder().child(&list_box).build();

        let empty = adw::StatusPage::builder()
            .icon_name("media-playlist-video-symbolic")
            .title("No jobs yet")
            .description("Reviewed generations will appear here. Remote jobs can be resumed after restarting Video Harness.")
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&list_scroll, Some("list"));
        stack.set_visible_child_name("empty");
        stack.set_vexpand(true);
        page.append(&stack);
        Self {
            page,
            list,
            stack,
            resume_all,
            pause_all,
        }
    }
}

struct ProvidersWidgets {
    page: gtk::ScrolledWindow,
    providers: BTreeMap<ProviderId, ProviderWidgets>,
    default_provider: gtk::DropDown,
}

impl ProvidersWidgets {
    fn build() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.add_css_class("harness-page");
        let title = gtk::Label::new(Some("Providers & Settings"));
        title.set_halign(gtk::Align::Start);
        title.add_css_class("harness-hero");
        content.append(&title);
        let note = gtk::Label::new(Some(
            "API keys are checked before they are used. Drafts never contain credentials.",
        ));
        note.set_halign(gtk::Align::Start);
        note.add_css_class("harness-muted");
        content.append(&note);

        let mut providers = BTreeMap::new();
        for (id, name) in PROVIDERS {
            let status = gtk::Label::new(Some("Checking connection…"));
            status.set_halign(gtk::Align::Start);
            status.add_css_class("harness-muted");
            let storage = gtk::Label::new(Some(
                "Keys are kept in memory unless keyring storage is selected.",
            ));
            storage.set_halign(gtk::Align::Start);
            storage.set_wrap(true);
            storage.add_css_class("harness-muted");
            let key = gtk::PasswordEntry::builder()
                .hexpand(true)
                .placeholder_text("Paste API key")
                .show_peek_icon(true)
                .build();
            let remember = gtk::CheckButton::with_label("Store in the system keyring");
            let connect = gtk::Button::with_label("Connect");
            connect.add_css_class("suggested-action");
            let forget = gtk::Button::with_label("Forget key");
            forget.add_css_class("destructive-action");
            forget.set_sensitive(false);
            let key_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            key_row.append(&key);
            key_row.append(&connect);
            key_row.append(&forget);
            let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
            body.append(&status);
            body.append(&key_row);
            body.append(&remember);
            body.append(&storage);
            content.append(&card(
                name,
                match id {
                    "fal" => "Local references upload to fal's public CDN only when you Review; uploads expire after 24 hours.",
                    _ => "OpenRouter accepts public HTTPS reference URLs; local reference files are intentionally blocked.",
                },
                &body,
            ));
            providers.insert(
                ProviderId::new(id).expect("static provider id"),
                ProviderWidgets {
                    status,
                    storage,
                    key,
                    remember,
                    connect,
                    forget,
                },
            );
        }

        let default_provider = dropdown(&["OpenRouter", "fal.ai"]);
        let default_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        default_body.append(&field_label("Default provider for new drafts"));
        default_body.append(&default_provider);
        content.append(&card(
            "Defaults",
            "This preference does not move or alter existing jobs.",
            &default_body,
        ));

        let clamp = adw::Clamp::builder()
            .maximum_size(880)
            .child(&content)
            .build();
        let page = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .build();
        Self {
            page,
            providers,
            default_provider,
        }
    }
}

fn build_options() -> OptionWidgets {
    OptionWidgets {
        duration: dropdown(&["Provider default"]),
        durations: RefCell::new(vec![None]),
        resolution: dropdown(&["Provider default"]),
        resolutions: RefCell::new(vec![None]),
        aspect: dropdown(&["Provider default"]),
        aspects: RefCell::new(vec![None]),
        size: dropdown(&["Provider default"]),
        sizes: RefCell::new(vec![None]),
        audio: gtk::Switch::new(),
        seed: gtk::Entry::builder()
            .placeholder_text("Provider default")
            .build(),
        schema_box: gtk::Box::new(gtk::Orientation::Vertical, 8),
        schema_controls: RefCell::new(BTreeMap::new()),
        advanced: gtk::TextView::new(),
    }
}

fn options_grid(options: &OptionWidgets) -> gtk::Box {
    let grid = gtk::Grid::builder()
        .column_spacing(16)
        .row_spacing(12)
        .hexpand(true)
        .build();
    let rows: [(&str, &gtk::Widget); 6] = [
        ("Duration", options.duration.upcast_ref()),
        ("Resolution", options.resolution.upcast_ref()),
        ("Aspect ratio", options.aspect.upcast_ref()),
        ("Exact size", options.size.upcast_ref()),
        ("Generate audio", options.audio.upcast_ref()),
        ("Seed", options.seed.upcast_ref()),
    ];
    for (index, (label, widget)) in rows.into_iter().enumerate() {
        grid.attach(&field_label(label), 0, index as i32, 1, 1);
        widget.set_hexpand(true);
        grid.attach(widget, 1, index as i32, 1, 1);
    }
    options.advanced.set_monospace(true);
    options.advanced.set_wrap_mode(gtk::WrapMode::WordChar);
    options.advanced.buffer().set_text("{}");
    let advanced_scroll = gtk::ScrolledWindow::builder()
        .height_request(130)
        .has_frame(true)
        .child(&options.advanced)
        .build();
    let expander = gtk::Expander::builder()
        .label("Advanced provider JSON")
        .child(&advanced_scroll)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.append(&grid);
    let schema_title = gtk::Label::new(Some("Model-specific controls"));
    schema_title.set_halign(gtk::Align::Start);
    schema_title.add_css_class("heading");
    body.append(&schema_title);
    body.append(&options.schema_box);
    body.append(&expander);
    body
}

fn card(title: &str, subtitle: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
    card.add_css_class("harness-card");
    let title = gtk::Label::new(Some(title));
    title.set_halign(gtk::Align::Start);
    title.add_css_class("harness-card-title");
    card.append(&title);
    let subtitle = gtk::Label::new(Some(subtitle));
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);
    subtitle.add_css_class("harness-muted");
    card.append(&subtitle);
    card.append(child);
    card
}

fn field_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(gtk::Align::Start);
    label.add_css_class("heading");
    label
}

fn dropdown(values: &[&str]) -> gtk::DropDown {
    let model = gtk::StringList::new(values);
    gtk::DropDown::builder().model(&model).build()
}

fn text(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

fn provider_id_for_index(index: u32) -> ProviderId {
    match index {
        1 => ProviderId::fal(),
        _ => ProviderId::openrouter(),
    }
}

fn index_for_provider(provider_id: &ProviderId) -> u32 {
    u32::from(provider_id == &ProviderId::fal())
}

fn provider_name(provider_id: &ProviderId) -> &'static str {
    match provider_id.as_str() {
        "fal" => "fal.ai",
        "openrouter" => "OpenRouter",
        _ => "Video provider",
    }
}

fn remote_kind_for_index(index: u32) -> Option<MediaKind> {
    match index {
        1 => Some(MediaKind::Image),
        2 => Some(MediaKind::Video),
        3 => Some(MediaKind::Audio),
        _ => None,
    }
}

fn default_role_for_kind(kind: MediaKind) -> MediaRole {
    match kind {
        MediaKind::Image => MediaRole::Reference,
        MediaKind::Video => MediaRole::VideoInput,
        MediaKind::Audio => MediaRole::AudioInput,
    }
}

fn role_for_kind(kind: MediaKind, image_role_index: u32) -> MediaRole {
    match kind {
        MediaKind::Image => role_for_index(image_role_index),
        MediaKind::Video | MediaKind::Audio => default_role_for_kind(kind),
    }
}

fn role_for_index(index: u32) -> MediaRole {
    match index {
        1 => MediaRole::StartFrame,
        2 => MediaRole::EndFrame,
        _ => MediaRole::Reference,
    }
}

fn index_for_role(role: MediaRole) -> u32 {
    match role {
        MediaRole::Reference => 0,
        MediaRole::StartFrame => 1,
        MediaRole::EndFrame => 2,
        MediaRole::VideoInput | MediaRole::AudioInput => 0,
    }
}

fn media_role_label(role: MediaRole) -> &'static str {
    match role {
        MediaRole::Reference => "Reference",
        MediaRole::StartFrame => "Start frame",
        MediaRole::EndFrame => "End frame",
        MediaRole::VideoInput => "Video input",
        MediaRole::AudioInput => "Audio input",
    }
}

fn media_kind_index(kind: MediaKind) -> usize {
    match kind {
        MediaKind::Image => 0,
        MediaKind::Video => 1,
        MediaKind::Audio => 2,
    }
}

fn media_kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "Image",
        MediaKind::Video => "Video",
        MediaKind::Audio => "Audio",
    }
}

fn media_kind_plural(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "images",
        MediaKind::Video => "videos",
        MediaKind::Audio => "audio clips",
    }
}

fn media_kind_icon(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image-x-generic-symbolic",
        MediaKind::Video => "video-x-generic-symbolic",
        MediaKind::Audio => "audio-x-generic-symbolic",
    }
}

fn selected_copy<T: Copy>(values: &RefCell<Vec<Option<T>>>, index: u32) -> Option<T> {
    values.borrow().get(index as usize).copied().flatten()
}

fn selected_clone<T: Clone>(values: &RefCell<Vec<Option<T>>>, index: u32) -> Option<T> {
    values.borrow().get(index as usize).cloned().flatten()
}

fn set_selected_copy<T: Copy + PartialEq>(
    dropdown: &gtk::DropDown,
    values: &RefCell<Vec<Option<T>>>,
    selected: Option<T>,
) {
    let index = values
        .borrow()
        .iter()
        .position(|value| *value == selected)
        .unwrap_or(0);
    dropdown.set_selected(index as u32);
}

fn set_selected_clone<T: Clone + PartialEq>(
    dropdown: &gtk::DropDown,
    values: &RefCell<Vec<Option<T>>>,
    selected: Option<T>,
) {
    let index = values
        .borrow()
        .iter()
        .position(|value| value == &selected)
        .unwrap_or(0);
    dropdown.set_selected(index as u32);
}

fn set_dropdown_strings(dropdown: &gtk::DropDown, labels: &[String]) {
    let refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
    dropdown.set_model(Some(&gtk::StringList::new(&refs)));
    dropdown.set_selected(0);
    dropdown.set_sensitive(labels.len() > 1);
}

fn set_optional_strings(
    dropdown: &gtk::DropDown,
    values: &RefCell<Vec<Option<String>>>,
    source: &[String],
) {
    let options = std::iter::once(None)
        .chain(source.iter().cloned().map(Some))
        .collect::<Vec<_>>();
    let labels = options
        .iter()
        .map(|value| value.clone().unwrap_or_else(|| "Provider default".into()))
        .collect::<Vec<_>>();
    set_dropdown_strings(dropdown, &labels);
    *values.borrow_mut() = options;
}

fn schema_type(schema: &serde_json::Value) -> Option<&str> {
    match schema.get("type") {
        Some(serde_json::Value::String(value)) => Some(value.as_str()),
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .find(|value| *value != "null"),
        _ => None,
    }
}

fn schema_control_value(control: &SchemaControl) -> Result<Option<serde_json::Value>, String> {
    match control {
        SchemaControl::Choice { widget, values } => {
            Ok(values.get(widget.selected() as usize).cloned().flatten())
        }
        SchemaControl::Text { widget, kind } => {
            let raw = widget.text();
            let value = raw.trim();
            if value.is_empty() {
                return Ok(None);
            }
            match kind {
                SchemaTextKind::String => Ok(Some(serde_json::Value::String(raw.to_string()))),
                SchemaTextKind::Integer => value
                    .parse::<i64>()
                    .map(serde_json::Value::from)
                    .map(Some)
                    .map_err(|_| "must be a whole number".into()),
                SchemaTextKind::Number => value
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .map(Some)
                    .ok_or_else(|| "must be a finite number".into()),
            }
        }
    }
}

fn set_schema_control(control: &SchemaControl, value: &serde_json::Value) {
    match control {
        SchemaControl::Choice { widget, values } => {
            if let Some(index) = values
                .iter()
                .position(|candidate| candidate.as_ref() == Some(value))
            {
                widget.set_selected(index as u32);
            }
        }
        SchemaControl::Text { widget, kind } => {
            let value = match kind {
                SchemaTextKind::String => value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string()),
                SchemaTextKind::Integer | SchemaTextKind::Number => value.to_string(),
            };
            widget.set_text(&value);
        }
    }
}

fn media_name(source: &MediaSource) -> String {
    match source {
        MediaSource::LocalFile { path } => path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Reference media")
            .to_owned(),
        MediaSource::RemoteUrl { url } => url
            .split('?')
            .next()
            .and_then(|url| url.rsplit('/').next())
            .filter(|name| !name.is_empty())
            .unwrap_or("Remote reference")
            .to_owned(),
    }
}

fn classify_local_reference(path: &Path) -> Option<MediaKind> {
    let kind = media_kind_for_extension(path)?;
    MediaSource::local(path).validate_for_kind(kind).ok()?;
    Some(kind)
}

fn media_kind_for_extension(path: &Path) -> Option<MediaKind> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)?;
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "avif" | "bmp" | "tif" | "tiff" => {
            Some(MediaKind::Image)
        }
        "mp4" | "mov" => Some(MediaKind::Video),
        "mp3" | "wav" => Some(MediaKind::Audio),
        _ => None,
    }
}

fn typed_reference_counts(request: &crate::domain::VideoRequest) -> String {
    let mut counts = [request.frame_images.len(), 0, 0];
    for reference in &request.input_references {
        counts[media_kind_index(MediaKind::from(reference.kind))] += 1;
    }
    format!(
        "{}, {}, {}",
        count_label(counts[0], "image", "images"),
        count_label(counts[1], "video", "videos"),
        count_label(counts[2], "audio clip", "audio clips")
    )
}

fn typed_media_ordinal(items: &[MediaItem], target: usize) -> String {
    let Some(target_item) = items.get(target) else {
        return format!("media item {}", target.saturating_add(1));
    };
    let kind = target_item.role.kind();
    let ordinal = items[..=target]
        .iter()
        .filter(|item| item.role.kind() == kind)
        .count();
    format!("{} {ordinal}", media_kind_label(kind))
}

fn typed_reference_details(request: &crate::domain::VideoRequest) -> Vec<String> {
    let mut ordinals = [0usize; 3];
    let mut details = Vec::with_capacity(
        request
            .frame_images
            .len()
            .saturating_add(request.input_references.len()),
    );
    for frame in &request.frame_images {
        ordinals[media_kind_index(MediaKind::Image)] += 1;
        details.push(format!(
            "Image {} ({}): {}",
            ordinals[media_kind_index(MediaKind::Image)],
            frame.frame_type.as_str().replace('_', " "),
            frame.url
        ));
    }
    for reference in &request.input_references {
        let kind = MediaKind::from(reference.kind);
        let index = media_kind_index(kind);
        ordinals[index] += 1;
        let role = match kind {
            MediaKind::Image => "general reference",
            MediaKind::Video => "video input",
            MediaKind::Audio => "audio input",
        };
        details.push(format!(
            "{} {} ({role}): {}",
            media_kind_label(kind),
            ordinals[index],
            reference.url
        ));
    }
    details
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn summary_row(label: &str, value: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let label = gtk::Label::new(Some(label));
    label.set_width_chars(14);
    label.set_halign(gtk::Align::Start);
    label.add_css_class("heading");
    let value = gtk::Label::new(Some(value));
    value.set_halign(gtk::Align::Start);
    value.set_wrap(true);
    value.set_selectable(true);
    value.set_hexpand(true);
    row.append(&label);
    row.append(&value);
    row
}

fn ellipsize_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.trim().chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn job_title(record: &JobRecord) -> Option<String> {
    record
        .request
        .as_ref()
        .map(|request| ellipsize_text(&request.prompt, 72))
        .filter(|title| !title.is_empty())
}

fn byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FrameImage, FrameType, InputReference, VideoRequest};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn navigation_indexes_use_stable_provider_ids() {
        assert_eq!(provider_id_for_index(0), ProviderId::openrouter());
        assert_eq!(provider_id_for_index(1), ProviderId::fal());
        assert_eq!(index_for_provider(&ProviderId::openrouter()), 0);
        assert_eq!(index_for_provider(&ProviderId::fal()), 1);
    }

    #[test]
    fn image_role_indexes_round_trip_and_typed_roles_are_fixed() {
        for role in [
            MediaRole::Reference,
            MediaRole::StartFrame,
            MediaRole::EndFrame,
        ] {
            assert_eq!(role_for_index(index_for_role(role)), role);
        }
        assert_eq!(
            default_role_for_kind(MediaKind::Video),
            MediaRole::VideoInput
        );
        assert_eq!(
            default_role_for_kind(MediaKind::Audio),
            MediaRole::AudioInput
        );
        assert_eq!(role_for_kind(MediaKind::Video, 2), MediaRole::VideoInput);
        assert_eq!(role_for_kind(MediaKind::Audio, 1), MediaRole::AudioInput);
        assert_eq!(remote_kind_for_index(0), None);
        assert_eq!(remote_kind_for_index(1), Some(MediaKind::Image));
        assert_eq!(remote_kind_for_index(2), Some(MediaKind::Video));
        assert_eq!(remote_kind_for_index(3), Some(MediaKind::Audio));
    }

    #[test]
    fn local_reference_classification_is_extension_and_signature_safe() {
        let directory = tempdir().expect("temp dir");
        let fixtures: [(&str, &[u8], MediaKind); 5] = [
            ("frame.PNG", b"\x89PNG\r\n\x1a\n", MediaKind::Image),
            ("clip.mp4", b"\0\0\0\0ftypmp42", MediaKind::Video),
            ("clip.mov", b"\0\0\0\0ftypqt  ", MediaKind::Video),
            ("sound.mp3", b"ID3typed-audio", MediaKind::Audio),
            ("sound.wav", b"RIFF\0\0\0\0WAVE", MediaKind::Audio),
        ];
        for (name, bytes, expected) in fixtures {
            let path = directory.path().join(name);
            std::fs::write(&path, bytes).expect("write media fixture");
            assert_eq!(classify_local_reference(&path), Some(expected), "{name}");
        }

        let disguised_video = directory.path().join("disguised.mp4");
        std::fs::write(&disguised_video, b"not really video").expect("write mismatch");
        assert_eq!(classify_local_reference(&disguised_video), None);
        assert_eq!(media_kind_for_extension(Path::new("unknown.mkv")), None);
        assert_eq!(media_kind_for_extension(Path::new("no-extension")), None);
    }

    #[test]
    fn typed_ordinals_count_each_media_kind_independently() {
        let items = vec![
            MediaItem {
                source: MediaSource::remote("https://example.test/one.png").expect("image"),
                role: MediaRole::Reference,
            },
            MediaItem {
                source: MediaSource::remote("https://example.test/clip.mp4").expect("video"),
                role: MediaRole::VideoInput,
            },
            MediaItem {
                source: MediaSource::remote("https://example.test/two.png").expect("image"),
                role: MediaRole::EndFrame,
            },
            MediaItem {
                source: MediaSource::remote("https://example.test/sound.wav").expect("audio"),
                role: MediaRole::AudioInput,
            },
        ];
        assert_eq!(typed_media_ordinal(&items, 0), "Image 1");
        assert_eq!(typed_media_ordinal(&items, 1), "Video 1");
        assert_eq!(typed_media_ordinal(&items, 2), "Image 2");
        assert_eq!(typed_media_ordinal(&items, 3), "Audio 1");
    }

    #[test]
    fn review_summary_exposes_typed_counts_and_ordinals() {
        let mut request =
            VideoRequest::for_provider(ProviderId::fal(), "model", "prompt").expect("request");
        request.frame_images.push(
            FrameImage::new("https://example.test/start.png", FrameType::FirstFrame)
                .expect("frame"),
        );
        request.input_references.push(
            InputReference::with_kind("https://example.test/ref.png", MediaKind::Image)
                .expect("image"),
        );
        request.input_references.push(
            InputReference::with_kind("https://example.test/clip.mp4", MediaKind::Video)
                .expect("video"),
        );
        request.input_references.push(
            InputReference::with_kind("https://example.test/sound.wav", MediaKind::Audio)
                .expect("audio"),
        );

        assert_eq!(
            typed_reference_counts(&request),
            "2 images, 1 video, 1 audio clip"
        );
        assert_eq!(
            typed_reference_details(&request),
            vec![
                "Image 1 (first frame): https://example.test/start.png",
                "Image 2 (general reference): https://example.test/ref.png",
                "Video 1 (video input): https://example.test/clip.mp4",
                "Audio 1 (audio input): https://example.test/sound.wav",
            ]
        );
    }

    #[test]
    fn schema_type_ignores_nullable_marker() {
        assert_eq!(schema_type(&json!({"type": "integer"})), Some("integer"));
        assert_eq!(
            schema_type(&json!({"type": ["null", "string"]})),
            Some("string")
        );
        assert_eq!(schema_type(&json!({"type": "object"})), Some("object"));
    }

    #[test]
    fn prompt_summary_truncates_on_character_boundaries() {
        assert_eq!(ellipsize_text("hello", 10), "hello");
        assert_eq!(ellipsize_text("🦀🦀🦀", 2), "🦀🦀…");
    }
}
