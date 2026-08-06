//! Pure Ratatui rendering for the transitional Video Harness TUI.

use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph,
        Row, Table, TableState, Wrap,
    },
};

use crate::{
    app::{
        App, ComposeFocus, CostView, HistoryItem, ImportFocus, LayoutMode, Modal,
        ProviderConnectionKind, Route, Severity, TerminalCapabilities, Toast, provider_name,
    },
    ui_input::TextEditor,
};

#[derive(Clone, Copy)]
struct Palette {
    unicode: bool,
    background: Color,
    panel: Color,
    panel_alt: Color,
    text: Color,
    muted: Color,
    cyan: Color,
    pink: Color,
    violet: Color,
    success: Color,
    warning: Color,
    error: Color,
}

impl Palette {
    fn for_terminal(capabilities: TerminalCapabilities) -> Self {
        if !capabilities.color {
            return Self {
                unicode: capabilities.unicode,
                background: Color::Reset,
                panel: Color::Reset,
                panel_alt: Color::Reset,
                text: Color::Reset,
                muted: Color::Reset,
                cyan: Color::Reset,
                pink: Color::Reset,
                violet: Color::Reset,
                success: Color::Reset,
                warning: Color::Reset,
                error: Color::Reset,
            };
        }
        Self {
            unicode: capabilities.unicode,
            background: Color::Rgb(13, 16, 32),
            panel: Color::Rgb(23, 27, 49),
            panel_alt: Color::Rgb(32, 38, 64),
            text: Color::Rgb(245, 243, 255),
            muted: Color::Rgb(170, 168, 192),
            cyan: Color::Rgb(102, 228, 255),
            pink: Color::Rgb(255, 121, 198),
            violet: Color::Rgb(168, 139, 255),
            success: Color::Rgb(101, 245, 165),
            warning: Color::Rgb(255, 209, 102),
            error: Color::Rgb(255, 107, 129),
        }
    }
}

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let palette = Palette::for_terminal(app.capabilities);
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.background).fg(palette.text)),
        frame.area(),
    );

    if app.layout_mode() == LayoutMode::TooSmall {
        render_too_small(frame, app, palette);
        return;
    }

    let shell = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    render_header(frame, shell[0], app, palette);
    match app.route {
        Route::Onboarding => render_onboarding(frame, shell[1], app, palette),
        Route::Compose => render_compose(frame, shell[1], app, palette),
        Route::Progress => render_progress(frame, shell[1], app, palette),
        Route::Complete => render_complete(frame, shell[1], app, palette),
        Route::History => render_history(frame, shell[1], app, palette),
        Route::Providers => render_providers(frame, shell[1], app, palette),
    }
    render_footer(frame, shell[2], app, palette);
    if let Some(modal) = app.modal.as_ref() {
        render_modal(frame, app, modal, palette);
    }
    if let Some(picker) = app.compose.picker.as_ref() {
        render_model_picker(frame, app, picker, palette);
    }
    if let Some(toast) = app.toast.as_ref() {
        render_toast(frame, toast, palette);
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let title = " Video Harness TUI ";
    let clock_width = app.clock.chars().count() as u16;
    let gap = area.width.saturating_sub(title.len() as u16 + clock_width);
    let line = Line::from(vec![
        Span::styled(
            title,
            Style::default()
                .fg(palette.cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap as usize)),
        Span::styled(app.clock.clone(), Style::default().fg(palette.muted)),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(palette.panel)),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let text = if app.modal.is_some() {
        match app.modal.as_ref() {
            Some(Modal::Confirmation { .. }) => " Enter generate   Esc cancel ",
            Some(Modal::PauseMonitoring { .. }) => {
                " Left/Right choose   Enter confirm   Esc keep watching "
            }
            Some(Modal::ImportJob { .. }) => " Enter import   Esc cancel ",
            Some(Modal::Help) => " Esc close help ",
            None => "",
        }
    } else {
        match app.route {
            Route::Onboarding => " Enter connect   Ctrl+P providers   Ctrl+Q quit ",
            Route::Compose => {
                " Tab focus   Ctrl+Enter review   Ctrl+H history   Ctrl+P providers   Ctrl+Q quit "
            }
            Route::Progress => " Esc/Q pause safely   R retry monitoring ",
            Route::Complete => {
                " O/Enter open   N new   R reuse   H history   Ctrl+P providers   Q quit "
            }
            Route::History => {
                " Up/Down select   Enter resume/open   I import   Ctrl+P providers   Esc back "
            }
            Route::Providers => " Up/Down provider   Enter edit key   U use   F forget   Esc back ",
        }
    };
    let compact = if area.width < 70 {
        match app.route {
            Route::Compose => " Tab focus  ^Enter review  ^H history ",
            Route::Progress => " Q pause  R retry ",
            Route::Complete => " O open  N new  R reuse  Q quit ",
            Route::History => " Up/Down  Enter open  I import ",
            Route::Onboarding => " Enter connect  ^P providers  ^Q quit ",
            Route::Providers => " Up/Down  Enter key  U use  F forget ",
        }
    } else {
        text
    };
    frame.render_widget(
        Paragraph::new(compact)
            .alignment(Alignment::Center)
            .style(Style::default().bg(palette.panel).fg(palette.muted)),
        area,
    );
}

fn render_too_small(frame: &mut Frame<'_>, app: &App, palette: Palette) {
    let mut lines = vec![
        Line::styled(
            "Terminal too small",
            Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw("Resize to at least 40 x 14."),
    ];
    if app.route == Route::Progress {
        lines.push(Line::raw(format!(
            "Job: {}",
            app.generation.job_id.as_deref().unwrap_or("waiting for ID")
        )));
        lines.push(Line::raw(format!("Status: {}", app.generation.status)));
    }
    lines.push(Line::styled(
        "Esc/Q: safe exit",
        Style::default().fg(palette.muted),
    ));
    let area = centered(
        frame.area(),
        36.min(frame.area().width),
        8.min(frame.area().height),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(panel_block(" Video Harness TUI ", false, palette)),
        area,
    );
}

fn render_onboarding(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let width = area.width.saturating_sub(4).min(76);
    let height = if app.short() { 12 } else { 16 }.min(area.height);
    let card = centered(area, width, height);
    let inner = card.inner(Margin::new(3.min(card.width / 4), 1));
    frame.render_widget(
        panel_block(
            if app.capabilities.unicode {
                " ✦ Video Harness TUI ✦ "
            } else {
                " Video Harness TUI "
            },
            false,
            palette,
        ),
        card,
    );
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .split(inner);
    if !app.short() {
        frame.render_widget(
            Paragraph::new("Paste an OpenRouter key, or press Ctrl+P to connect fal.ai instead.")
                .alignment(Alignment::Center)
                .style(Style::default().fg(palette.muted)),
            rows[0],
        );
    }
    frame.render_widget(
        Paragraph::new("OpenRouter API key").style(
            Style::default()
                .fg(palette.pink)
                .add_modifier(Modifier::BOLD),
        ),
        rows[1],
    );
    let mut masked = app
        .onboarding
        .key
        .masked_for_terminal(app.capabilities.unicode);
    if !app.onboarding.validating {
        insert_cursor(
            &mut masked,
            app.onboarding.key.cursor(),
            app.capabilities.unicode,
        );
    }
    frame.render_widget(
        Paragraph::new(masked)
            .block(focus_block(" key ", true, palette))
            .style(Style::default().bg(palette.panel_alt)),
        rows[2],
    );
    if !app.short() {
        frame.render_widget(
            Paragraph::new("Masked; saved in the system keyring when available. Never written to logs or history.")
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(palette.muted)),
            rows[3],
        );
    }
    frame.render_widget(
        Paragraph::new(app.onboarding.status.as_str())
            .wrap(Wrap { trim: true })
            .style(severity_style(app.onboarding.severity, palette)),
        rows[4],
    );
}

fn render_compose(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let page = area.inner(Margin::new(if area.width > 90 { 2 } else { 1 }, 0));
    let body = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(page);
    frame.render_widget(
        Paragraph::new("Video Harness TUI")
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(palette.cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        body[0],
    );
    match app.layout_mode() {
        LayoutMode::Wide => {
            let columns =
                Layout::horizontal([Constraint::Percentage(64), Constraint::Percentage(36)])
                    .spacing(1)
                    .split(body[1]);
            render_prompt_panel(frame, columns[0], app, palette);
            render_settings_panel(frame, columns[1], app, palette);
        }
        LayoutMode::Stacked | LayoutMode::Compact => {
            let prompt_percent = if app.short() { 42 } else { 48 };
            let rows = Layout::vertical([
                Constraint::Percentage(prompt_percent),
                Constraint::Percentage(100 - prompt_percent),
            ])
            .split(body[1]);
            render_prompt_panel(frame, rows[0], app, palette);
            render_settings_panel(frame, rows[1], app, palette);
        }
        LayoutMode::TooSmall => {}
    }
}

fn render_prompt_panel(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let rows = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(if app.short() { 0 } else { 2 }),
        Constraint::Length(3),
    ])
    .split(area);
    let focused = app.compose.focus == ComposeFocus::Prompt;
    let prompt = editor_display(&app.compose.prompt, focused, app.capabilities.unicode);
    frame.render_widget(
        Paragraph::new(prompt)
            .wrap(Wrap { trim: false })
            .block(focus_block(" Describe your video ", focused, palette))
            .style(Style::default().bg(palette.panel_alt).fg(palette.text)),
        rows[0],
    );
    if !app.short() {
        frame.render_widget(
            Paragraph::new("Tip: describe subject, motion, camera, lighting, and mood.")
                .style(Style::default().fg(palette.muted)),
            rows[1],
        );
    }
    let generate = button_span(
        "Review & Generate",
        app.compose.focus == ComposeFocus::Generate,
        palette,
    );
    let history = button_span(
        "History",
        app.compose.focus == ComposeFocus::History,
        palette,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![history, Span::raw("  "), generate]))
            .alignment(Alignment::Right),
        rows[2],
    );
}

