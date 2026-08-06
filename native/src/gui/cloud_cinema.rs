//! A small, truthful processing scene for the selected active video job.
//!
//! The moving sky is deliberately decorative. Provider state, elapsed time, and
//! polling cadence are supplied by the caller and rendered as ordinary GTK
//! labels so assistive technology never has to infer meaning from animation.

use std::{
    cell::RefCell,
    f64::consts::{FRAC_PI_2, TAU},
    rc::{Rc, Weak},
    time::Duration,
};

use adw::prelude::*;
use gtk::{cairo, glib};

/// The cadence of decorative frames from the original Tiny Cloud Cinema.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(240);

/// Whether the selected job is eligible to animate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CloudCinemaActivity {
    /// There is no currently running job.
    #[default]
    Inactive,
    /// The provider reports that the job is still active.
    Active,
    /// The job is paused. The scene remains on its last frame.
    Paused,
    /// The job needs attention. The scene remains on its last frame.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnimationGate {
    activity: CloudCinemaActivity,
    mapped: bool,
    animations_enabled: bool,
    motion_allowed: bool,
}

impl Default for AnimationGate {
    fn default() -> Self {
        Self {
            activity: CloudCinemaActivity::Inactive,
            mapped: false,
            animations_enabled: true,
            motion_allowed: true,
        }
    }
}

