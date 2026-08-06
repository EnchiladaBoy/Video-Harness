from __future__ import annotations

from datetime import datetime, timezone
from decimal import Decimal

import pytest

from openrouter_video_studio.models import (
    FrameImage,
    InputReference,
    JobStatus,
    VideoCatalog,
    VideoJob,
    VideoModel,
    VideoRequest,
    estimate_cost,
)


def model_payload(**overrides: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "id": "example/video-one",
        "name": "Video One",
        "supported_resolutions": ["720p", "1080p"],
        "supported_aspect_ratios": ["16:9", "9:16"],
        "supported_durations": [4, "8", "invalid"],
        "supported_frame_images": ["first_frame"],
        "generate_audio": False,
        "seed": True,
        "allowed_passthrough_parameters": ["provider.foo"],
        "pricing_skus": {"cents_per_second_output_720p": "17", "broken": "nan"},
    }
    payload.update(overrides)
    return payload


def test_model_catalog_maps_capabilities_and_pricing() -> None:
    model = VideoModel.from_api(model_payload())

    assert model.id == "example/video-one"
    assert model.supported_durations == (4, 8)
    assert model.supported_resolutions == ("720p", "1080p")
    assert model.generate_audio is False
    assert model.seed is True
    assert model.pricing_skus == {
        "cents_per_second_output_720p": Decimal("17")
    }


def test_request_payload_omits_unset_fields_and_copies_provider_data() -> None:
    provider = {"routing": {"only": ["example"]}}
    request = VideoRequest(
        model=" example/video-one ",
        prompt=" drifting clouds ",
        duration=4,
        resolution="720p",
        aspect_ratio="16:9",
        generate_audio=False,
        seed=42,
        frame_images=(FrameImage("https://images.example/first.png"),),
        input_references=(InputReference("https://images.example/style.png"),),
        provider=provider,
    )

    payload = request.to_payload()
    provider["routing"]["only"].append("changed")

    assert payload == {
        "model": "example/video-one",
        "prompt": "drifting clouds",
        "duration": 4,
        "resolution": "720p",
        "aspect_ratio": "16:9",
        "generate_audio": False,
        "seed": 42,
        "frame_images": [
            {
                "type": "image_url",
                "image_url": {"url": "https://images.example/first.png"},
                "frame_type": "first_frame",
            }
        ],
        "input_references": [
            {
                "type": "image_url",
                "image_url": {"url": "https://images.example/style.png"},
            }
        ],
        "provider": {"routing": {"only": ["example"]}},
    }


@pytest.mark.parametrize(
    "url",
    [
        "http://images.example/frame.png",
        "file:///tmp/frame.png",
        "https://user:password@images.example/frame.png",
        "not-a-url",
    ],
)
def test_reference_images_require_public_credential_free_https_urls(url: str) -> None:
    with pytest.raises(ValueError, match="HTTPS|credentials"):
        InputReference(url)


def test_model_reports_all_incompatible_request_settings() -> None:
    model = VideoModel.from_api(model_payload(seed=False))
    request = VideoRequest(
        model=model.id,
        prompt="test",
        duration=5,
        resolution="4k",
        aspect_ratio="1:1",
        generate_audio=True,
        seed=7,
        frame_images=(
            FrameImage("https://images.example/last.png", frame_type="last_frame"),
        ),
    )

    problems = model.supports_request(request)

    assert len(problems) == 6
    assert any("resolution" in problem for problem in problems)
    assert any("audio" in problem for problem in problems)
    assert any("last_frame" in problem for problem in problems)


def test_cost_estimate_converts_live_cents_per_output_second_price() -> None:
    model = VideoModel.from_api(model_payload())
    request = VideoRequest(
        model=model.id,
        prompt="test",
        duration=5,
        resolution="720p",
    )

    estimate = estimate_cost(model, request)

    assert estimate.amount == Decimal("0.85")
    assert estimate.exact is True
    assert estimate.pricing_sku == "cents_per_second_output_720p"
    assert estimate.unit_price == Decimal("0.17")