fn render_settings_panel(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let model = app
        .compose
        .model_index
        .and_then(|index| app.compose.models.get(index));
    let mut lines = Vec::new();
    let provider = app.providers.get(&app.compose.provider_id);
    let provider_value = format!(
        "{} {}",
        provider_badge(&app.compose.provider_id),
        provider
            .map(|value| value.name.as_str())
            .unwrap_or_else(|| provider_name(&app.compose.provider_id))
    );
    lines.push(field_line(
        "Provider",
        &provider_value,
        app.compose.focus == ComposeFocus::Provider,
        palette,
    ));
    lines.push(field_line(
        "Model",
        model
            .map(|value| value.name.as_str())
            .unwrap_or("Loading..."),
        app.compose.focus == ComposeFocus::Model,
        palette,
    ));
    lines.push(Line::styled(
        app.compose.catalog_message.clone(),
        severity_style(
            if app.compose.catalog_stale {
                Severity::Warning
            } else {
                Severity::Info
            },
            palette,
        ),
    ));
    if let Some(model) = model {
        if !model.durations.is_empty() {
            lines.push(field_line(
                "Duration",
                option_label_u32(&model.durations, app.compose.duration_index),
                app.compose.focus == ComposeFocus::Duration,
                palette,
            ));
        }
        if !model.resolutions.is_empty() {
            lines.push(field_line(
                "Resolution",
                option_label(&model.resolutions, app.compose.resolution_index),
                app.compose.focus == ComposeFocus::Resolution,
                palette,
            ));
        }
        if !model.aspect_ratios.is_empty() {
            lines.push(field_line(
                "Aspect",
                option_label(&model.aspect_ratios, app.compose.aspect_index),
                app.compose.focus == ComposeFocus::AspectRatio,
                palette,
            ));
        }
        if !model.sizes.is_empty() {
            lines.push(field_line(
                "Exact size",
                option_label(&model.sizes, app.compose.size_index),
                app.compose.focus == ComposeFocus::Size,
                palette,
            ));
        }
        if model.supports_audio() {
            lines.push(field_line(
                "Audio",
                if app.compose.audio {
                    "[x] on"
                } else {
                    "[ ] off"
                },
                app.compose.focus == ComposeFocus::Audio,
                palette,
            ));
        }
        if model.seed.unwrap_or(false) {
            lines.push(field_line(
                "Seed",
                editor_inline(
                    &app.compose.seed,
                    app.compose.focus == ComposeFocus::Seed,
                    app.capabilities.unicode,
                )
                .as_str(),
                app.compose.focus == ComposeFocus::Seed,
                palette,
            ));
        }
    }
    lines.push(field_line(
        "Advanced",
        if app.compose.advanced {
            "[-] hide"
        } else {
            "[+] show"
        },
        app.compose.focus == ComposeFocus::AdvancedToggle,
        palette,
    ));
    if app.compose.advanced {
        if model.is_some_and(|model| model.supports_frame("first_frame")) {
            lines.push(field_line(
                "First frame",
                editor_inline(
                    &app.compose.first_frame,
                    app.compose.focus == ComposeFocus::FirstFrame,
                    app.capabilities.unicode,
                )
                .as_str(),
                app.compose.focus == ComposeFocus::FirstFrame,
                palette,
            ));
        }
        if model.is_some_and(|model| model.supports_frame("last_frame")) {
            lines.push(field_line(
                "Last frame",
                editor_inline(
                    &app.compose.last_frame,
                    app.compose.focus == ComposeFocus::LastFrame,
                    app.capabilities.unicode,
                )
                .as_str(),
                app.compose.focus == ComposeFocus::LastFrame,
                palette,
            ));
        }
        lines.push(field_line(
            "References",
            summarize_editor(&app.compose.references).as_str(),
            app.compose.focus == ComposeFocus::References,
            palette,
        ));
        lines.push(field_line(
            "Advanced options (JSON)",
            summarize_editor(&app.compose.adapter_options).as_str(),
            app.compose.focus == ComposeFocus::AdapterOptions,
            palette,
        ));
    }
    lines.push(Line::raw(""));
    lines.extend(cost_lines(&app.compose.estimate, palette));
    if let Some(model) = model
        && !model.description.is_empty()
        && !app.short()
    {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            model.description.clone(),
            Style::default().fg(palette.muted),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.compose.scroll, 0))
            .block(panel_block(" Provider, model & settings ", false, palette))
            .style(Style::default().bg(palette.panel)),
        area,
    );
}

