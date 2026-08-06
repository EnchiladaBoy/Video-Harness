from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest
from textual.widgets import Input, Select, Static, TextArea

from openrouter_video_studio.app import GenerationOutcome, OpenRouterVideoStudio
from openrouter_video_studio.config import AppPaths
from openrouter_video_studio.models import (
    JobStatus,
    VideoCatalog,
    VideoJob,
    VideoModel,
    VideoRequest,
)
from openrouter_video_studio.screens import (
    CompleteScreen,
    ComposeScreen,
    ConfirmationScreen,
    OnboardingScreen,
    ProgressScreen,
)
from openrouter_video_studio.widgets import CloudCinema


def sample_model() -> VideoModel:
    return VideoModel.from_api(
        {
            "id": "example/video-one",
            "name": "Video One",
            "description": "A test-only video model",
            "supported_durations": [4, 8],
            "supported_resolutions": ["720p", "1080p"],
            "supported_aspect_ratios": ["16:9", "9:16"],
            "supported_sizes": ["1280x720"],
            "supported_frame_images": ["first_frame", "last_frame"],
            "generate_audio": True,
            "seed": True,
            "allowed_passthrough_parameters": ["guidance"],
            "pricing_skus": {"cents_per_second_output_720p": "17"},
        }
    )


class FakeCredentials:
    def __init__(self, value: str | None = None) -> None:
        self.value = value
        self.saved: list[str] = []

    def get(self) -> str | None:
        return self.value

    def set(self, value: str) -> bool:
        self.value = value
        self.saved.append(value)
        return False

    def delete(self) -> bool:
        self.value = None
        return False

    def status(self) -> SimpleNamespace:
        return SimpleNamespace(persistent=False, backend="memory")


class FakeHistory:
    def __init__(self) -> None:
        self.records: list[Any] = []

    def initialize(self) -> None:
        return None

    def close(self) -> None:
        return None

    def list(self) -> list[Any]:
        return list(self.records)


class FakeClient:
    validated_keys: list[str] = []
    wait_forever = False

    def __init__(self, api_key: str) -> None:
        self.api_key = api_key

    async def __aenter__(self) -> "FakeClient":
        return self

    async def __aexit__(self, *_: object) -> None:
        return None

    async def validate_key(self) -> None:
        self.validated_keys.append(self.api_key)

    async def list_video_models(self) -> VideoCatalog:
        return VideoCatalog((sample_model(),))

    async def submit(self, request: VideoRequest) -> VideoJob:
        if self.wait_forever:
            await asyncio.Event().wait()
        raise AssertionError("A pilot test unexpectedly attempted submission")


def app_paths(tmp_path: Path) -> AppPaths:
    return AppPaths(
        data_dir=tmp_path / "data",
        cache_dir=tmp_path / "cache",
        config_dir=tmp_path / "config",
        videos_dir=tmp_path / "Videos",
    )


def make_app(
    tmp_path: Path,
    *,
    key: str | None = "sk-test-saved",
    client_factory: Any = FakeClient,
) -> OpenRouterVideoStudio:
    return OpenRouterVideoStudio(
        paths=app_paths(tmp_path),
        credential_store=FakeCredentials(key),
        history_store=FakeHistory(),
        client_factory=client_factory,
        poll_interval=0.05,
        max_poll_attempts=1,
    )


@asynccontextmanager
async def running_app(app: OpenRouterVideoStudio):
    """Run a Textual app and explicitly stop its persistent screen stack."""

    async with app.run_test() as pilot:
        try:
            yield pilot
        finally:
            app.exit()


@pytest.mark.asyncio
async def test_onboarding_masks_key_then_validation_opens_compose(tmp_path: Path) -> None:
    FakeClient.validated_keys.clear()
    credentials = FakeCredentials()
    app = OpenRouterVideoStudio(
        paths=app_paths(tmp_path),
        credential_store=credentials,
        history_store=FakeHistory(),
        client_factory=FakeClient,
    )

    async with running_app(app) as pilot:
        await pilot.pause()
        assert isinstance(app.screen, OnboardingScreen)
        key_input = app.screen.query_one("#onboarding-key", Input)
        assert key_input.password is True

        key_input.value = "sk-test-new"
        await pilot.press("enter")
        await pilot.pause(0.5)

        assert isinstance(app.screen, ComposeScreen)
        assert FakeClient.validated_keys == ["sk-test-new"]
        assert credentials.saved == ["sk-test-new"]
        assert key_input.value == ""