@pytest.mark.parametrize(
    ("pricing", "audio", "expected_sku", "expected"),
    [
        (
            {"duration_seconds_720p": "0.12"},
            None,
            "duration_seconds_720p",
            Decimal("0.60"),
        ),
        (
            {"duration_seconds_with_audio": "0.25"},
            True,
            "duration_seconds_with_audio",
            Decimal("1.25"),
        ),
        (
            {"duration_seconds_without_audio": "0.10"},
            False,
            "duration_seconds_without_audio",
            Decimal("0.50"),
        ),
    ],
)
def test_cost_estimate_supports_live_dollar_per_second_skus(
    pricing: dict[str, str],
    audio: bool | None,
    expected_sku: str,
    expected: Decimal,
) -> None:
    model = VideoModel.from_api(model_payload(pricing_skus=pricing, generate_audio=None))
    request = VideoRequest(
        model=model.id,
        prompt="test",
        duration=5,
        resolution="720p",
        generate_audio=audio,
    )

    estimate = estimate_cost(model, request)

    assert estimate.amount == expected
    assert estimate.pricing_sku == expected_sku


def test_text_to_video_price_is_not_used_for_reference_generation() -> None:
    model = VideoModel.from_api(
        model_payload(pricing_skus={"text_to_video_duration_seconds_720p": "0.20"})
    )
    text_request = VideoRequest(
        model=model.id,
        prompt="test",
        duration=4,
        resolution="720p",
    )
    reference_request = VideoRequest(
        model=model.id,
        prompt="test",
        duration=4,
        resolution="720p",
        input_references=(InputReference("https://images.example/style.png"),),
    )

    assert estimate_cost(model, text_request).amount == Decimal("0.80")
    assert estimate_cost(model, reference_request).amount is None


def test_video_token_pricing_remains_explicitly_unknown() -> None:
    model = VideoModel.from_api(model_payload(pricing_skus={"video_tokens": "0.0001"}))
    request = VideoRequest(model=model.id, prompt="test", duration=4)

    estimate = estimate_cost(model, request)

    assert estimate.amount is None
    assert estimate.raw_pricing == {"video_tokens": Decimal("0.0001")}


def test_cost_estimate_refuses_to_guess_unknown_pricing_units() -> None:
    model = VideoModel.from_api(
        model_payload(pricing_skus={"pixels": "0.0000001", "mystery": "2"})
    )
    request = VideoRequest(model=model.id, prompt="test", duration=4)

    estimate = estimate_cost(model, request)

    assert estimate.amount is None
    assert estimate.available is False
    assert estimate.raw_pricing == {
        "pixels": Decimal("0.0000001"),
        "mystery": Decimal("2"),
    }


def test_catalog_cache_round_trip_is_marked_stale(tmp_path) -> None:
    fetched_at = datetime(2026, 8, 6, 3, 4, 5, tzinfo=timezone.utc)
    catalog = VideoCatalog.from_api(
        {"data": [model_payload()]}, fetched_at=fetched_at
    )
    cache = tmp_path / "nested" / "models.json"

    catalog.save(cache)
    restored = VideoCatalog.load(cache)

    assert restored.stale is True
    assert restored.fetched_at == fetched_at
    assert restored.find("example/video-one") is not None


@pytest.mark.parametrize(
    ("raw_status", "expected", "terminal"),
    [
        ("pending", JobStatus.PENDING, False),
        ("in_progress", JobStatus.IN_PROGRESS, False),
        ("completed", JobStatus.COMPLETED, True),
        ("provider-specific", JobStatus.UNKNOWN, False),
    ],
)
def test_job_status_and_cost_mapping(
    raw_status: str, expected: JobStatus, terminal: bool
) -> None:
    job = VideoJob.from_api(
        {
            "id": "job-1",
            "status": raw_status,
            "polling_url": "/api/v1/videos/job-1",
            "usage": {"cost": "0.85"},
        }
    )

    assert job.status is expected
    assert job.terminal is terminal
    assert job.cost == Decimal("0.85")


def test_video_request_rejects_conflicting_dimensions() -> None:
    with pytest.raises(ValueError, match="cannot be combined"):
        VideoRequest(
            model="example/video-one",
            prompt="test",
            size="1280x720",
            resolution="720p",
        )