fn render_progress(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let width = area.width.saturating_sub(4).min(86);
    let height = if app.short() { 14 } else { 19 }.min(area.height);
    let card = centered(area, width, height);
    frame.render_widget(
        panel_block(
            format!(
                " {} {} ",
                provider_badge(&app.generation.provider_id),
                provider_name(&app.generation.provider_id)
            ),
            false,
            palette,
        ),
        card,
    );
    let inner = card.inner(Margin::new(2.min(card.width / 4), 1));
    let mut lines = cloud_cinema(app, inner.width.saturating_sub(2));
    lines.push(Line::styled(
        format!("{}  {}", app.generation.status, app.generation.detail),
        Style::default()
            .fg(palette.cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let elapsed = app.elapsed(Instant::now()).as_secs();
    let mut timing = format!("Elapsed {}:{:02}", elapsed / 60, elapsed % 60);
    if let Some(countdown) = app.generation.countdown {
        timing.push_str(&format!(
            "  {}  checking again in {countdown}s",
            if app.capabilities.unicode { "•" } else { "-" }
        ));
    }
    lines.push(Line::styled(timing, Style::default().fg(palette.muted)));
    lines.push(Line::styled(
        app.generation.job_id.as_ref().map_or_else(
            || "Waiting for a job ID...".into(),
            |id| format!("Job {id}"),
        ),
        Style::default().fg(palette.muted),
    ));
    if app.generation.download_received > 0 {
        let received = app.generation.download_received as f64 / 1_048_576.0;
        let text = app.generation.download_total.map_or_else(
            || format!("Downloaded {received:.1} MiB"),
            |total| {
                format!(
                    "Downloaded {received:.1} / {:.1} MiB",
                    total as f64 / 1_048_576.0
                )
            },
        );
        lines.push(Line::styled(text, Style::default().fg(palette.success)));
    }
    if let Some(error) = app.generation.error.as_ref() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("Error: {error}"),
            Style::default()
                .fg(palette.error)
                .add_modifier(Modifier::BOLD),
        ));
        if app.generation.job_id.is_some() {
            lines.push(Line::styled(
                "Press R to retry monitoring; no new generation will be submitted.",
                Style::default().fg(palette.warning),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_complete(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let width = area.width.saturating_sub(4).min(76);
    let card = centered(area, width, 14.min(area.height));
    frame.render_widget(panel_block(" Video ready ", false, palette), card);
    let inner = card.inner(Margin::new(3.min(card.width / 4), 1));
    let Some(outcome) = app.completion.as_ref() else {
        frame.render_widget(Paragraph::new("Completion details unavailable."), inner);
        return;
    };
    let icon = if app.capabilities.unicode {
        "🎬"
    } else {
        "[film]"
    };
    let lines = vec![
        Line::styled(
            format!("{icon}  Your video is ready!"),
            Style::default()
                .fg(palette.cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(
            outcome.path.display().to_string(),
            Style::default().fg(palette.success),
        ),
        Line::raw(""),
        Line::styled(
            format!(
                "{}  Job {}  {}  Final cost {}",
                provider_badge(&outcome.provider_id),
                outcome.job_id,
                if app.capabilities.unicode { "•" } else { "-" },
                format_cost_currency(outcome.cost.as_deref(), outcome.currency.as_deref())
            ),
            Style::default().fg(palette.muted),
        ),
        Line::raw(""),
        Line::raw("Press O or Enter to open it in your default video player."),
        Line::raw(""),
        Line::from(vec![
            button_span("New video", false, palette),
            Span::raw("  "),
            button_span("Reuse settings", false, palette),
            Span::raw("  "),
            button_span("Open video", true, palette),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_providers(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let page = area.inner(Margin::new(if area.width > 90 { 4 } else { 1 }, 1));
    let rows = Layout::vertical([
        Constraint::Length(if app.short() { 1 } else { 3 }),
        Constraint::Min(5),
        Constraint::Length(if app.short() { 6 } else { 9 }),
    ])
    .split(page);
    frame.render_widget(
        Paragraph::new(if app.short() {
            "Providers"
        } else {
            "Provider connections\nKeys are isolated by provider and never shown or stored in history."
        })
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(palette.cyan)
                .add_modifier(Modifier::BOLD),
        ),
        rows[0],
    );

    let items = app
        .providers
        .providers
        .iter()
        .map(|provider| {
            let status = match provider.connection {
                ProviderConnectionKind::Connected => provider.connection.label(),
                ProviderConnectionKind::SessionOnly => provider.connection.label(),
                ProviderConnectionKind::NeedsKey => provider.connection.label(),
            };
            ListItem::new(format!(
                "{}  {:<12}  {}",
                provider_badge(&provider.id),
                provider.name,
                status
            ))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(
        (!items.is_empty()).then_some(app.providers.selected.min(items.len().saturating_sub(1))),
    );
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(
                Style::default()
                    .bg(palette.violet)
                    .fg(palette.background)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(if app.capabilities.unicode {
                "▶ "
            } else {
                "> "
            })
            .block(panel_block(" OpenRouter and fal.ai ", false, palette)),
        rows[1],
        &mut state,
    );

    let selected = app.providers.selected();
    let detail = rows[2];
    let inner = detail.inner(Margin::new(1, 1));
    frame.render_widget(panel_block(" Connection ", false, palette), detail);
    if app.providers.editing_key {
        let edit_rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(selected.map_or("API key", |provider| provider.name.as_str())).style(
                Style::default()
                    .fg(palette.pink)
                    .add_modifier(Modifier::BOLD),
            ),
            edit_rows[0],
        );
        let mut masked = app
            .providers
            .key
            .masked_for_terminal(app.capabilities.unicode);
        insert_cursor(
            &mut masked,
            app.providers.key.cursor(),
            app.capabilities.unicode,
        );
        frame.render_widget(
            Paragraph::new(masked).block(focus_block(" masked key ", true, palette)),
            edit_rows[1],
        );
        frame.render_widget(
            Paragraph::new("Enter validate & connect   Esc cancel")
                .alignment(Alignment::Center)
                .style(Style::default().fg(palette.muted)),
            edit_rows[2],
        );
    } else {
        let key_status = selected.map_or("Unavailable", |provider| {
            if provider.connection.has_key() {
                "******** (masked)"
            } else {
                "Not connected"
            }
        });
        let note = selected
            .map(|provider| provider.storage_note.as_str())
            .filter(|note| !note.is_empty())
            .unwrap_or(app.providers.status.as_str());
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(format!("Key: {key_status}")),
                Line::styled(
                    note.to_owned(),
                    severity_style(app.providers.severity, palette),
                ),
                Line::raw(""),
                Line::styled(
                    "Enter edit/replace   U use in Compose   F forget only this provider",
                    Style::default().fg(palette.muted),
                ),
            ])
            .wrap(Wrap { trim: true }),
            inner,
        );
    }
}

fn render_history(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let page = area.inner(Margin::new(1, 0));
    let rows = Layout::vertical([
        Constraint::Length(if app.short() { 1 } else { 2 }),
        Constraint::Min(7),
        Constraint::Length(if app.short() { 3 } else { 6 }),
    ])
    .split(page);
    frame.render_widget(
        Paragraph::new(if app.short() { "Generation history" } else { "Generation history\nPending jobs can be resumed after a restart. No API keys are stored here." })
            .alignment(Alignment::Center)
            .style(Style::default().fg(palette.cyan).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    let (header, table_rows, widths) = history_table(app, palette);
    let table = Table::new(table_rows, widths)
        .header(header)
        .row_highlight_style(
            Style::default()
                .bg(palette.violet)
                .fg(palette.background)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(if app.capabilities.unicode {
            "▶ "
        } else {
            "> "
        })
        .block(panel_block(" Jobs ", false, palette));
    let mut state = TableState::default()
        .with_selected((!app.history.items.is_empty()).then_some(app.history.selected));
    frame.render_stateful_widget(table, rows[1], &mut state);
    let detail = app
        .history
        .items
        .get(app.history.selected)
        .map(history_detail)
        .unwrap_or_else(|| {
            "No generations yet. Press I to import an existing provider job.".into()
        });
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .block(panel_block(" Details ", false, palette))
            .style(Style::default().fg(palette.muted)),
        rows[2],
    );
}

fn render_modal(frame: &mut Frame<'_>, app: &App, modal: &Modal, palette: Palette) {
    let (width, height, title) = match modal {
        Modal::Confirmation { .. } => (72, 20, " Ready for the premiere? "),
        Modal::PauseMonitoring { .. } => (68, 12, " Leave the screening room? "),
        Modal::ImportJob { draft } => (
            72,
            if draft.provider_id.as_str() == "fal" {
                17
            } else {
                12
            },
            " Import an existing job ",
        ),
        Modal::Help => (70, 20, " Keyboard help "),
    };
    let area = centered(
        frame.area(),
        width.min(frame.area().width.saturating_sub(2)),
        height.min(frame.area().height.saturating_sub(2)),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(title, true, palette), area);
    let inner = area.inner(Margin::new(2.min(area.width / 4), 1));
    match modal {
        Modal::Confirmation { request, estimate } => {
            let mut lines = vec![
                Line::styled(
                    format!(
                        "This submits exactly one paid generation request to {}.",
                        provider_name(&request.provider_id)
                    ),
                    Style::default()
                        .fg(palette.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::raw("The POST is never automatically retried after an ambiguous result."),
                Line::raw(""),
                Line::raw(format!(
                    "Provider: {} {}",
                    provider_badge(&request.provider_id),
                    provider_name(&request.provider_id)
                )),
                Line::raw(format!("Model: {}", request.model)),
                Line::raw(format!("Prompt: {}", request.prompt)),
                Line::raw(format!(
                    "Duration: {}",
                    request
                        .duration
                        .map_or_else(|| "provider default".into(), |value| format!("{value}s"))
                )),
                Line::raw(format!(
                    "Resolution: {}",
                    request
                        .size
                        .as_deref()
                        .or(request.resolution.as_deref())
                        .unwrap_or("provider default")
                )),
                Line::raw(""),
            ];
            lines.extend(cost_lines(estimate, palette));
            if estimate.amount.is_none() {
                lines.push(Line::styled(
                    "Confirm only if you accept an unknown charge.",
                    Style::default().fg(palette.warning),
                ));
            }
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                button_span("Go back [Esc]", false, palette),
                Span::raw("    "),
                button_span("Generate video [Enter]", true, palette),
            ]));
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: true })
                    .alignment(Alignment::Center),
                inner,
            );
        }
        Modal::PauseMonitoring { pause_selected } => {
            let lines = vec![
                Line::styled(
                    "The remote generation cannot be cancelled here and may still incur its full cost.",
                    Style::default().fg(palette.warning),
                ),
                Line::raw("The job is saved in History so monitoring can resume later."),
                Line::raw(""),
                Line::from(vec![
                    button_span("Keep watching", !pause_selected, palette),
                    Span::raw("    "),
                    button_span("Pause monitoring", *pause_selected, palette),
                ]),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner,
            );
        }
        Modal::ImportJob { draft } => {
            if draft.provider_id.as_str() == "fal" {
                let rows = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(1),
                ])
                .split(inner);
                frame.render_widget(
                    Paragraph::new("Paste a fal queue URL, or enter an endpoint ID and request ID. Import only monitors; it never creates a generation.")
                        .wrap(Wrap { trim: true }),
                    rows[0],
                );
                let locator_focus = draft.focus == ImportFocus::Locator;
                frame.render_widget(
                    Paragraph::new(editor_display(
                        &draft.locator,
                        locator_focus,
                        app.capabilities.unicode,
                    ))
                    .block(focus_block(
                        " Queue URL or endpoint ID ",
                        locator_focus,
                        palette,
                    )),
                    rows[1],
                );
                let request_focus = draft.focus == ImportFocus::RequestId;
                frame.render_widget(
                    Paragraph::new(editor_display(
                        &draft.request_id,
                        request_focus,
                        app.capabilities.unicode,
                    ))
                    .block(focus_block(
                        " Request ID (when using endpoint ID) ",
                        request_focus,
                        palette,
                    )),
                    rows[2],
                );
                frame.render_widget(
                    Paragraph::new("Tab field   Enter import & monitor   Esc cancel")
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(palette.muted)),
                    rows[3],
                );
            } else {
                let rows = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(1),
                ])
                .split(inner);
                frame.render_widget(Paragraph::new("Paste an OpenRouter video job ID or polling URL. This monitors an existing job and never creates a generation.").wrap(Wrap { trim: true }), rows[0]);
                frame.render_widget(
                    Paragraph::new(editor_display(
                        &draft.locator,
                        true,
                        app.capabilities.unicode,
                    ))
                    .block(focus_block(
                        " OpenRouter job ID or URL ",
                        true,
                        palette,
                    )),
                    rows[1],
                );
                frame.render_widget(
                    Paragraph::new("Enter import & monitor   Esc cancel")
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(palette.muted)),
                    rows[2],
                );
            }
        }
        Modal::Help => {
            frame.render_widget(Paragraph::new("All operations are keyboard accessible. Tab moves focus; contextual keys are always shown in the footer. Esc closes dialogs. Progress never displays a fabricated percentage.").wrap(Wrap { trim: true }), inner);
        }
    }
}

