"""Application orchestration for the OpenRouter Video Studio TUI."""

from __future__ import annotations

import asyncio
import inspect
import json
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Awaitable, Callable, Iterable, TypeVar, cast

from textual.app import App

from .config import AppPaths, make_output_path
from .models import JobStatus, VideoCatalog, VideoJob, VideoModel, VideoRequest
from .screens import ComposeScreen, OnboardingScreen


T = TypeVar("T")
StatusCallback = Callable[..., None]


async def _maybe_await(value: T | Awaitable[T]) -> T:
    return await value if inspect.isawaitable(value) else cast(T, value)


def _field(value: Any, *names: str, default: Any = None) -> Any:
    for name in names:
        if isinstance(value, dict) and name in value:
            return value[name]
        candidate = getattr(value, name, None)
        if candidate is not None:
            return candidate
    return default


def _status_text(value: Any) -> str:
    return str(getattr(value, "value", value) or "unknown")


@dataclass(frozen=True, slots=True)
class GenerationOutcome:
    """Everything the completion screen needs after a successful download."""

    request: VideoRequest
    job: VideoJob
    path: Path
    record: Any | None = None

    @property
    def cost(self) -> Any:
        return self.job.cost


class OpenRouterVideoStudio(App[None]):
    """A Linux-first full-screen client for OpenRouter video generation."""

    TITLE = "OpenRouter Video Studio"
    SUB_TITLE = "Prompt · Generate · Premiere"
    CSS_PATH = "styles.tcss"
    ENABLE_COMMAND_PALETTE = False

    def __init__(
        self,
        *,
        paths: AppPaths | None = None,
        credential_store: Any | None = None,
        history_store: Any | None = None,
        client_factory: Callable[..., Any] | None = None,
        poll_interval: float = 30.0,
        max_poll_attempts: int = 60,
    ) -> None:
        super().__init__()
        self.paths = (paths or AppPaths.discover()).ensure()

        if credential_store is None:
            from .credentials import CredentialStore

            credential_store = CredentialStore()
        if history_store is None:
            from .history import HistoryStore

            history_store = HistoryStore(self.paths.history_db)
        if client_factory is None:
            from .api import OpenRouterClient

            client_factory = OpenRouterClient

        self.credentials = credential_store
        self.history = history_store
        self.client_factory = client_factory
        self.poll_interval = max(0.05, float(poll_interval))
        self.max_poll_attempts = max(1, int(max_poll_attempts))

        self.api_key: str | None = None
        self.catalog: VideoCatalog | None = None
        self._record_cache: dict[str, Any] = {}
        self._settings_path = self.paths.config_dir / "model-settings.json"
        self._model_settings = self._load_model_settings()

    async def on_mount(self) -> None:
        await _maybe_await(self.history.initialize())
        key = await _maybe_await(self.credentials.get())
        self.api_key = self._extract_key(key)
        if self.api_key:
            self.push_screen(ComposeScreen())
        else:
            self.push_screen(OnboardingScreen())

    async def on_unmount(self) -> None:
        close = getattr(self.history, "close", None)
        if close is not None:
            await _maybe_await(close())

    @staticmethod
    def _extract_key(value: Any) -> str | None:
        if isinstance(value, str):
            return value.strip() or None
        candidate = _field(value, "key", "value", "secret")
        return candidate.strip() if isinstance(candidate, str) and candidate.strip() else None

    def _client(self) -> Any:
        if not self.api_key:
            raise RuntimeError("No OpenRouter API key is connected.")
        return self.client_factory(self.api_key)

    async def connect_api_key(self, api_key: str) -> str:
        """Validate before persisting, then report the selected key storage mode."""

        candidate = api_key.strip()
        if not candidate:
            raise ValueError("Enter an OpenRouter API key.")
        async with self.client_factory(candidate) as client:
            await client.validate_key()

        await _maybe_await(self.credentials.set(candidate))
        self.api_key = candidate
        status_method = getattr(self.credentials, "status", None)
        status = await _maybe_await(status_method()) if callable(status_method) else None
        persistent = bool(_field(status, "persistent", "available", default=False))
        backend = _field(status, "backend", "name", default="system keyring")
        if persistent:
            return f"Saved securely in {backend}."
        return "No usable system keyring was found; the key will be kept in memory for this run only."

    def forget_api_key(self) -> None:
        """Forget the current key locally; this does not revoke it on OpenRouter."""

        self.api_key = None
        result = self.credentials.delete()
        if inspect.isawaitable(result):
            # Credential stores are normally synchronous, but injected test or
            # platform implementations may be async.
            self.run_worker(result, group="credential-delete", exclusive=True)

    async def get_video_catalog(self) -> tuple[VideoCatalog, bool]:
        """Fetch the live catalog, falling back to a visibly stale local copy."""

        try:
            async with self._client() as client:
                catalog = await client.list_video_models()
            catalog.save(self.paths.catalog_cache)
            self.catalog = catalog
            return catalog, False
        except Exception as live_error:
            try:
                catalog = VideoCatalog.load(self.paths.catalog_cache)
            except (OSError, ValueError, json.JSONDecodeError):
                raise live_error
            self.catalog = catalog
            return catalog, True

    def preferred_model_id(self, models: dict[str, Any]) -> str:
        preferred = self.catalog.preferred() if self.catalog else None
        if preferred and preferred.id in models:
            return preferred.id
        return next(iter(models))

    def model_for_request(self, request: VideoRequest) -> VideoModel:
        if self.catalog:
            model = self.catalog.find(request.model)
            if model:
                return model
        raise RuntimeError("Refresh the video model catalog before reusing this request.")

    def settings_for_model(self, model_id: str) -> dict[str, Any]:
        value = self._model_settings.get(model_id, {})
        return dict(value) if isinstance(value, dict) else {}

    def remember_request(self, request: VideoRequest) -> None:
        """Persist only non-secret, primitive controls for the selected model."""

        self._model_settings[request.model] = {
            "duration": request.duration,
            "resolution": request.resolution,
            "aspect_ratio": request.aspect_ratio,
            "size": request.size,
            "generate_audio": request.generate_audio,
            "seed": request.seed,
        }
        temporary = self._settings_path.with_name(self._settings_path.name + ".tmp")
        try:
            temporary.write_text(
                json.dumps(self._model_settings, ensure_ascii=False, indent=2),
                encoding="utf-8",
            )
            temporary.replace(self._settings_path)
        except OSError:
            temporary.unlink(missing_ok=True)
            # Preferences are optional; generation should still proceed if a
            # read-only or full config directory prevents saving them.

    def _load_model_settings(self) -> dict[str, dict[str, Any]]:
        try:
            payload = json.loads(self._settings_path.read_text(encoding="utf-8"))
        except (OSError, ValueError, json.JSONDecodeError):
            return {}
        if not isinstance(payload, dict):
            return {}
        return {
            str(model_id): dict(settings)
            for model_id, settings in payload.items()
            if isinstance(settings, dict)
        }

    async def generate_or_resume(
        self,
        *,
        request: VideoRequest | None,
        record: Any | None,
        update: StatusCallback,
    ) -> GenerationOutcome:
        """Submit at most once, monitor a remote job, and atomically download it."""

        if record is None and request is None:
            raise ValueError("A request or existing history record is required.")

        actual_request = request
        async with self._client() as client:
            if record is None:
                assert actual_request is not None
                update("submitting", detail="Sending one paid request to OpenRouter…")
                # Intentionally no UI-layer retry around this POST. If it returns
                # ambiguously, the API client raises and the user is told to inspect
                # OpenRouter before starting another generation.
                job = await client.submit(actual_request)
                # Surface the recoverable identifier before touching local storage.
                # Even if SQLite fails, the user can copy/import this job rather than
                # risk repeating the paid POST.
                update(
                    _status_text(job.status),
                    job_id=job.id,
                    detail="OpenRouter accepted the job; saving it to local history…",
                )
                record = await _maybe_await(self.history.create_job(actual_request, job))
                self._cache_record(record, job.id)
            else:
                job = await self._poll_record(client, record)
                if actual_request is None:
                    actual_request = self._request_from_record(record, job=job)

            self._cache_record(record, job.id)
            update(
                _status_text(job.status),
                job_id=job.id,
                detail=self._detail_for_job(job),
            )
            record = await self._history_update(job, record=record)

            attempts = 0
            while not job.terminal and attempts < self.max_poll_attempts:
                await self._poll_countdown(update, job)
                attempts += 1
                job = await client.poll(job.polling_url or job.id)
                update(
                    _status_text(job.status),
                    job_id=job.id,
                    detail=self._detail_for_job(job),
                )
                record = await self._history_update(job, record=record)

            if not job.terminal:
                raise RuntimeError(
                    "Monitoring reached its local limit. The remote job was not cancelled; "
                    "open History later to resume checking it."
                )
            if not job.successful:
                reason = job.error or f"OpenRouter marked the job {_status_text(job.status)}."
                raise RuntimeError(reason)

            assert actual_request is not None
            download_url = job.unsigned_urls[0] if job.unsigned_urls else client.content_url(job.id)
            destination = make_output_path(
                actual_request.prompt,
                job.id,
                videos_dir=self.paths.videos_dir,
            )
            update("downloading", job_id=job.id, detail=f"Saving to {destination.name}…")

            def download_progress(received: int, total: int | None = None) -> None:
                if total and total > 0:
                    detail = f"Downloaded {received / 1_048_576:.1f} / {total / 1_048_576:.1f} MiB…"
                else:
                    detail = f"Downloaded {received / 1_048_576:.1f} MiB…"
                update("downloading", job_id=job.id, detail=detail)

            # The client decides whether Authorization is safe for this URL. It
            # must never forward credentials to unsigned provider/CDN hosts.
            saved = await client.download(download_url, destination, progress=download_progress)
            record = await self._history_update(job, record=record, output_path=saved)
            update("completed", job_id=job.id, detail="Saved. House lights are coming up!")
            return GenerationOutcome(actual_request, job, Path(saved), record)

    async def _poll_countdown(self, update: StatusCallback, job: VideoJob) -> None:
        remaining = self.poll_interval
        while remaining > 0:
            shown = max(0, int(remaining + 0.999))
            update(
                _status_text(job.status),
                job_id=job.id,
                detail=self._detail_for_job(job),
                countdown=shown,
            )
            step = min(1.0, remaining)
            await asyncio.sleep(step)
            remaining -= step

    async def _poll_record(self, client: Any, record: Any) -> VideoJob:
        polling = _field(record, "polling_url")
        job_id = _field(record, "job_id", "remote_id")
        if not polling and not job_id:
            existing_job = _field(record, "job")
            polling = _field(existing_job, "polling_url", "id")
        target = polling or job_id
        if not target:
            raise RuntimeError("This history entry has no OpenRouter polling URL or job ID.")
        return await client.poll(str(target))

    @staticmethod
    def _request_from_record(record: Any, *, job: VideoJob | None = None) -> VideoRequest:
        value = _field(record, "request")
        if isinstance(value, VideoRequest):
            return value
        if isinstance(value, dict):
            return VideoRequest.from_payload(value)
        payload = _field(record, "request_payload", "request_json")
        if isinstance(payload, str):
            payload = json.loads(payload)
        if isinstance(payload, dict):
            return VideoRequest.from_payload(payload)
        # Imported jobs may predate local history. The status payload often carries
        # enough context for a useful filename; otherwise use an explicit placeholder
        # without ever submitting it as a new generation request.
        raw = dict(job.raw) if job is not None else {}
        return VideoRequest(
            model=str(raw.get("model") or "imported-openrouter-job"),
            prompt=str(raw.get("prompt") or "Imported OpenRouter video"),
        )

    @staticmethod
    def _detail_for_job(job: VideoJob) -> str:
        status = _status_text(job.status)
        if status == "pending":
            return "Waiting in the provider queue…"
        if status == "in_progress":
            return "The model is rendering your frames…"
        if status == "completed":
            return "Rendering finished; preparing the download…"
        if status in {"failed", "cancelled", "expired"}:
            return job.error or f"The job ended with status {status}."
        return f"OpenRouter reports: {status}."

    async def _history_update(self, job: VideoJob, *, record: Any, output_path: Path | None = None) -> Any:
        kwargs: dict[str, Any] = {}
        if output_path is not None:
            kwargs["output_path"] = output_path
        try:
            updated = await _maybe_await(self.history.update_job(job, **kwargs))
        except TypeError:
            # Support stores that key updates by record rather than job.
            updated = await _maybe_await(self.history.update_job(record, job, **kwargs))
        resolved = updated if updated is not None else record
        self._cache_record(resolved, job.id)
        return resolved

    def _cache_record(self, record: Any, job_id: str | None = None) -> None:
        if record is None:
            return
        key = str(job_id or _field(record, "job_id", "remote_id", "id", default=""))
        if key:
            self._record_cache[key] = record

    def history_record(self, job_id: str) -> Any | None:
        if job_id in self._record_cache:
            return self._record_cache[job_id]
        getter = getattr(self.history, "get", None)
        if getter is not None:
            try:
                record = getter(job_id)
                if not inspect.isawaitable(record):
                    self._cache_record(record, job_id)
                    return record
            except Exception:
                pass
        for record in self.list_history():
            if str(_field(record, "job_id", "remote_id", default="")) == job_id:
                self._cache_record(record, job_id)
                return record
        return None

    def list_history(self) -> list[Any]:
        records = self.history.list()
        if inspect.isawaitable(records):
            # The bundled store is synchronous; async injected stores should expose
            # a cached `records` collection for the synchronous table render pass.
            records = getattr(self.history, "records", ())
        result = list(records or ())
        for record in result:
            self._cache_record(record)
        return result

    async def import_job(self, job_id: str) -> Any:
        """Fetch and persist an existing job without issuing a generation POST."""

        async with self._client() as client:
            job = await client.poll(job_id)
        importer = getattr(self.history, "import_job")
        try:
            record = await _maybe_await(importer(job))
        except TypeError:
            record = await _maybe_await(importer(job_id, job))
        self._cache_record(record, job.id or job_id)
        return record

    @staticmethod
    def open_video(path: Path) -> None:
        resolved = path.expanduser().resolve()
        if not resolved.is_file():
            raise FileNotFoundError(f"Video file no longer exists: {resolved}")
        opener = shutil.which("xdg-open")
        if opener is None:
            raise RuntimeError("xdg-open is not installed; open the saved path in your file manager.")
        try:
            subprocess.Popen(
                [opener, str(resolved)],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
                close_fds=True,
            )
        except OSError as exc:
            raise RuntimeError(f"Could not start the default video player: {exc}") from exc


def main() -> None:
    OpenRouterVideoStudio().run()
