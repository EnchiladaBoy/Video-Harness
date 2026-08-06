"""Textual screens for OpenRouter Video Studio."""

from __future__ import annotations

import json
from dataclasses import asdict, is_dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Iterable, TYPE_CHECKING, cast

from textual import events, on, work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Center, Horizontal, Middle, ScrollableContainer, Vertical
from textual.screen import ModalScreen, Screen
from textual.widgets import (
    Button,
    DataTable,
    Footer,
    Header,
    Input,
    Label,
    Select,
    Static,
    Switch,
    TextArea,
)

from .widgets import CloudCinema, CostSummary, StatusPill, format_money

if TYPE_CHECKING:
    from .app import GenerationOutcome, OpenRouterVideoStudio
    from .models import CostEstimate, VideoModel, VideoRequest


def _attr(value: Any, *names: str, default: Any = None) -> Any:
    """Read a field from either a dataclass/object or an API-shaped dict."""

    for name in names:
        if isinstance(value, dict) and name in value:
            return value[name]
        candidate = getattr(value, name, None)
        if candidate is not None:
            return candidate
    return default


def _as_list(value: Any) -> list[Any]:
    if value is None:
        return []
    if isinstance(value, (str, bytes)):
        return [value]
    if isinstance(value, dict):
        return list(value)
    try:
        return list(value)
    except TypeError:
        return [value]


def _select_value(select: Select[Any]) -> Any | None:
    value = select.value
    # Textual's blank sentinel is deliberately not imported; it has changed names
    # across releases and is never one of our primitive option values.
    return value if isinstance(value, (str, int, float, bool)) else None


def _number(value: Any | None) -> int | float | None:
    if value in (None, ""):
        return None
    text = str(value)
    try:
        return int(text)
    except ValueError:
        try:
            return float(text)
        except ValueError:
            return cast(Any, value)


def _display_status(value: Any) -> str:
    return str(getattr(value, "value", value) or "unknown")


def _request_payload(request: Any) -> dict[str, Any]:
    if hasattr(request, "to_payload"):
        return cast(dict[str, Any], request.to_payload())
    if is_dataclass(request):
        return asdict(request)
    return dict(request) if isinstance(request, dict) else {"request": str(request)}


class OnboardingScreen(Screen[None]):
    """First-run API-key onboarding and validation."""

    BINDINGS = [Binding("ctrl+q", "app.quit", "Quit", show=True)]

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Middle(), Center():
            with Vertical(classes="narrow", id="onboarding-card"):
                yield Static("✦ OpenRouter Video Studio ✦", classes="hero")
                yield Static(
                    "Paste an OpenRouter key once, then make little movies from your terminal.",
                    classes="subtitle",
                )
                yield Label("OpenRouter API key", classes="section-title")
                yield Input(
                    placeholder="sk-or-v1-…",
                    password=True,
                    id="onboarding-key",
                )
                yield Static(
                    "The key is masked and saved in your Linux keyring when one is available. "
                    "It is never written to history or logs.",
                    classes="hint",
                )
                yield StatusPill("", id="onboarding-status")
                with Horizontal(classes="button-row"):
                    yield Button("Quit", id="quit", variant="default")
                    yield Button("Connect", id="connect", classes="primary", variant="primary")
        yield Footer()

    def on_mount(self) -> None:
        self.query_one("#onboarding-key", Input).focus()

    @on(Input.Submitted, "#onboarding-key")
    def key_submitted(self) -> None:
        self.validate_key()

    @on(Button.Pressed)
    def button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "quit":
            self.app.exit()
        elif event.button.id == "connect":
            self.validate_key()

    @work(exclusive=True)
    async def validate_key(self) -> None:
        key_input = self.query_one("#onboarding-key", Input)
        status = self.query_one("#onboarding-status", StatusPill)
        button = self.query_one("#connect", Button)
        api_key = key_input.value.strip()
        if not api_key:
            status.set("Enter an API key to continue.", "error")
            key_input.focus()
            return

        key_input.disabled = True
        button.disabled = True
        status.set("Validating securely with OpenRouter…")
        try:
            storage_note = await cast("OpenRouterVideoStudio", self.app).connect_api_key(api_key)
        except Exception as exc:  # The client normalizes actionable API errors.
            status.set(str(exc), "error")
            key_input.disabled = False
            button.disabled = False
            key_input.focus()
            return

        # Erase the widget value before leaving the screen so the key isn't held in
        # the inactive screen tree.
        key_input.value = ""
        status.set(f"Connected. {storage_note}", "success")
        # Keep onboarding beneath the studio instead of removing the screen that
        # owns this worker.  Switching it out from its own worker creates a
        # cancellation cycle in Textual 4 during shutdown.
        await self.app.push_screen(ComposeScreen())