fn render_model_picker(
    frame: &mut Frame<'_>,
    app: &App,
    picker: &crate::app::ModelPicker,
    palette: Palette,
) {
    let area = centered(
        frame.area(),
        frame.area().width.saturating_sub(4).min(72),
        frame.area().height.saturating_sub(4).min(22),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        panel_block(
            format!(
                " {} {} models ",
                provider_badge(&picker.provider_id),
                provider_name(&picker.provider_id)
            ),
            true,
            palette,
        ),
        area,
    );
    let inner = area.inner(Margin::new(2.min(area.width / 4), 1));
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(editor_display(
            &picker.query,
            true,
            app.capabilities.unicode,
        ))
        .block(focus_block(" type to filter ", true, palette)),
        rows[0],
    );
    let items: Vec<ListItem> = picker
        .filtered
        .iter()
        .filter_map(|index| app.compose.models.get(*index))
        .map(|model| ListItem::new(format!("{}  ({})", model.name, model.id)))
        .collect();
    let mut state =
        ListState::default().with_selected((!items.is_empty()).then_some(picker.selected));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(palette.violet)
                .fg(palette.background)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(if app.capabilities.unicode {
            "▶ "
        } else {
            "> "
        });
    frame.render_stateful_widget(list, rows[1], &mut state);
    frame.render_widget(
        Paragraph::new("Up/Down select   Enter choose   Esc close")
            .alignment(Alignment::Center)
            .style(Style::default().fg(palette.muted)),
        rows[2],
    );
}

