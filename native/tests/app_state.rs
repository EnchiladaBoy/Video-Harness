use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use openrouter_video_studio::app::{
    Action, App, Completion, ComposeFocus, CostView, Effect, HistoryItem, LayoutMode, Modal,
    ProviderConnectionKind, Route, TaskEvent, TaskScope, TerminalCapabilities, UiModel, UiProvider,
};
use openrouter_video_studio::domain::{JobLocator, ProviderId};
use openrouter_video_studio::ui;
use ratatui::{Terminal, backend::TestBackend};

fn capabilities() -> TerminalCapabilities {
    TerminalCapabilities {
        unicode: false,
        color: false,
        reduced_motion: true,
    }
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
}

fn fixture_model() -> UiModel {
    UiModel {
        id: "black-forest-labs/flux-3-video".into(),
        name: "FLUX.3 Video".into(),
        description: "Local reducer fixture".into(),
        durations: vec![4, 8],
        resolutions: vec!["720p".into(), "1080p".into()],
        aspect_ratios: vec!["16:9".into(), "9:16".into()],
        frame_types: vec!["first_frame".into(), "last_frame".into()],
        generate_audio: Some(false),
        seed: Some(true),
        passthrough_parameters: vec!["guidance".into()],
        pricing: BTreeMap::from([("cents_per_second_output_720p".into(), "17".into())]),
        ..UiModel::default()
    }
}

fn ready_compose_app() -> App {
    let mut app = App::new(capabilities());
    let effects = app.update(Action::Task(TaskEvent::Ready {
        providers: vec![
            UiProvider {
                connection: ProviderConnectionKind::Connected,
                storage_note: "memory-only fixture".into(),
                ..UiProvider::openrouter()
            },
            UiProvider::fal(),
        ],
        default_provider: ProviderId::openrouter(),
    }));
    assert!(matches!(
        &effects[..],
        [Effect::LoadCatalog(provider)] if provider == &ProviderId::openrouter()
    ));
    app.update(Action::Task(TaskEvent::CatalogLoaded {
        provider_id: ProviderId::openrouter(),
        models: vec![fixture_model()],
        stale: false,
        remembered: HashMap::new(),
    }));
    assert_eq!(app.route, Route::Compose);
    app
}

#[test]
fn onboarding_masks_and_clears_the_key_before_emitting_a_redacted_effect() {
    let mut app = App::new(capabilities());
    app.update(Action::Paste("sk-test-placeholder".into()));
    assert!(!app.onboarding.key.masked().contains("sk-test-placeholder"));

    let effects = app.update(key(KeyCode::Enter));
    assert!(app.onboarding.validating);
    assert!(app.onboarding.key.is_empty());
    let [
        Effect::ConnectKey {
            provider_id,
            key: secret,
        },
    ] = &effects[..]
    else {
        panic!("expected one credential validation effect");
    };
    assert_eq!(provider_id, &ProviderId::openrouter());
    assert_eq!(
        secret.expose().expect("UTF-8 fixture key"),
        "sk-test-placeholder"
    );
    assert!(!format!("{secret:?}").contains("sk-test-placeholder"));
}

#[test]
fn confirmation_is_the_only_path_to_exactly_one_paid_submission_effect() {
    let mut app = ready_compose_app();
    app.update(Action::Paste(
        "A tiny cinema drifting through sunset clouds".into(),
    ));

    let review_effects = app.update(ctrl(KeyCode::Enter));
    assert!(matches!(&review_effects[..], [Effect::Quote(_)]));
    app.update(Action::Task(TaskEvent::QuoteLoaded {
        provider_id: ProviderId::openrouter(),
        model_id: fixture_model().id,
        quote: CostView {
            amount: Some("0.85".into()),
            currency: "USD".into(),
            basis: "fixture quote".into(),
            exact: true,
            raw_pricing: BTreeMap::new(),
        },
    }));
    assert!(matches!(app.modal, Some(Modal::Confirmation { .. })));

    let confirmation_effects = app.update(key(KeyCode::Enter));
    assert_eq!(app.route, Route::Progress);
    assert!(app.generation.submitting);
    assert!(matches!(
        &confirmation_effects[..],
        [Effect::PersistSettings { .. }, Effect::SubmitOnce(_)]
    ));
    assert_eq!(
        confirmation_effects
            .iter()
            .filter(|effect| matches!(effect, Effect::SubmitOnce(_)))
            .count(),
        1
    );

    // A repeated Enter is a progress-screen no-op, not a duplicate POST.
    let repeated = app.update(key(KeyCode::Enter));
    assert!(
        !repeated
            .iter()
            .any(|effect| matches!(effect, Effect::SubmitOnce(_)))
    );
}

#[test]
fn reducer_cost_estimate_matches_domain_cents_per_second_pricing() {
    let mut app = App::new(capabilities());
    let mut model = fixture_model();
    model.durations = vec![5];
    app.update(Action::Task(TaskEvent::CatalogLoaded {
        provider_id: ProviderId::openrouter(),
        models: vec![model],
        stale: false,
        remembered: HashMap::new(),
    }));
    app.route = Route::Compose;
    app.update(Action::Paste("Five seconds of fixture clouds".into()));

    assert_eq!(app.compose.estimate.amount.as_deref(), Some("0.85"));
    assert!(app.compose.estimate.exact);
    assert!(app.compose.estimate.basis.contains("17¢/video-second"));
}