class ComposeScreen(Screen[None]):
    """Prompt editor and capability-aware generation settings."""

    BINDINGS = [
        Binding("ctrl+enter", "generate", "Review & generate", show=True),
        Binding("ctrl+h", "history", "History", show=True),
        Binding("ctrl+k", "replace_key", "API key", show=True),
        Binding("ctrl+q", "app.quit", "Quit", show=True),
    ]

    def __init__(self, *, request: Any | None = None) -> None:
        super().__init__()
        self.initial_request = request
        self.models: dict[str, Any] = {}
        self.selected_model: Any | None = None
        self.current_estimate: Any | None = None
        self._setting_options: dict[str, dict[str, Any]] = {}
        self.catalog_stale = False

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Vertical(classes="page"):
            yield Static("OpenRouter Video Studio", classes="hero")
            with Horizontal(id="compose-grid"):
                with Vertical(id="prompt-panel"):
                    yield Label("Describe your video", classes="section-title")
                    yield TextArea(id="prompt", tab_behavior="indent")
                    yield Static(
                        "Tip: describe subject, motion, camera, lighting, and mood. Ctrl+Enter reviews the request.",
                        classes="hint",
                    )
                    with Horizontal(classes="button-row"):
                        yield Button("History", id="history")
                        yield Button("Review & Generate", id="generate", classes="primary", variant="primary")
                with ScrollableContainer(id="settings-panel"):
                    yield Label("Model & settings", classes="section-title")
                    yield Select([], prompt="Loading video models…", id="model", allow_blank=True)
                    yield StatusPill("Fetching OpenRouter's current video catalog…", id="model-status")
                    with Horizontal(classes="setting-row", id="duration-row"):
                        yield Label("Duration")
                        yield Select([], prompt="Provider default", id="duration", allow_blank=True)
                    with Horizontal(classes="setting-row", id="resolution-row"):
                        yield Label("Resolution")
                        yield Select([], prompt="Provider default", id="resolution", allow_blank=True)
                    with Horizontal(classes="setting-row", id="aspect-row"):
                        yield Label("Aspect ratio")
                        yield Select([], prompt="Provider default", id="aspect", allow_blank=True)
                    with Horizontal(classes="setting-row", id="size-row"):
                        yield Label("Exact size")
                        yield Select([], prompt="Provider default", id="size", allow_blank=True)
                    with Horizontal(classes="setting-row switch-row", id="audio-row"):
                        yield Label("Generate audio")
                        yield Switch(False, id="audio")
                    with Horizontal(classes="setting-row", id="seed-row"):
                        yield Label("Seed")
                        yield Input(placeholder="Optional integer", id="seed", type="integer")
                    yield Button("Show advanced options", id="toggle-advanced", variant="default")
                    with Vertical(id="advanced"):
                        yield Label("First-frame image URL", classes="section-title")
                        yield Input(placeholder="https://…", id="first-frame")
                        yield Label("Last-frame image URL", classes="section-title")
                        yield Input(placeholder="https://…", id="last-frame")
                        yield Label("Reference image URLs", classes="section-title")
                        yield TextArea(id="references")
                        yield Static("One public HTTPS URL per line.", classes="hint")
                        yield Label("Provider options (JSON)", classes="section-title")
                        yield TextArea(id="provider-json")
                        yield Static("Only parameters advertised by the selected model are accepted.", classes="hint")
                    yield CostSummary(id="cost")
        yield Footer()

    def on_mount(self) -> None:
        self.query_one("#advanced").display = False
        self.query_one("#generate", Button).disabled = True
        self.load_models()
        self.query_one("#prompt", TextArea).focus()

    def on_resize(self, event: events.Resize) -> None:
        """Stack the editor and settings when the terminal is narrow."""

        self.query_one("#compose-grid").set_class(event.size.width < 80, "compact")
        self.set_class(event.size.height < 28, "short")

    @work(exclusive=True, group="catalog")
    async def load_models(self) -> None:
        status = self.query_one("#model-status", StatusPill)
        try:
            catalog, stale = await cast("OpenRouterVideoStudio", self.app).get_video_catalog()
            self.catalog_stale = stale
            models = _as_list(_attr(catalog, "models", "data", default=catalog))
            self.models = {str(_attr(model, "id")): model for model in models if _attr(model, "id")}
            if not self.models:
                raise RuntimeError("OpenRouter returned no video models.")
            choices = [
                (str(_attr(model, "name", default=model_id) or model_id), model_id)
                for model_id, model in self.models.items()
            ]
            choices.sort(key=lambda item: item[0].lower())
            model_select = self.query_one("#model", Select)
            model_select.set_options(choices)
            preferred = cast("OpenRouterVideoStudio", self.app).preferred_model_id(self.models)
            model_select.value = preferred
            status.set(
                "Using cached catalog — settings may be stale." if stale else f"{len(choices)} video models available.",
                "warning" if stale else "success",
            )
            self._apply_initial_request()
        except Exception as exc:
            status.set(f"Could not load models: {exc}", "error")

    def _apply_initial_request(self) -> None:
        request = self.initial_request
        if request is None:
            return
        model_id = str(_attr(request, "model", default=""))
        if model_id in self.models:
            self.query_one("#model", Select).value = model_id
        self.query_one("#prompt", TextArea).text = str(_attr(request, "prompt", default=""))
        self.call_after_refresh(self._restore_request_settings, request)

    def _restore_request_settings(self, request: Any) -> None:
        mapping = {
            "duration": _attr(request, "duration"),
            "resolution": _attr(request, "resolution"),
            "aspect": _attr(request, "aspect_ratio"),
            "size": _attr(request, "size"),
        }
        for widget_id, value in mapping.items():
            select = self.query_one(f"#{widget_id}", Select)
            if value is not None:
                select.value = str(value)
        self.query_one("#audio", Switch).value = bool(_attr(request, "generate_audio", default=False))
        seed = _attr(request, "seed")
        self.query_one("#seed", Input).value = "" if seed is None else str(seed)
        frames = _as_list(_attr(request, "frame_images", default=()))
        for frame in frames:
            frame_type = _attr(frame, "frame_type", default="first_frame")
            url = str(_attr(frame, "url", default=""))
            if frame_type == "last_frame":
                self.query_one("#last-frame", Input).value = url
            else:
                self.query_one("#first-frame", Input).value = url
        references = _as_list(_attr(request, "input_references", default=()))
        self.query_one("#references", TextArea).text = "\n".join(
            str(_attr(reference, "url", default="")) for reference in references
        )
        provider = _attr(request, "provider")
        if provider:
            self.query_one("#provider-json", TextArea).text = json.dumps(dict(provider), indent=2)

    @on(Select.Changed, "#model")
    def model_changed(self, event: Select.Changed) -> None:
        model_id = event.value if isinstance(event.value, str) else None
        if not model_id or model_id not in self.models:
            return
        self.selected_model = self.models[model_id]
        self._configure_for_model(self.selected_model)
        self._refresh_estimate()

    def _configure_for_model(self, model: Any) -> None:
        capabilities = {
            "duration": _as_list(_attr(model, "supported_durations")),
            "resolution": _as_list(_attr(model, "supported_resolutions")),
            "aspect": _as_list(_attr(model, "supported_aspect_ratios")),
            "size": _as_list(_attr(model, "supported_sizes")),
        }
        for widget_id, values in capabilities.items():
            row = self.query_one(f"#{widget_id}-row")
            select = self.query_one(f"#{widget_id}", Select)
            row.display = bool(values)
            lookup = {str(value): value for value in values}
            self._setting_options[widget_id] = lookup
            select.set_options([(str(value), str(value)) for value in values])
            if values:
                # Exact pixel size is an alternative to resolution/aspect ratio,
                # not an additional constraint. Keep it blank while those friendly
                # controls are in use.
                if widget_id == "size" and (capabilities["resolution"] or capabilities["aspect"]):
                    select.clear()
                else:
                    preferred = self._preferred_setting(widget_id, values)
                    select.value = str(preferred)

        audio_support = _attr(model, "generate_audio", default=None)
        self.query_one("#audio-row").display = audio_support is not None and audio_support is not False
        self.query_one("#audio", Switch).value = False
        self.query_one("#seed-row").display = bool(_attr(model, "seed", default=False))

        frame_support = set(_as_list(_attr(model, "supported_frame_images", default=())))
        self.query_one("#first-frame", Input).disabled = "first_frame" not in frame_support
        self.query_one("#last-frame", Input).disabled = "last_frame" not in frame_support

        allowed = _as_list(_attr(model, "allowed_passthrough_parameters", default=()))
        # OpenRouter routing fields are useful even when the model advertises no
        # model-specific passthrough parameters, so the outer provider object stays
        # available in both cases.
        self.query_one("#provider-json", TextArea).disabled = False

        remembered = cast("OpenRouterVideoStudio", self.app).settings_for_model(str(_attr(model, "id")))
        for widget_id, key in (
            ("duration", "duration"),
            ("resolution", "resolution"),
            ("aspect", "aspect_ratio"),
            ("size", "size"),
        ):
            value = remembered.get(key)
            if value is not None and str(value) in self._setting_options.get(widget_id, {}):
                self.query_one(f"#{widget_id}", Select).value = str(value)
        if remembered.get("generate_audio") is not None and self.query_one("#audio-row").display:
            self.query_one("#audio", Switch).value = bool(remembered["generate_audio"])
        if remembered.get("seed") is not None and self.query_one("#seed-row").display:
            self.query_one("#seed", Input).value = str(remembered["seed"])

        description = str(_attr(model, "description", default="") or "")
        message = description[:180] if description else f"Selected {str(_attr(model, 'name', default=_attr(model, 'id')))}."
        if self.catalog_stale:
            message = f"Cached catalog (may be stale). {message}"
        self.query_one("#model-status", StatusPill).set(
            message,
            "warning" if self.catalog_stale else "info",
        )

    @staticmethod
    def _preferred_setting(kind: str, values: list[Any]) -> Any:
        if kind == "resolution":
            for value in values:
                if str(value).lower() == "720p":
                    return value
        if kind == "aspect":
            for value in values:
                if str(value) == "16:9":
                    return value
        if kind == "duration":
            try:
                return min(values, key=lambda item: float(str(item).rstrip("s")))
            except ValueError:
                pass
        return values[0]

    @on(Button.Pressed, "#toggle-advanced")
    def toggle_advanced(self, event: Button.Pressed) -> None:
        panel = self.query_one("#advanced")
        panel.display = not panel.display
        event.button.label = "Hide advanced options" if panel.display else "Show advanced options"

    @on(Button.Pressed, "#generate")
    def generate_pressed(self) -> None:
        self.action_generate()

    @on(Button.Pressed, "#history")
    def history_pressed(self) -> None:
        self.action_history()

    @on(TextArea.Changed, "#prompt")
    def prompt_changed(self) -> None:
        self._sync_generate_button()

    @on(Select.Changed)
    @on(Switch.Changed)
    @on(Input.Changed)
    @on(TextArea.Changed, "#provider-json")
    def settings_changed(self, event: Select.Changed | Switch.Changed | Input.Changed | TextArea.Changed) -> None:
        if isinstance(event, Select.Changed):
            if event.select.id == "size" and _select_value(event.select) is not None:
                self.query_one("#resolution", Select).clear()
                self.query_one("#aspect", Select).clear()
            elif event.select.id in {"resolution", "aspect"} and _select_value(event.select) is not None:
                self.query_one("#size", Select).clear()
        self._refresh_estimate()

    def _sync_generate_button(self) -> None:
        prompt = self.query_one("#prompt", TextArea).text.strip()
        self.query_one("#generate", Button).disabled = not (prompt and self.selected_model)

    def _build_request(self) -> Any:
        from .models import FrameImage, InputReference, VideoRequest

        model_id = _select_value(self.query_one("#model", Select))
        prompt = self.query_one("#prompt", TextArea).text.strip()
        if not model_id or self.selected_model is None:
            raise ValueError("Choose a video model.")
        if not prompt:
            raise ValueError("Write a video prompt first.")

        provider_text = self.query_one("#provider-json", TextArea).text.strip()
        provider: dict[str, Any] | None = None
        if provider_text:
            try:
                parsed = json.loads(provider_text)
            except json.JSONDecodeError as exc:
                raise ValueError(f"Provider options are not valid JSON: {exc.msg}.") from exc
            if not isinstance(parsed, dict):
                raise ValueError("Provider options must be a JSON object.")
            allowed = set(_as_list(_attr(self.selected_model, "allowed_passthrough_parameters", default=())))
            # OpenRouter's provider object contains routing fields plus an optional
            # nested `parameters` object. Only those passthrough parameter names are
            # model-specific; routing fields must not be rejected here.
            parameters = parsed.get("parameters", {})
            if not isinstance(parameters, dict):
                raise ValueError("provider.parameters must be a JSON object.")
            unrecognized = set(parameters) - allowed
            if unrecognized:
                raise ValueError("Unsupported provider option(s): " + ", ".join(sorted(unrecognized)))
            provider = parsed

        frames: list[FrameImage] = []
        first = self.query_one("#first-frame", Input).value.strip()
        last = self.query_one("#last-frame", Input).value.strip()
        if first:
            frames.append(FrameImage(self._validate_url(first, "First-frame URL"), "first_frame"))
        if last:
            frames.append(FrameImage(self._validate_url(last, "Last-frame URL"), "last_frame"))
        references = [
            InputReference(self._validate_url(line.strip(), "Reference URL"))
            for line in self.query_one("#references", TextArea).text.splitlines()
            if line.strip()
        ]

        seed_text = self.query_one("#seed", Input).value.strip()
        seed = int(seed_text) if seed_text else None
        duration = _select_value(self.query_one("#duration", Select))
        return VideoRequest(
            model=str(model_id),
            prompt=prompt,
            duration=_number(duration),
            resolution=_select_value(self.query_one("#resolution", Select)),
            aspect_ratio=_select_value(self.query_one("#aspect", Select)),
            size=_select_value(self.query_one("#size", Select)),
            generate_audio=(self.query_one("#audio", Switch).value if self.query_one("#audio-row").display else None),
            seed=seed,
            frame_images=tuple(frames),
            input_references=tuple(references),
            provider=provider,
        )

    @staticmethod
    def _validate_url(value: str, label: str) -> str:
        if not value.lower().startswith("https://"):
            raise ValueError(f"{label} must be a public HTTPS URL.")
        return value

    def _refresh_estimate(self) -> None:
        self._sync_generate_button()
        if self.selected_model is None:
            return
        try:
            from .models import estimate_cost

            request = self._build_request()
            self.current_estimate = estimate_cost(self.selected_model, request)
            self.query_one("#cost", CostSummary).show_estimate(self.current_estimate)
        except (ValueError, TypeError):
            # Incomplete form state is normal while typing.
            self.current_estimate = None

    def action_generate(self) -> None:
        try:
            request = self._build_request()
            problems = tuple(self.selected_model.supports_request(request))
            if problems:
                raise ValueError("\n".join(str(problem) for problem in problems))
            from .models import estimate_cost

            estimate = estimate_cost(self.selected_model, request)
        except (ValueError, TypeError) as exc:
            self.app.notify(str(exc), title="Check your request", severity="error", timeout=7)
            return

        def confirmed(result: bool | None) -> None:
            if result:
                cast("OpenRouterVideoStudio", self.app).remember_request(request)
                self.app.switch_screen(ProgressScreen(request=request, estimate=estimate))

        self.app.push_screen(ConfirmationScreen(request, estimate), confirmed)

    def action_history(self) -> None:
        self.app.switch_screen(HistoryScreen())

    def action_replace_key(self) -> None:
        cast("OpenRouterVideoStudio", self.app).forget_api_key()
        self.app.switch_screen(OnboardingScreen())