fn render_toast(frame: &mut Frame<'_>, toast: &Toast, palette: Palette) {
    let width = frame.area().width.saturating_sub(2).min(52);
    let height = 5.min(frame.area().height);
    let area = Rect::new(
        frame.area().right().saturating_sub(width + 1),
        frame.area().y.saturating_add(1),
        width,
        height,
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(toast.message.as_str())
            .wrap(Wrap { trim: true })
            .style(severity_style(toast.severity, palette))
            .block(panel_block(format!(" {} ", toast.title), true, palette)),
        area,
    );
}

fn history_table(
    app: &App,
    palette: Palette,
) -> (Row<'static>, Vec<Row<'static>>, Vec<Constraint>) {
    let compact = app.width < 60;
    let stacked = app.width < 80;
    let header = if compact {
        Row::new([Cell::from("Status"), Cell::from("Prompt")])
    } else if stacked {
        Row::new([
            Cell::from("Created"),
            Cell::from("Status"),
            Cell::from("Prompt"),
        ])
    } else {
        Row::new([
            Cell::from("Created"),
            Cell::from("Provider"),
            Cell::from("Status"),
            Cell::from("Model"),
            Cell::from("Prompt"),
            Cell::from("Cost"),
        ])
    }
    .style(
        Style::default()
            .fg(palette.pink)
            .add_modifier(Modifier::BOLD),
    );
    let rows = app
        .history
        .items
        .iter()
        .map(|item| {
            if compact {
                Row::new(vec![
                    Cell::from(format!(
                        "{} {}",
                        provider_badge(&item.provider_id),
                        item.status
                    )),
                    Cell::from(item.prompt.clone()),
                ])
            } else if stacked {
                Row::new(vec![
                    Cell::from(item.created.clone()),
                    Cell::from(format!(
                        "{} {}",
                        provider_badge(&item.provider_id),
                        item.status
                    )),
                    Cell::from(item.prompt.clone()),
                ])
            } else {
                Row::new(vec![
                    Cell::from(item.created.clone()),
                    Cell::from(provider_badge(&item.provider_id)),
                    Cell::from(item.status.clone()),
                    Cell::from(item.model.clone()),
                    Cell::from(item.prompt.clone()),
                    Cell::from(format_cost_currency(
                        item.cost.as_deref(),
                        item.currency.as_deref(),
                    )),
                ])
            }
        })
        .collect();
    let widths = if compact {
        vec![Constraint::Length(13), Constraint::Min(12)]
    } else if stacked {
        vec![
            Constraint::Length(16),
            Constraint::Length(7),
            Constraint::Length(13),
            Constraint::Min(12),
        ]
    } else {
        vec![
            Constraint::Length(16),
            Constraint::Length(13),
            Constraint::Length(24),
            Constraint::Min(18),
            Constraint::Length(12),
        ]
    };
    (header, rows, widths)
}