impl AnimationGate {
    fn should_animate(self) -> bool {
        self.activity == CloudCinemaActivity::Active
            && self.mapped
            && self.animations_enabled
            && self.motion_allowed
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MotionState {
    phase: u64,
}

impl MotionState {
    fn advance(&mut self) {
        self.phase = self.phase.wrapping_add(1);
    }

    fn star_slots(self, width: u32) -> [u32; 6] {
        let width = i64::from(width.max(1));
        std::array::from_fn(|index| {
            let direction = if index % 2 == 0 { -1 } else { 1 };
            let position = index as i64 * 11 + self.phase as i64 * direction;
            position.rem_euclid(width) as u32
        })
    }

    fn cloud_slots(self, width: u32) -> [u32; 4] {
        let width = u64::from(width.max(1));
        std::array::from_fn(|index| {
            ((index as u64 * 17 + self.phase / (index as u64 + 2)) % width) as u32
        })
    }
}

struct Inner {
    canvas: gtk::DrawingArea,
    provider_label: gtk::Label,
    status_label: gtk::Label,
    detail_label: gtk::Label,
    timing_label: gtk::Label,
    job_label: gtk::Label,
    gate: AnimationGate,
    motion: MotionState,
    timer: Option<glib::SourceId>,
}

/// GTK widget containing the Tiny Cloud Cinema and its real job telemetry.
///
/// Keep this value alive for as long as its [`Self::widget`] is in the widget
/// tree. Cloning it is inexpensive and refers to the same scene.
#[derive(Clone)]
pub struct CloudCinema {
    root: gtk::Box,
    inner: Rc<RefCell<Inner>>,
}

impl Default for CloudCinema {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudCinema {
    /// Build a frozen scene. Call [`Self::set_activity`] with
    /// [`CloudCinemaActivity::Active`] once the selected job is actually active.
    pub fn new() -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .accessible_role(gtk::AccessibleRole::Group)
            .build();
        root.update_property(&[gtk::accessible::Property::Label(
            "Tiny Cloud Cinema job telemetry",
        )]);
        root.add_css_class("card");
        root.add_css_class("cloud-cinema");

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(16)
            .margin_end(16)
            .build();
        root.append(&content);

        let provider_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .visible(false)
            .build();
        provider_label.add_css_class("caption");
        provider_label.add_css_class("dim-label");
        content.append(&provider_label);

        let canvas = gtk::DrawingArea::builder()
            .content_height(180)
            .hexpand(true)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        canvas.set_can_focus(false);
        content.append(&canvas);

        let status_label = gtk::Label::builder()
            .label("Waiting")
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .build();
        status_label.add_css_class("title-3");
        content.append(&status_label);

        let detail_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .build();
        content.append(&detail_label);

        let timing_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .visible(false)
            .build();
        timing_label.add_css_class("caption");
        timing_label.add_css_class("dim-label");
        timing_label.add_css_class("monospace");
        content.append(&timing_label);

        let job_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .selectable(true)
            .visible(false)
            .build();
        job_label.add_css_class("caption");
        job_label.add_css_class("dim-label");
        job_label.add_css_class("monospace");
        content.append(&job_label);

        let gate = AnimationGate {
            animations_enabled: canvas.settings().is_gtk_enable_animations(),
            ..AnimationGate::default()
        };
        let inner = Rc::new(RefCell::new(Inner {
            canvas: canvas.clone(),
            provider_label,
            status_label,
            detail_label,
            timing_label,
            job_label,
            gate,
            motion: MotionState::default(),
            timer: None,
        }));

        install_draw_func(&canvas, Rc::downgrade(&inner));
        install_lifecycle_handlers(&canvas, Rc::downgrade(&inner));

        Self { root, inner }
    }

    /// The root widget to place in the selected job detail view.
    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Change whether the provider says this job is currently active.
    pub fn set_activity(&self, activity: CloudCinemaActivity) {
        self.root.remove_css_class("paused");
        self.root.remove_css_class("error");
        match activity {
            CloudCinemaActivity::Paused => self.root.add_css_class("paused"),
            CloudCinemaActivity::Error => self.root.add_css_class("error"),
            CloudCinemaActivity::Inactive | CloudCinemaActivity::Active => {}
        }

        self.inner.borrow_mut().gate.activity = activity;
        refresh_animation(&self.inner);
        self.inner.borrow().canvas.queue_draw();
    }

    /// Allow an app-level reduced-motion preference to freeze the decoration.
    /// GTK's `gtk-enable-animations` setting is always respected as well.
    pub fn set_motion_allowed(&self, allowed: bool) {
        self.inner.borrow_mut().gate.motion_allowed = allowed;
        refresh_animation(&self.inner);
    }

    /// Display the actual provider for the selected job.
    pub fn set_provider(&self, provider: Option<&str>) {
        let inner = self.inner.borrow();
        set_optional_label(&inner.provider_label, provider, None);
    }

    /// Display provider-supplied job status and detail without manufacturing a
    /// percentage or changing the caller's wording.
    pub fn set_status(&self, status: &str, detail: Option<&str>) {
        let inner = self.inner.borrow();
        inner.status_label.set_label(status);
        set_optional_label(&inner.detail_label, detail, None);
    }

    /// Display elapsed time and the real next-poll countdown supplied by the
    /// workflow. This method does not decrement or estimate either value.
    pub fn set_timing(&self, elapsed: Duration, next_poll: Option<Duration>) {
        let text = format_timing(elapsed, next_poll);
        let inner = self.inner.borrow();
        inner.timing_label.set_label(&text);
        inner.timing_label.set_visible(true);
    }

    /// Hide timing telemetry when no authoritative clock is available.
    pub fn clear_timing(&self) {
        self.inner.borrow().timing_label.set_visible(false);
    }

    /// Display the provider's real job identifier, if one has been issued.
    pub fn set_job_id(&self, job_id: Option<&str>) {
        let inner = self.inner.borrow();
        set_optional_label(&inner.job_label, job_id, Some("Job "));
    }
}

fn set_optional_label(label: &gtk::Label, value: Option<&str>, prefix: Option<&str>) {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    if let Some(value) = value {
        label.set_label(&format!("{}{value}", prefix.unwrap_or_default()));
        label.set_visible(true);
    } else {
        label.set_label("");
        label.set_visible(false);
    }
}

fn install_lifecycle_handlers(canvas: &gtk::DrawingArea, inner: Weak<RefCell<Inner>>) {
    let mapped_inner = inner.clone();
    canvas.connect_map(move |canvas| {
        let Some(inner) = mapped_inner.upgrade() else {
            return;
        };
        {
            let mut state = inner.borrow_mut();
            state.gate.mapped = true;
            state.gate.animations_enabled = canvas.settings().is_gtk_enable_animations();
        }
        refresh_animation(&inner);
    });

    let unmapped_inner = inner.clone();
    canvas.connect_unmap(move |_| {
        let Some(inner) = unmapped_inner.upgrade() else {
            return;
        };
        inner.borrow_mut().gate.mapped = false;
        refresh_animation(&inner);
    });

    let settings = canvas.settings();
    let animation_inner = inner.clone();
    settings.connect_gtk_enable_animations_notify(move |settings| {
        let Some(inner) = animation_inner.upgrade() else {
            return;
        };
        inner.borrow_mut().gate.animations_enabled = settings.is_gtk_enable_animations();
        refresh_animation(&inner);
        inner.borrow().canvas.queue_draw();
    });

    adw::StyleManager::default().connect_dark_notify(move |_| {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        inner.borrow().canvas.queue_draw();
    });
}

fn refresh_animation(inner: &Rc<RefCell<Inner>>) {
    let should_animate = inner.borrow().gate.should_animate();
    let running = inner.borrow().timer.is_some();
    match (should_animate, running) {
        (true, false) => start_animation(inner),
        (false, true) => stop_animation(inner),
        (true, true) | (false, false) => {}
    }
}

fn start_animation(inner: &Rc<RefCell<Inner>>) {
    let weak = Rc::downgrade(inner);
    let timer = glib::timeout_add_local(FRAME_INTERVAL, move || {
        let Some(inner) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let mut state = inner.borrow_mut();
        if !state.gate.should_animate() {
            state.timer = None;
            return glib::ControlFlow::Break;
        }
        state.motion.advance();
        state.canvas.queue_draw();
        glib::ControlFlow::Continue
    });
    inner.borrow_mut().timer = Some(timer);
}

fn stop_animation(inner: &Rc<RefCell<Inner>>) {
    if let Some(timer) = inner.borrow_mut().timer.take() {
        timer.remove();
    }
}

fn install_draw_func(canvas: &gtk::DrawingArea, inner: Weak<RefCell<Inner>>) {
    canvas.set_draw_func(move |_, context, width, height| {
        let phase = inner
            .upgrade()
            .map(|inner| inner.borrow().motion.phase)
            .unwrap_or_default();
        let dark = adw::StyleManager::default().is_dark();
        draw_scene(context, width, height, MotionState { phase }, dark);
    });
}

#[derive(Clone, Copy)]
struct Color(f64, f64, f64);

impl Color {
    fn set(self, context: &cairo::Context) {
        context.set_source_rgb(self.0, self.1, self.2);
    }

    fn set_alpha(self, context: &cairo::Context, alpha: f64) {
        context.set_source_rgba(self.0, self.1, self.2, alpha);
    }
}

#[derive(Clone, Copy)]
struct Palette {
    backdrop: Color,
    surface: Color,
    foreground: Color,
    muted: Color,
    cyan: Color,
    magenta: Color,
}

impl Palette {
    fn for_scheme(dark: bool) -> Self {
        if dark {
            Self {
                backdrop: Color(0.035, 0.055, 0.090),
                surface: Color(0.075, 0.105, 0.155),
                foreground: Color(0.930, 0.965, 1.000),
                muted: Color(0.470, 0.560, 0.660),
                cyan: Color(0.150, 0.900, 0.940),
                magenta: Color(0.965, 0.310, 0.790),
            }
        } else {
            Self {
                backdrop: Color(0.915, 0.950, 0.975),
                surface: Color(0.985, 0.995, 1.000),
                foreground: Color(0.080, 0.120, 0.170),
                muted: Color(0.430, 0.510, 0.590),
                cyan: Color(0.000, 0.570, 0.650),
                magenta: Color(0.790, 0.120, 0.560),
            }
        }
    }
}

fn draw_scene(context: &cairo::Context, width: i32, height: i32, motion: MotionState, dark: bool) {
    if width <= 0 || height <= 0 {
        return;
    }

    let palette = Palette::for_scheme(dark);
    let width = f64::from(width);
    let height = f64::from(height);
    let scale = (width / 520.0).min(height / 180.0).clamp(0.55, 1.35);
    let left = 4.0;
    let top = 4.0;
    let scene_width = (width - 8.0).max(1.0);
    let scene_height = (height - 8.0).max(1.0);

    context.set_antialias(cairo::Antialias::None);
    rounded_rectangle(context, left, top, scene_width, scene_height, 12.0 * scale);
    palette.backdrop.set(context);
    let _ = context.fill_preserve();
    palette
        .muted
        .set_alpha(context, if dark { 0.28 } else { 0.22 });
    context.set_line_width(1.0);
    let _ = context.stroke();

    let _ = context.save();
    rounded_rectangle(context, left, top, scene_width, scene_height, 12.0 * scale);
    context.clip();

    draw_stars(context, motion, left, top, scene_width, scale, palette);
    draw_clouds(context, motion, left, top, scene_width, scale, palette);
    draw_projector(context, motion, width, height, scale, palette);
    let _ = context.restore();
}

fn draw_stars(
    context: &cairo::Context,
    motion: MotionState,
    left: f64,
    top: f64,
    width: f64,
    scale: f64,
    palette: Palette,
) {
    let slots = motion.star_slots(72);
    for (index, slot) in slots.into_iter().enumerate() {
        let x = left + 12.0 * scale + f64::from(slot) / 71.0 * (width - 24.0 * scale);
        let y = top + (16.0 + f64::from(((index * 13) % 35) as u32)) * scale;
        let size = if index % 3 == 0 { 2.5 } else { 1.5 } * scale;
        palette
            .magenta
            .set_alpha(context, if index % 2 == 0 { 0.95 } else { 0.68 });
        context.rectangle(x - size, y - size / 2.0, size * 2.0, size);
        context.rectangle(x - size / 2.0, y - size, size, size * 2.0);
        let _ = context.fill();
    }
}

fn draw_clouds(
    context: &cairo::Context,
    motion: MotionState,
    left: f64,
    top: f64,
    width: f64,
    scale: f64,
    palette: Palette,
) {
    let slots = motion.cloud_slots(72);
    for (index, slot) in slots.into_iter().enumerate() {
        let x = left - 18.0 * scale + f64::from(slot) / 71.0 * (width + 18.0 * scale);
        let y = top + (47.0 + f64::from(((index * 17) % 28) as u32)) * scale;
        let block = 4.0 * scale;
        palette
            .cyan
            .set_alpha(context, if index % 2 == 0 { 0.40 } else { 0.24 });
        for (column, row) in [(0, 1), (1, 0), (1, 1), (2, 0), (2, 1), (3, 1)] {
            context.rectangle(
                x + f64::from(column) * block,
                y + f64::from(row) * block,
                block,
                block,
            );
        }
        let _ = context.fill();
    }
}

fn draw_projector(
    context: &cairo::Context,
    motion: MotionState,
    width: f64,
    height: f64,
    scale: f64,
    palette: Palette,
) {
    let center_x = width / 2.0;
    let body_y = height * 0.57;
    let body_width = 176.0 * scale;
    let body_height = 38.0 * scale;
    let reel_y = body_y - 13.0 * scale;
    let reel_offset = 46.0 * scale;
    let reel_radius = 17.0 * scale;

    palette.cyan.set_alpha(context, 0.12);
    context.move_to(center_x - body_width * 0.56, body_y + body_height * 0.20);
    context.line_to(center_x - body_width * 0.78, height - 25.0 * scale);
    context.line_to(center_x + body_width * 0.78, height - 25.0 * scale);
    context.line_to(center_x + body_width * 0.56, body_y + body_height * 0.20);
    context.close_path();
    let _ = context.fill();

    palette.surface.set(context);
    context.rectangle(center_x - body_width / 2.0, body_y, body_width, body_height);
    let _ = context.fill_preserve();
    palette.foreground.set_alpha(context, 0.82);
    context.set_line_width((2.0 * scale).max(1.0));
    let _ = context.stroke();

    let angle = f64::from((motion.phase % 4) as u32) * FRAC_PI_2;
    draw_reel(
        context,
        center_x - reel_offset,
        reel_y,
        reel_radius,
        angle,
        palette,
    );
    draw_reel(
        context,
        center_x + reel_offset,
        reel_y,
        reel_radius,
        -angle,
        palette,
    );

    palette.magenta.set(context);
    context.rectangle(
        center_x - body_width * 0.36,
        body_y + body_height * 0.25,
        7.0 * scale,
        7.0 * scale,
    );
    let _ = context.fill();

    palette.foreground.set(context);
    context.select_font_face(
        "Monospace",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Bold,
    );
    context.set_font_size((10.0 * scale).max(7.0));
    let title = "TINY CLOUD CINEMA";
    if let Ok(extents) = context.text_extents(title) {
        context.move_to(
            center_x - extents.width() / 2.0 - extents.x_bearing(),
            body_y + body_height * 0.66,
        );
        let _ = context.show_text(title);
    }

    let ground_y = height - 20.0 * scale;
    palette.magenta.set_alpha(context, 0.72);
    context.set_line_width((2.0 * scale).max(1.0));
    for index in 0..24 {
        let x = center_x - 120.0 * scale + f64::from(index) * 10.0 * scale;
        context.move_to(x, ground_y);
        context.line_to(x + 5.0 * scale, ground_y);
    }
    let _ = context.stroke();
}

fn draw_reel(
    context: &cairo::Context,
    center_x: f64,
    center_y: f64,
    radius: f64,
    angle: f64,
    palette: Palette,
) {
    palette.surface.set(context);
    context.arc(center_x, center_y, radius, 0.0, TAU);
    let _ = context.fill_preserve();
    palette.cyan.set(context);
    context.set_line_width((2.0_f64).max(radius / 9.0));
    let _ = context.stroke();

    for offset in [0.0, FRAC_PI_2, FRAC_PI_2 * 2.0, FRAC_PI_2 * 3.0] {
        let spoke = angle + offset;
        context.move_to(center_x, center_y);
        context.line_to(
            center_x + spoke.cos() * radius * 0.70,
            center_y + spoke.sin() * radius * 0.70,
        );
    }
    let _ = context.stroke();

    palette.magenta.set(context);
    context.arc(center_x, center_y, radius * 0.18, 0.0, TAU);
    let _ = context.fill();
}

fn rounded_rectangle(
    context: &cairo::Context,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
) {
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    context.new_sub_path();
    context.arc(x + width - radius, y + radius, radius, -FRAC_PI_2, 0.0);
    context.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        FRAC_PI_2,
    );
    context.arc(
        x + radius,
        y + height - radius,
        radius,
        FRAC_PI_2,
        std::f64::consts::PI,
    );
    context.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::PI,
        std::f64::consts::PI + FRAC_PI_2,
    );
    context.close_path();
}