class ConfirmationScreen(ModalScreen[bool]):
    """Last, explicit checkpoint before a potentially billable POST."""

    BINDINGS = [Binding("escape", "cancel", "Cancel"), Binding("enter", "confirm", "Generate")]

    def __init__(self, request: Any, estimate: Any) -> None:
        super().__init__()
        self.request = request
        self.estimate = estimate

    def compose(self) -> ComposeResult:
        payload = _request_payload(self.request)
        display_payload = json.dumps(payload, indent=2, ensure_ascii=False)
        amount = _attr(self.estimate, "amount")
        exact = bool(_attr(self.estimate, "exact", default=False))
        with Middle(classes="modal-backdrop"), Center():
            with Vertical(classes="modal-card"):
                yield Static("Ready for the premiere?", classes="hero")
                yield Static(
                    "This submits a paid generation request to OpenRouter. The POST is sent only once.",
                    classes="subtitle",
                )
                yield Static(display_payload, id="confirmation-details", markup=False)
                yield CostSummary(id="confirmation-cost")
                if amount is None:
                    yield Static(
                        "Pricing is unavailable for this configuration. Confirm only if you accept an unknown charge.",
                        classes="warning",
                    )
                elif not exact:
                    yield Static("The displayed amount is approximate; final provider cost may differ.", classes="warning")
                with Horizontal(classes="button-row"):
                    yield Button("Go back", id="cancel")
                    yield Button("Generate video", id="confirm", classes="success", variant="success")

    def on_mount(self) -> None:
        self.query_one("#confirmation-cost", CostSummary).show_estimate(self.estimate)
        self.query_one("#confirm", Button).focus()

    @on(Button.Pressed)
    def button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "confirm")

    def action_cancel(self) -> None:
        self.dismiss(False)

    def action_confirm(self) -> None:
        self.dismiss(True)