@pytest.mark.asyncio
async def test_compose_applies_capabilities_and_ctrl_enter_opens_confirmation(
    tmp_path: Path,
) -> None:
    app = make_app(tmp_path)

    async with running_app(app) as pilot:
        await pilot.pause(0.2)
        assert isinstance(app.screen, ComposeScreen)
        screen = app.screen
        assert screen.selected_model is not None

        duration = screen.query_one("#duration", Select)
        resolution = screen.query_one("#resolution", Select)
        aspect = screen.query_one("#aspect", Select)
        size = screen.query_one("#size", Select)
        assert duration.value == "4"
        assert resolution.value == "720p"
        assert aspect.value == "16:9"
        assert not isinstance(size.value, str)

        size.value = "1280x720"
        await pilot.pause()
        assert not isinstance(resolution.value, str)
        assert not isinstance(aspect.value, str)

        resolution.value = "720p"
        await pilot.pause()
        assert not isinstance(size.value, str)

        screen.query_one("#prompt", TextArea).text = "A tiny cinema drifting in clouds"
        await pilot.pause()
        await pilot.press("ctrl+enter")
        await pilot.pause()

        assert isinstance(app.screen, ConfirmationScreen)


@pytest.mark.asyncio
async def test_compose_stacks_controls_in_a_narrow_terminal(tmp_path: Path) -> None:
    app = make_app(tmp_path)

    async with app.run_test() as pilot:
        await pilot.pause(0.2)
        assert isinstance(app.screen, ComposeScreen)

        await pilot.resize_terminal(70, 24)
        await pilot.pause()

        assert app.screen.query_one("#compose-grid").has_class("compact")
        assert app.screen.has_class("short")


@pytest.mark.asyncio
async def test_progress_shows_real_status_while_cloud_cinema_animates(
    tmp_path: Path,
) -> None:
    class WaitingClient(FakeClient):
        wait_forever = True

    app = make_app(tmp_path, client_factory=WaitingClient)
    request = VideoRequest(model=sample_model().id, prompt="test", duration=4)

    async with running_app(app) as pilot:
        await pilot.pause(0.2)
        progress = ProgressScreen(request=request)
        app.switch_screen(progress)
        await pilot.pause()

        cinema = progress.query_one("#cinema", CloudCinema)
        starting_phase = cinema.phase
        progress.update_job_state(
            "in_progress",
            job_id="job-real-123",
            detail="The provider is rendering frames",
            countdown=17,
        )
        await pilot.pause(0.3)

        assert cinema.phase > starting_phase
        assert cinema.status == "In Progress"
        assert cinema.detail == "The provider is rendering frames"
        assert cinema.countdown == 17
        assert "job-real-123" in str(progress.query_one("#job-meta", Static).render())


@pytest.mark.asyncio
async def test_complete_hotkeys_open_video_and_start_new_prompt(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    opened: list[Path] = []
    monkeypatch.setattr(
        OpenRouterVideoStudio,
        "open_video",
        staticmethod(lambda path: opened.append(path)),
    )
    app = make_app(tmp_path)
    request = VideoRequest(model=sample_model().id, prompt="test", duration=4)
    job = VideoJob(
        id="job-complete",
        status=JobStatus.COMPLETED,
        polling_url="/api/v1/videos/job-complete",
        usage={"cost": "0.68"},
    )
    video_path = tmp_path / "Videos" / "finished.mp4"
    outcome = GenerationOutcome(request=request, job=job, path=video_path)

    async with running_app(app) as pilot:
        await pilot.pause(0.2)
        app.switch_screen(CompleteScreen(outcome))
        await pilot.pause()

        await pilot.press("o")
        await pilot.pause()
        assert opened == [video_path]

        await pilot.press("n")
        await pilot.pause()
        assert isinstance(app.screen, ComposeScreen)