#[test]
fn import_retry_pause_and_open_actions_never_submit_a_generation() {
    let mut app = ready_compose_app();
    app.route = Route::History;
    assert!(app.update(key(KeyCode::Char('i'))).is_empty());
    app.update(Action::Paste("job-existing-1".into()));
    let imported = app.update(key(KeyCode::Enter));
    assert!(matches!(
        &imported[..],
        [Effect::Import { provider_id, locator: JobLocator::OpenRouter { polling_url } }]
            if provider_id == &ProviderId::openrouter() && polling_url == "job-existing-1"
    ));

    app.update(Action::Task(TaskEvent::JobAccepted {
        provider_id: ProviderId::openrouter(),
        job_id: "job-existing-1".into(),
        status: "in_progress".into(),
    }));
    app.update(Action::Task(TaskEvent::Error {
        provider_id: Some(ProviderId::openrouter()),
        scope: TaskScope::Generation,
        message: "fixture timeout".into(),
        recoverable: true,
    }));
    let retry = app.update(key(KeyCode::Char('r')));
    assert!(matches!(
        &retry[..],
        [Effect::Resume(key)] if key.remote_job_id == "job-existing-1"
    ));

    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(matches!(
        app.modal,
        Some(Modal::PauseMonitoring {
            pause_selected: false
        })
    ));
    app.update(key(KeyCode::Right));
    let paused = app.update(key(KeyCode::Enter));
    assert!(matches!(&paused[..], [Effect::CancelCurrent]));

    let path = PathBuf::from("/tmp/openrouter-video-fixture.mp4");
    app.update(Action::Task(TaskEvent::Completed(Completion {
        provider_id: ProviderId::openrouter(),
        job_id: "job-existing-1".into(),
        path: path.clone(),
        cost: Some("0.85".into()),
        currency: Some("USD".into()),
        request: None,
    })));
    let opened = app.update(key(KeyCode::Char('o')));
    assert!(matches!(&opened[..], [Effect::OpenVideo(value)] if value == &path));

    for effects in [&imported, &retry, &paused, &opened] {
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::SubmitOnce(_)))
        );
    }
}

#[test]
fn progress_cannot_be_left_before_a_recoverable_job_id_exists() {
    let mut app = App::new(capabilities());
    app.route = Route::Progress;
    app.generation.submitting = true;
    let effects = app.update(key(KeyCode::Esc));
    assert!(effects.is_empty());
    assert_eq!(app.route, Route::Progress);
    assert!(app.modal.is_none());
    assert!(app.toast.is_some());
}

#[test]
fn failed_submission_without_a_job_id_can_leave_progress_after_error() {
    let mut app = App::new(capabilities());
    app.route = Route::Progress;
    app.generation.submitting = true;
    app.generation.monitoring = true;
    app.update(Action::Task(TaskEvent::Error {
        provider_id: Some(ProviderId::openrouter()),
        scope: TaskScope::Generation,
        message: "submission result is uncertain".into(),
        recoverable: true,
    }));
    assert!(!app.generation.submitting);
    assert!(!app.generation.monitoring);
    assert!(app.generation.job_id.is_none());

    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert_eq!(app.route, Route::Compose);

    app.route = Route::Progress;
    let quit = app.update(key(KeyCode::Char('q')));
    assert!(matches!(&quit[..], [Effect::Quit]));
    assert!(app.should_quit);
}

#[test]
fn reducer_handles_layouts_and_reduced_motion_deterministically() {
    let mut app = App::new(capabilities());
    app.update(Action::Resize(39, 13));
    assert_eq!(app.layout_mode(), LayoutMode::TooSmall);

    let phase = app.generation.phase;
    app.route = Route::Progress;
    app.update(Action::Tick(Instant::now()));
    assert_eq!(app.generation.phase, phase);

    app.capabilities.reduced_motion = false;
    app.update(Action::Tick(Instant::now()));
    assert_eq!(app.generation.phase, phase + 1);
}

fn buffer_text(backend: &TestBackend) -> String {
    backend
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("")
}

fn render_at(app: &mut App, width: u16, height: u16) -> String {
    app.update(Action::Resize(width, height));
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("create test terminal");
    terminal
        .draw(|frame| ui::render(frame, app))
        .expect("render interface");
    buffer_text(terminal.backend())
}

#[test]
fn ratatui_test_backend_renders_wide_stacked_compact_and_too_small_interfaces() {
    let mut app = ready_compose_app();
    app.update(Action::Paste("A local render fixture".into()));
    for (width, height, mode) in [
        (120, 40, LayoutMode::Wide),
        (70, 24, LayoutMode::Stacked),
        (50, 20, LayoutMode::Compact),
    ] {
        let content = render_at(&mut app, width, height);
        assert_eq!(app.layout_mode(), mode);
        assert!(content.contains("Video Studio Beta"));
        assert!(content.contains("[OR]"));
        assert!(content.contains("Review & Generate"));
        assert!(
            content.is_ascii(),
            "non-ASCII render symbols: {:?}",
            content
                .chars()
                .filter(|character| !character.is_ascii())
                .collect::<Vec<_>>()
        );
    }

    let content = render_at(&mut app, 35, 10);
    assert_eq!(app.layout_mode(), LayoutMode::TooSmall);
    assert!(content.contains("Terminal too small"));
}