class PauseMonitoringScreen(ModalScreen[bool]):
    """Explain that leaving does not cancel a remote paid job."""

    BINDINGS = [Binding("escape", "keep", "Keep watching")]

    def compose(self) -> ComposeResult:
        with Middle(classes="modal-backdrop"), Center():
            with Vertical(classes="modal-card"):
                yield Static("Leave the screening room?", classes="hero")
                yield Static(
                    "The remote generation cannot be cancelled here and may still incur its full cost. "
                    "The job is saved in History so you can resume monitoring later.",
                    classes="warning",
                )
                with Horizontal(classes="button-row"):
                    yield Button("Keep watching", id="keep", variant="primary")
                    yield Button("Pause monitoring", id="pause")

    @on(Button.Pressed)
    def button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "pause")

    def action_keep(self) -> None:
        self.dismiss(False)


class ProgressScreen(Screen[None]):
    """Real job state plus an explicitly decorative waiting animation."""

    BINDINGS = [
        Binding("escape", "pause", "Pause monitoring", show=True),
        Binding("q", "pause", "Leave safely", show=True),
    ]

    def __init__(self, *, request: Any | None = None, estimate: Any | None = None, record: Any | None = None) -> None:
        super().__init__()
        self.request = request
        self.estimate = estimate
        self.record = record
        self.job_id: str | None = str(_attr(record, "job_id", "remote_id", default="")) or None
        self._monitor_started = False

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Middle(), Center():
            with Vertical(classes="narrow", id="progress-card"):
                yield Static("Your video is in the clouds", classes="hero")
                yield CloudCinema(id="cinema")
                yield Static("", id="job-meta", markup=False)
                yield Static("", id="progress-error", markup=False)
                with Horizontal(classes="button-row", id="progress-actions"):
                    yield Button("Back to studio", id="back")
                    yield Button("Retry monitoring", id="retry", variant="primary")
        yield Footer()

    def on_mount(self) -> None:
        self.query_one("#progress-actions").display = False
        self.monitor_job()

    @work(exclusive=True, group="generation", exit_on_error=False)
    async def monitor_job(self) -> None:
        self._monitor_started = True
        cinema = self.query_one("#cinema", CloudCinema)
        cinema.elapsed_seconds = 0
        cinema.set_job_state(
            "resuming" if self.record is not None else "submitting",
            "Finding the best seat in the provider queue…",
        )
        self.query_one("#progress-error").display = False
        self.query_one("#progress-actions").display = False
        try:
            outcome = await cast("OpenRouterVideoStudio", self.app).generate_or_resume(
                request=self.request,
                record=self.record,
                update=self.update_job_state,
            )
        except Exception as exc:
            cinema.set_job_state("monitoring paused", "The projector needs your attention.")
            error = self.query_one("#progress-error", Static)
            error.update(str(exc))
            error.display = True
            self.query_one("#progress-actions").display = True
            self.query_one("#retry", Button).display = bool(self.job_id or self.record is not None)
            return

        self.app.switch_screen(CompleteScreen(outcome))

    def update_job_state(
        self,
        status: str,
        *,
        job_id: str | None = None,
        detail: str | None = None,
        countdown: int | None = None,
    ) -> None:
        """Callback used by the API orchestration layer after each real state change."""

        if job_id:
            self.job_id = job_id
        cinema = self.query_one("#cinema", CloudCinema)
        cinema.set_job_state(status, detail, countdown=countdown)
        meta = f"Job {self.job_id}" if self.job_id else "Waiting for a job ID…"
        self.query_one("#job-meta", Static).update(meta)

    @on(Button.Pressed, "#retry")
    def retry_pressed(self) -> None:
        # Once a job exists the app's orchestration always resumes by ID; it never
        # repeats an ambiguous paid POST.
        if not self.job_id and self.record is None:
            self.app.notify(
                "This submission did not return a recoverable job ID. Check OpenRouter before creating another paid job.",
                severity="warning",
                timeout=8,
            )
            return
        if self.job_id and self.record is None:
            self.record = cast("OpenRouterVideoStudio", self.app).history_record(self.job_id) or {
                "job_id": self.job_id,
                "request": self.request,
            }
            self.request = None
        self.monitor_job()

    @on(Button.Pressed, "#back")
    def back_pressed(self) -> None:
        self.app.switch_screen(ComposeScreen(request=self.request))

    def action_pause(self) -> None:
        if not self.job_id:
            self.app.notify("Wait until OpenRouter returns a job ID so the paid request is not left ambiguous.", severity="warning")
            return

        def decided(pause: bool | None) -> None:
            if pause:
                self.workers.cancel_group(self, "generation")
                self.app.switch_screen(ComposeScreen(request=self.request))

        self.app.push_screen(PauseMonitoringScreen(), decided)