fn history_detail(item: &HistoryItem) -> String {
    let mut detail = format!(
        "Provider: {} {}\nJob: {}\nOutput: {}",
        provider_badge(&item.provider_id),
        provider_name(&item.provider_id),
        item.job_id,
        item.output_path.as_ref().map_or_else(
            || "Not downloaded".into(),
            |path| path.display().to_string()
        )
    );
    if let Some(error) = item.error.as_ref() {
        detail.push_str(&format!("\nError: {error}"));
    }
    if !item.prompt.is_empty() {
        detail.push_str(&format!("\nPrompt: {}", item.prompt));
    }
    detail
}

fn provider_badge(provider_id: &crate::domain::ProviderId) -> String {
    match provider_id.as_str() {
        "fal" => "[FAL]".into(),
        "openrouter" => "[OR]".into(),
        value => format!("[{value}]").to_ascii_uppercase(),
    }
}

fn cloud_cinema(app: &App, requested_width: u16) -> Vec<Line<'static>> {
    let width = requested_width.clamp(34, 72) as usize;
    let phase = if app.capabilities.reduced_motion {
        0
    } else {
        app.generation.phase as usize
    };
    let ascii = !app.capabilities.unicode;
    let star_color = if app.capabilities.color {
        Color::Magenta
    } else {
        Color::Reset
    };
    let cloud_color = if app.capabilities.color {
        Color::Cyan
    } else {
        Color::Reset
    };
    let mut sky = vec![' '; width];
    let stars = if ascii {
        ['*', '.', '+', '.', '*', '.']
    } else {
        ['✦', '·', '⋆', '✧', '·', '✶']
    };
    for index in 0..stars.len() {
        let position = (index * 11 + phase / (index + 1)) % width;
        sky[position] = stars[(phase + index) % stars.len()];
    }
    let mut clouds = vec![' '; width];
    for index in 0..4 {
        let position = (index * 17 + phase / (index + 2)) % width;
        clouds[position] = if ascii { '~' } else { '☁' };
    }
    let reels = if ascii {
        ['o', 'O', 'o', 'O']
    } else {
        ['◜', '◝', '◞', '◟']
    };
    let reel = reels[phase % reels.len()];
    let cinema = if ascii {
        format!("{reel}O\\  +-------------+  /O{reel}")
    } else {
        format!("{reel}◉╲  ┌─────────────┐  ╱◉{reel}")
    };
    let beam = if ascii {
        "\\      tiny cloud cinema      /"
    } else {
        "╲      tiny cloud cinema      ╱"
    };
    let ground = if ascii {
        "--------------------------------"
    } else {
        "────────────────────────────────"
    };
    vec![
        Line::styled(
            sky.into_iter().collect::<String>(),
            Style::default().fg(star_color),
        ),
        Line::styled(
            clouds.into_iter().collect::<String>(),
            Style::default().fg(cloud_color),
        ),
        Line::styled(cinema, Style::default().add_modifier(Modifier::BOLD)),
        Line::styled(beam, Style::default().fg(cloud_color)),
        Line::styled(ground, Style::default().fg(star_color)),
    ]
}