fn format_timing(elapsed: Duration, next_poll: Option<Duration>) -> String {
    let elapsed = elapsed.as_secs();
    let hours = elapsed / 3_600;
    let minutes = elapsed % 3_600 / 60;
    let seconds = elapsed % 60;
    let elapsed = if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    };
    match next_poll {
        Some(next_poll) => format!(
            "Observed this session: {elapsed}  •  checking again in {}s",
            next_poll.as_secs()
        ),
        None => format!("Observed this session: {elapsed}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animation_requires_every_gate() {
        let ready = AnimationGate {
            activity: CloudCinemaActivity::Active,
            mapped: true,
            animations_enabled: true,
            motion_allowed: true,
        };
        assert!(ready.should_animate());

        for blocked in [
            AnimationGate {
                activity: CloudCinemaActivity::Paused,
                ..ready
            },
            AnimationGate {
                mapped: false,
                ..ready
            },
            AnimationGate {
                animations_enabled: false,
                ..ready
            },
            AnimationGate {
                motion_allowed: false,
                ..ready
            },
        ] {
            assert!(!blocked.should_animate());
        }
    }

    #[test]
    fn motion_recreates_the_original_cloud_cinema_paths() {
        let first = MotionState::default();
        assert_eq!(first.star_slots(72), [0, 11, 22, 33, 44, 55]);
        assert_eq!(first.cloud_slots(72), [0, 17, 34, 51]);

        let mut next = first;
        next.advance();
        assert_eq!(next.star_slots(72), [71, 12, 21, 34, 43, 56]);
        assert_eq!(next.cloud_slots(72), [0, 17, 34, 51]);
    }

    #[test]
    fn timing_is_truthful_and_never_invents_a_countdown() {
        assert_eq!(
            format_timing(Duration::from_secs(62), None),
            "Observed this session: 1:02"
        );
        assert_eq!(
            format_timing(Duration::from_secs(3_723), Some(Duration::from_secs(7))),
            "Observed this session: 1:02:03  •  checking again in 7s"
        );
    }

    #[test]
    fn cadence_matches_the_original_widget() {
        assert_eq!(FRAME_INTERVAL, Duration::from_millis(240));
    }
}