class CompleteScreen(Screen[None]):
    """Saved-file confirmation and post-generation hotkeys."""

    BINDINGS = [
        Binding("o", "open", "Open video", show=True),
        Binding("enter", "open", "Open", show=False),
        Binding("n", "new", "New video", show=True),
        Binding("r", "reuse", "Reuse settings", show=True),
        Binding("h", "history", "History", show=True),
        Binding("q", "app.quit", "Quit", show=True),
    ]

    def __init__(self, outcome: Any) -> None:
        super().__init__()
        self.outcome = outcome

    def compose(self) -> ComposeResult:
        path = Path(str(_attr(self.outcome, "path", "output_path")))
        cost = _attr(self.outcome, "cost")
        job = _attr(self.outcome, "job")
        job_id = _attr(job, "id", default=_attr(self.outcome, "job_id", default="unknown"))
        with Middle(), Center():
            with Vertical(classes="narrow"):
                yield Static("🎬  Your video is ready!", classes="hero")
                yield Static(str(path), classes="complete-path", markup=False)
                yield Static(f"Job {job_id}  •  Final cost {format_money(cost)}", classes="subtitle")
                yield Static("Press O or Enter to open it in your default video player.", classes="hint")
                with Horizontal(classes="button-row"):
                    yield Button("New video", id="new")
                    yield Button("Reuse settings", id="reuse")
                    yield Button("Open video", id="open", classes="success", variant="success")
        yield Footer()

    def on_mount(self) -> None:
        self.query_one("#open", Button).focus()

    @on(Button.Pressed)
    def button_pressed(self, event: Button.Pressed) -> None:
        actions: dict[str, Callable[[], None]] = {
            "open": self.action_open,
            "new": self.action_new,
            "reuse": self.action_reuse,
        }
        action = actions.get(str(event.button.id))
        if action:
            action()

    def action_open(self) -> None:
        path = Path(str(_attr(self.outcome, "path", "output_path")))
        try:
            cast("OpenRouterVideoStudio", self.app).open_video(path)
        except Exception as exc:
            self.app.notify(str(exc), title="Could not open video", severity="error")

    def action_new(self) -> None:
        self.app.switch_screen(ComposeScreen())

    def action_reuse(self) -> None:
        request = _attr(self.outcome, "request")
        if request is None:
            self.app.switch_screen(ComposeScreen())
            return
        try:
            from .models import estimate_cost

            model = cast("OpenRouterVideoStudio", self.app).model_for_request(request)
            estimate = estimate_cost(model, request)
        except Exception as exc:
            self.app.notify(str(exc), title="Could not reuse settings", severity="error")
            return

        def confirmed(result: bool | None) -> None:
            if result:
                self.app.switch_screen(ProgressScreen(request=request, estimate=estimate))

        self.app.push_screen(ConfirmationScreen(request, estimate), confirmed)

    def action_history(self) -> None:
        self.app.switch_screen(HistoryScreen())