fn cost_lines(cost: &CostView, palette: Palette) -> Vec<Line<'static>> {
    if let Some(amount) = cost.amount.as_ref() {
        vec![
            Line::styled(
                format!(
                    "{}: {}",
                    if cost.exact {
                        "Quoted cost"
                    } else {
                        "Estimated cost"
                    },
                    format_cost_currency(Some(amount), Some(cost.currency.as_str()))
                ),
                Style::default()
                    .fg(palette.cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                terminal_text(&cost.basis, palette.unicode),
                Style::default().fg(palette.muted),
            ),
        ]
    } else {
        let mut lines = vec![
            Line::styled(
                "Cost estimate unavailable",
                Style::default()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                terminal_text(&cost.basis, palette.unicode),
                Style::default().fg(palette.muted),
            ),
        ];
        if !cost.raw_pricing.is_empty() {
            lines.push(Line::styled(
                format!("Advertised pricing: {:?}", cost.raw_pricing),
                Style::default().fg(palette.muted),
            ));
        }
        lines
    }
}

fn field_line(
    label: &str,
    value: impl Into<String>,
    focused: bool,
    palette: Palette,
) -> Line<'static> {
    let marker = if focused { "> " } else { "  " };
    let value = value.into();
    Line::from(vec![
        Span::styled(marker.to_owned(), Style::default().fg(palette.cyan)),
        Span::styled(format!("{label:<13}"), Style::default().fg(palette.muted)),
        Span::styled(
            value,
            if focused {
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(palette.text)
            },
        ),
    ])
}