#[test]
fn switching_provider_uses_scoped_catalog_and_preserves_prompt_and_urls() {
    let mut app = ready_compose_app();
    let mut fal_model = fixture_model();
    fal_model.provider_id = ProviderId::fal();
    fal_model.id = "fal-ai/veo3".into();
    fal_model.name = "Veo 3 on fal".into();
    fal_model.frame_types.clear();
    app.update(Action::Task(TaskEvent::CatalogLoaded {
        provider_id: ProviderId::fal(),
        models: vec![fal_model],
        stale: false,
        remembered: HashMap::new(),
    }));
    assert_eq!(app.compose.models[0].provider_id, ProviderId::openrouter());

    app.compose
        .prompt
        .set_text("A lighthouse reflected in rain");
    app.compose
        .first_frame
        .set_text("https://example.com/first.png");
    app.compose
        .references
        .set_text("https://example.com/ref.png");
    app.compose
        .adapter_options
        .set_text(r#"{"parameters":{"guidance":4}}"#);
    app.compose.focus = ComposeFocus::Provider;
    let effects = app.update(key(KeyCode::Right));

    assert!(matches!(
        &effects[..],
        [Effect::PersistDefaultProvider(provider)] if provider == &ProviderId::fal()
    ));
    assert_eq!(app.compose.provider_id, ProviderId::fal());
    assert_eq!(app.compose.models[0].id, "fal-ai/veo3");
    assert_eq!(app.compose.prompt.text(), "A lighthouse reflected in rain");
    assert_eq!(
        app.compose.first_frame.text(),
        "https://example.com/first.png"
    );
    assert_eq!(app.compose.references.text(), "https://example.com/ref.png");
    assert!(app.compose.adapter_options.is_empty());
}

#[test]
fn provider_management_keeps_connections_independent_and_masks_key_entry() {
    let mut app = ready_compose_app();
    app.update(ctrl(KeyCode::Char('p')));
    assert_eq!(app.route, Route::Providers);
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(Action::Paste("fal-secret-fixture".into()));
    assert!(!app.providers.key.masked().contains("fal-secret-fixture"));
    let rendered = render_at(&mut app, 90, 28);
    assert!(!rendered.contains("fal-secret-fixture"));
    assert!(rendered.contains("********"));
    assert!(rendered.is_ascii());
    let effects = app.update(key(KeyCode::Enter));
    assert!(matches!(
        &effects[..],
        [Effect::ConnectKey { provider_id, .. }] if provider_id == &ProviderId::fal()
    ));

    app.update(Action::Task(TaskEvent::KeyValidated {
        provider_id: ProviderId::fal(),
        connection: ProviderConnectionKind::SessionOnly,
        storage_note: "session fixture".into(),
    }));
    assert_eq!(
        app.providers
            .get(&ProviderId::openrouter())
            .expect("OpenRouter state")
            .connection,
        ProviderConnectionKind::Connected
    );
    assert_eq!(
        app.providers
            .get(&ProviderId::fal())
            .expect("fal state")
            .connection,
        ProviderConnectionKind::SessionOnly
    );
}

#[test]
fn fal_import_accepts_queue_url_without_emitting_a_generation() {
    let mut app = ready_compose_app();
    app.compose.provider_id = ProviderId::fal();
    app.route = Route::History;
    app.update(key(KeyCode::Char('i')));
    app.update(Action::Paste(
        "https://queue.fal.run/fal-ai/veo3/requests/request-123/status".into(),
    ));
    let effects = app.update(key(KeyCode::Enter));
    assert!(matches!(
        &effects[..],
        [Effect::Import {
            provider_id,
            locator: JobLocator::Fal { endpoint_id, request_id, .. },
        }] if provider_id == &ProviderId::fal()
            && endpoint_id == "fal-ai/veo3"
            && request_id == "request-123"
    ));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::SubmitOnce(_)))
    );
}

#[test]
fn provider_badges_render_in_progress_and_history() {
    let mut app = ready_compose_app();
    app.generation.provider_id = ProviderId::fal();
    app.generation.job_id = Some("fal-request".into());
    app.route = Route::Progress;
    assert!(render_at(&mut app, 90, 28).contains("[FAL]"));

    app.route = Route::History;
    app.history.items = vec![HistoryItem {
        provider_id: ProviderId::fal(),
        job_id: "fal-request".into(),
        created: "2026-08-06 12:00".into(),
        status: "in_progress".into(),
        model: "fal-ai/veo3".into(),
        prompt: "Fixture".into(),
        ..HistoryItem::default()
    }];
    assert!(render_at(&mut app, 110, 32).contains("[FAL]"));
}