class ImportJobScreen(ModalScreen[str | None]):
    """Prompt for an existing OpenRouter job ID without submitting anything."""

    BINDINGS = [Binding("escape", "cancel", "Cancel")]

    def compose(self) -> ComposeResult:
        with Middle(classes="modal-backdrop"), Center():
            with Vertical(classes="modal-card"):
                yield Static("Import an existing job", classes="hero")
                yield Static(
                    "Paste a video job ID. The studio will poll it with your current API key; no new generation is submitted.",
                    classes="subtitle",
                )
                yield Input(placeholder="OpenRouter video job ID", id="import-id")
                with Horizontal(classes="button-row"):
                    yield Button("Cancel", id="cancel")
                    yield Button("Import & monitor", id="import", variant="primary")

    def on_mount(self) -> None:
        self.query_one("#import-id", Input).focus()

    @on(Input.Submitted, "#import-id")
    def submitted(self) -> None:
        self._finish()

    @on(Button.Pressed)
    def button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "cancel":
            self.dismiss(None)
        else:
            self._finish()

    def _finish(self) -> None:
        job_id = self.query_one("#import-id", Input).value.strip()
        if not job_id:
            self.app.notify("Enter a job ID.", severity="error")
            return
        self.dismiss(job_id)

    def action_cancel(self) -> None:
        self.dismiss(None)