fn editor_display(editor: &TextEditor, focused: bool, unicode: bool) -> String {
    let mut value = editor.text().to_owned();
    if focused {
        insert_cursor(&mut value, editor.cursor(), unicode);
    }
    value
}

fn editor_inline(editor: &TextEditor, focused: bool, unicode: bool) -> String {
    let value = if editor.is_empty() {
        "provider default".into()
    } else {
        editor
            .text()
            .replace('\n', if unicode { " ↵ " } else { " / " })
    };
    if focused {
        let mut value = value;
        insert_cursor(&mut value, editor.cursor(), unicode);
        value
    } else {
        value
    }
}

fn summarize_editor(editor: &TextEditor) -> String {
    if editor.trimmed().is_empty() {
        "not set".into()
    } else {
        editor
            .text()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(30)
            .collect()
    }
}

fn insert_cursor(value: &mut String, character_index: usize, unicode: bool) {
    let byte = value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(index, _)| index);
    value.insert_str(byte, if unicode { "▏" } else { "|" });
}

fn option_label(values: &[String], selected: Option<usize>) -> &str {
    selected
        .and_then(|index| values.get(index))
        .map(String::as_str)
        .unwrap_or("provider default")
}

fn option_label_u32(values: &[u32], selected: Option<usize>) -> &'static str {
    // Duration is rendered by the caller's line lifetime; use a compact set for
    // common provider values and a generic marker for uncommon ones.
    match selected.and_then(|index| values.get(index)).copied() {
        Some(1) => "1s",
        Some(2) => "2s",
        Some(3) => "3s",
        Some(4) => "4s",
        Some(5) => "5s",
        Some(6) => "6s",
        Some(8) => "8s",
        Some(10) => "10s",
        Some(_) => "custom duration",
        None => "provider default",
    }
}

fn format_cost_currency(value: Option<&str>, currency: Option<&str>) -> String {
    value.map_or_else(
        || "unavailable".into(),
        |amount| {
            let currency = currency.filter(|value| !value.is_empty()).unwrap_or("USD");
            if currency.eq_ignore_ascii_case("USD") {
                format!("${amount} USD")
            } else {
                format!("{amount} {currency}")
            }
        },
    )
}

fn terminal_text(value: &str, unicode: bool) -> String {
    if unicode {
        return value.to_owned();
    }
    value
        .replace('×', "x")
        .replace('¢', "c")
        .replace('—', "--")
        .replace('…', "...")
        .replace('•', "-")
}

fn button_span(label: &str, focused: bool, palette: Palette) -> Span<'static> {
    let style = if focused {
        Style::default()
            .fg(palette.background)
            .bg(palette.success)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.panel_alt)
    };
    Span::styled(format!(" {label} "), style)
}

fn panel_block<'a>(title: impl Into<Line<'a>>, emphasized: bool, palette: Palette) -> Block<'a> {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if emphasized {
            palette.pink
        } else {
            palette.violet
        }))
        .style(Style::default().bg(palette.panel).fg(palette.text))
        .padding(Padding::horizontal(1));
    terminal_border(block, palette)
}

fn focus_block<'a>(title: impl Into<Line<'a>>, focused: bool, palette: Palette) -> Block<'a> {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(if focused { palette.cyan } else { palette.muted })
                .add_modifier(if focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        );
    terminal_border(block, palette)
}

fn terminal_border<'a>(block: Block<'a>, palette: Palette) -> Block<'a> {
    if palette.unicode {
        block.border_type(BorderType::Rounded)
    } else {
        block.border_set(border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        })
    }
}

fn severity_style(severity: Severity, palette: Palette) -> Style {
    Style::default().fg(match severity {
        Severity::Info => palette.cyan,
        Severity::Success => palette.success,
        Severity::Warning => palette.warning,
        Severity::Error => palette.error,
    })
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}