class HistoryScreen(Screen[None]):
    """Persistent local generation history, recovery, and import controls."""

    BINDINGS = [
        Binding("escape", "back", "Back", show=True),
        Binding("enter", "resume", "Resume / open", show=True),
        Binding("i", "import_job", "Import", show=True),
    ]

    def __init__(self) -> None:
        super().__init__()
        self.records: dict[str, Any] = {}
        self.selected_key: str | None = None

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Vertical(classes="page"):
            yield Static("Generation history", classes="hero")
            yield Static("Pending jobs can be resumed after a restart. No API keys are stored here.", classes="subtitle")
            yield DataTable(cursor_type="row", zebra_stripes=True, id="history-table")
            yield Static("Select a job to see details.", id="history-detail", markup=False)
            with Horizontal(classes="button-row"):
                yield Button("Back", id="back")
                yield Button("Import job ID", id="import")
                yield Button("Resume / Open", id="resume", variant="primary")
        yield Footer()

    def on_mount(self) -> None:
        table = self.query_one("#history-table", DataTable)
        table.add_columns("Created", "Status", "Model", "Prompt", "Cost")
        self.refresh_records()

    def refresh_records(self) -> None:
        table = self.query_one("#history-table", DataTable)
        table.clear()
        self.records.clear()
        records = cast("OpenRouterVideoStudio", self.app).list_history()
        for index, record in enumerate(records):
            key = str(_attr(record, "id", "job_id", "remote_id", default=index))
            self.records[key] = record
            request = _attr(record, "request", default={})
            prompt = str(_attr(request, "prompt", default=_attr(record, "prompt", default="")))
            created = _attr(record, "created_at", "created", default="")
            if isinstance(created, datetime):
                created = created.astimezone().strftime("%Y-%m-%d %H:%M")
            model = _attr(request, "model", default=_attr(record, "model", default=""))
            cost = _attr(record, "cost", "actual_cost")
            table.add_row(
                str(created)[:16],
                _display_status(_attr(record, "status", default="unknown")),
                str(model),
                prompt[:64] + ("…" if len(prompt) > 64 else ""),
                format_money(cost) if cost is not None else "—",
                key=key,
            )
        if not records:
            self.query_one("#history-detail", Static).update("No generations yet. Import a job ID or make your first video.")

    @on(DataTable.RowHighlighted, "#history-table")
    def row_highlighted(self, event: DataTable.RowHighlighted) -> None:
        self.selected_key = str(event.row_key.value)
        record = self.records.get(self.selected_key)
        if record is None:
            return
        request = _attr(record, "request", default={})
        job_id = _attr(record, "job_id", "remote_id", default=_attr(record, "id", default=""))
        output = _attr(record, "output_path", "path", default="Not downloaded")
        error = _attr(record, "error", default="")
        detail = f"Job: {job_id}\nOutput: {output}"
        if error:
            detail += f"\nError: {error}"
        prompt = _attr(request, "prompt", default=_attr(record, "prompt", default=""))
        if prompt:
            detail += f"\nPrompt: {prompt}"
        self.query_one("#history-detail", Static).update(detail)

    @on(Button.Pressed)
    def button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "back":
            self.action_back()
        elif event.button.id == "import":
            self.action_import_job()
        elif event.button.id == "resume":
            self.action_resume()

    def action_back(self) -> None:
        self.app.switch_screen(ComposeScreen())

    def action_import_job(self) -> None:
        def supplied(job_id: str | None) -> None:
            if job_id:
                self.import_and_resume(job_id)

        self.app.push_screen(ImportJobScreen(), supplied)

    @work(exclusive=True, group="import")
    async def import_and_resume(self, job_id: str) -> None:
        try:
            record = await cast("OpenRouterVideoStudio", self.app).import_job(job_id)
        except Exception as exc:
            self.app.notify(str(exc), title="Could not import job", severity="error", timeout=7)
            return
        self.app.switch_screen(ProgressScreen(record=record))

    def action_resume(self) -> None:
        record = self.records.get(self.selected_key or "")
        if record is None:
            self.app.notify("Select a job first.", severity="warning")
            return
        path_value = _attr(record, "output_path", "path")
        if path_value and Path(str(path_value)).is_file():
            try:
                cast("OpenRouterVideoStudio", self.app).open_video(Path(str(path_value)))
            except Exception as exc:
                self.app.notify(str(exc), severity="error")
            return
        status = str(_attr(record, "status", default="")).lower()
        if status in {"failed", "cancelled", "canceled", "expired"}:
            self.app.notify("This remote job is terminal and has no downloadable video.", severity="error")
            return
        self.app.switch_screen(ProgressScreen(record=record))
