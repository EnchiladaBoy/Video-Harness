"""Typed request, model-catalog, job, and cost objects."""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
from enum import Enum
from pathlib import Path
from types import MappingProxyType
from typing import Any, Mapping, Sequence
from urllib.parse import urlsplit


JsonMapping = Mapping[str, Any]


def _tuple_of_str(value: Any) -> tuple[str, ...]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes, bytearray)):
        return ()
    return tuple(str(item) for item in value if item is not None)


def _tuple_of_int(value: Any) -> tuple[int, ...]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes, bytearray)):
        return ()
    result: list[int] = []
    for item in value:
        try:
            result.append(int(item))
        except (TypeError, ValueError):
            continue
    return tuple(result)


def _decimal(value: Any) -> Decimal | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        result = Decimal(str(value))
    except (InvalidOperation, TypeError, ValueError):
        return None
    return result if result.is_finite() else None


def _validate_https_url(url: str, label: str) -> str:
    parsed = urlsplit(url)
    if parsed.scheme.lower() != "https" or not parsed.hostname:
        raise ValueError(f"{label} must be a public HTTPS URL")
    if parsed.username or parsed.password:
        raise ValueError(f"{label} must not contain embedded credentials")
    return url


@dataclass(frozen=True, slots=True)
class FrameImage:
    url: str
    frame_type: str = "first_frame"

    def __post_init__(self) -> None:
        _validate_https_url(self.url, "Frame image")
        if self.frame_type not in {"first_frame", "last_frame"}:
            raise ValueError("frame_type must be first_frame or last_frame")

    def to_payload(self) -> dict[str, Any]:
        return {
            "type": "image_url",
            "image_url": {"url": self.url},
            "frame_type": self.frame_type,
        }


@dataclass(frozen=True, slots=True)
class InputReference:
    """A provider-accessible reference image for reference-to-video."""

    url: str

    def __post_init__(self) -> None:
        _validate_https_url(self.url, "Input reference")

    def to_payload(self) -> dict[str, Any]:
        return {"type": "image_url", "image_url": {"url": self.url}}


@dataclass(frozen=True, slots=True)
class VideoRequest:
    model: str
    prompt: str
    duration: int | None = None
    resolution: str | None = None
    aspect_ratio: str | None = None
    size: str | None = None
    generate_audio: bool | None = None
    seed: int | None = None
    frame_images: tuple[FrameImage, ...] = ()
    input_references: tuple[InputReference, ...] = ()
    provider: Mapping[str, Any] | None = None

    def __post_init__(self) -> None:
        if not self.model.strip():
            raise ValueError("model is required")
        if not self.prompt.strip():
            raise ValueError("prompt is required")
        if self.duration is not None:
            if isinstance(self.duration, bool) or not isinstance(self.duration, int):
                raise ValueError("duration must be an integer number of seconds")
            if self.duration < 1:
                raise ValueError("duration must be at least 1 second")
        if self.size is not None and not re.fullmatch(r"[1-9]\d*x[1-9]\d*", self.size):
            raise ValueError("size must use WIDTHxHEIGHT, for example 1280x720")
        if self.size is not None and (self.resolution is not None or self.aspect_ratio is not None):
            raise ValueError("size cannot be combined with resolution or aspect_ratio")
        if self.seed is not None and (
            isinstance(self.seed, bool) or not isinstance(self.seed, int)
        ):
            raise ValueError("seed must be an integer")
        if self.provider is not None and not isinstance(self.provider, Mapping):
            raise ValueError("provider must be a JSON object")

    def to_payload(self) -> dict[str, Any]:
        payload: dict[str, Any] = {"model": self.model.strip(), "prompt": self.prompt.strip()}
        optional = {
            "duration": self.duration,
            "resolution": self.resolution,
            "aspect_ratio": self.aspect_ratio,
            "size": self.size,
            "generate_audio": self.generate_audio,
            "seed": self.seed,
        }
        payload.update({key: value for key, value in optional.items() if value is not None})
        if self.frame_images:
            payload["frame_images"] = [item.to_payload() for item in self.frame_images]
        if self.input_references:
            payload["input_references"] = [item.to_payload() for item in self.input_references]
        if self.provider is not None:
            # JSON round-tripping prevents callers from mutating nested request data later.
            payload["provider"] = json.loads(json.dumps(self.provider))
        return payload

    @classmethod
    def from_payload(cls, payload: JsonMapping) -> "VideoRequest":
        frames = tuple(
            FrameImage(
                url=str(item.get("image_url", {}).get("url", "")),
                frame_type=str(item.get("frame_type", "first_frame")),
            )
            for item in payload.get("frame_images", ())
            if isinstance(item, Mapping)
        )
        references = tuple(
            InputReference(url=str(item.get("image_url", {}).get("url", "")))
            for item in payload.get("input_references", ())
            if isinstance(item, Mapping)
        )
        return cls(
            model=str(payload.get("model", "")),
            prompt=str(payload.get("prompt", "")),
            duration=payload.get("duration"),
            resolution=payload.get("resolution"),
            aspect_ratio=payload.get("aspect_ratio"),
            size=payload.get("size"),
            generate_audio=payload.get("generate_audio"),
            seed=payload.get("seed"),
            frame_images=frames,
            input_references=references,
            provider=payload.get("provider"),
        )


@dataclass(frozen=True, slots=True)
class VideoModel:
    id: str
    name: str
    description: str = ""
    canonical_slug: str | None = None
    created: int | None = None
    supported_resolutions: tuple[str, ...] = ()
    supported_aspect_ratios: tuple[str, ...] = ()
    supported_sizes: tuple[str, ...] = ()
    supported_durations: tuple[int, ...] = ()
    supported_frame_images: tuple[str, ...] = ()
    generate_audio: bool | None = None
    seed: bool | None = None
    allowed_passthrough_parameters: tuple[str, ...] = ()
    pricing_skus: Mapping[str, Decimal] = field(default_factory=dict)
    raw: Mapping[str, Any] = field(default_factory=dict, repr=False, compare=False)

    @classmethod
    def from_api(cls, data: JsonMapping) -> "VideoModel":
        prices: dict[str, Decimal] = {}
        raw_prices = data.get("pricing_skus")
        if isinstance(raw_prices, Mapping):
            for key, value in raw_prices.items():
                parsed = _decimal(value)
                if parsed is not None:
                    prices[str(key)] = parsed
        seed_value = data.get("seed")
        return cls(
            id=str(data.get("id", "")),
            name=str(data.get("name") or data.get("id") or "Unknown model"),
            description=str(data.get("description") or ""),
            canonical_slug=str(data["canonical_slug"]) if data.get("canonical_slug") else None,
            created=int(data["created"]) if data.get("created") is not None else None,
            supported_resolutions=_tuple_of_str(data.get("supported_resolutions")),
            supported_aspect_ratios=_tuple_of_str(data.get("supported_aspect_ratios")),
            supported_sizes=_tuple_of_str(data.get("supported_sizes")),
            supported_durations=_tuple_of_int(data.get("supported_durations")),
            supported_frame_images=_tuple_of_str(data.get("supported_frame_images")),
            generate_audio=data.get("generate_audio") if isinstance(data.get("generate_audio"), bool) else None,
            seed=seed_value if isinstance(seed_value, bool) else None,
            allowed_passthrough_parameters=_tuple_of_str(data.get("allowed_passthrough_parameters")),
            pricing_skus=MappingProxyType(prices),
            raw=MappingProxyType(dict(data)),
        )

    def supports_request(self, request: VideoRequest) -> tuple[str, ...]:
        """Return human-readable capability mismatches (empty means compatible)."""

        problems: list[str] = []
        if request.model != self.id:
            problems.append(f"request model is {request.model}, expected {self.id}")
        if request.resolution and self.supported_resolutions and request.resolution not in self.supported_resolutions:
            problems.append(f"resolution {request.resolution} is not supported")
        if request.aspect_ratio and self.supported_aspect_ratios and request.aspect_ratio not in self.supported_aspect_ratios:
            problems.append(f"aspect ratio {request.aspect_ratio} is not supported")
        if request.size and self.supported_sizes and request.size not in self.supported_sizes:
            problems.append(f"size {request.size} is not supported")
        if request.duration is not None and self.supported_durations and request.duration not in self.supported_durations:
            problems.append(f"duration {request.duration}s is not supported")
        if request.generate_audio is True and self.generate_audio is False:
            problems.append("audio generation is not supported")
        if request.seed is not None and self.seed is False:
            problems.append("seeded generation is not supported")
        for frame in request.frame_images:
            if self.supported_frame_images and frame.frame_type not in self.supported_frame_images:
                problems.append(f"{frame.frame_type} is not supported")
        return tuple(problems)


@dataclass(frozen=True, slots=True)
class VideoCatalog:
    models: tuple[VideoModel, ...]
    fetched_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))
    stale: bool = False

    @classmethod
    def from_api(
        cls,
        data: JsonMapping,
        *,
        fetched_at: datetime | None = None,
        stale: bool = False,
    ) -> "VideoCatalog":
        raw_models = data.get("data")
        if not isinstance(raw_models, Sequence) or isinstance(raw_models, (str, bytes, bytearray)):
            raise ValueError("Model catalog response does not contain a data list")
        models = tuple(VideoModel.from_api(item) for item in raw_models if isinstance(item, Mapping))
        return cls(models=models, fetched_at=fetched_at or datetime.now(timezone.utc), stale=stale)

    def find(self, model_id: str) -> VideoModel | None:
        return next((model for model in self.models if model.id == model_id), None)

    def preferred(self, preferred_id: str = "black-forest-labs/flux-3-video") -> VideoModel | None:
        if not self.models:
            return None
        return self.find(preferred_id) or next(
            (model for model in self.models if "flux" in model.id.lower()), self.models[0]
        )

    def save(self, path: Path) -> None:
        payload = {
            "fetched_at": self.fetched_at.astimezone(timezone.utc).isoformat(),
            "data": [dict(model.raw) for model in self.models],
        }
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_name(path.name + ".tmp")
        temporary.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
        temporary.replace(path)

    @classmethod
    def load(cls, path: Path) -> "VideoCatalog":
        payload = json.loads(path.read_text(encoding="utf-8"))
        fetched_raw = payload.get("fetched_at")
        try:
            fetched_at = datetime.fromisoformat(str(fetched_raw).replace("Z", "+00:00"))
        except (TypeError, ValueError):
            fetched_at = datetime.fromtimestamp(path.stat().st_mtime, timezone.utc)
        return cls.from_api(payload, fetched_at=fetched_at, stale=True)


class JobStatus(str, Enum):
    PENDING = "pending"
    IN_PROGRESS = "in_progress"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"
    EXPIRED = "expired"
    UNKNOWN = "unknown"

    @property
    def terminal(self) -> bool:
        return self in {self.COMPLETED, self.FAILED, self.CANCELLED, self.EXPIRED}


@dataclass(frozen=True, slots=True)
class VideoJob:
    id: str
    status: JobStatus
    polling_url: str
    generation_id: str | None = None
    unsigned_urls: tuple[str, ...] = ()
    usage: Mapping[str, Any] = field(default_factory=dict)
    error: str | None = None
    raw: Mapping[str, Any] = field(default_factory=dict, repr=False, compare=False)
    raw_status: str = "unknown"

    @classmethod
    def from_api(cls, data: JsonMapping) -> "VideoJob":
        raw_status = str(data.get("status") or "unknown").lower()
        try:
            status = JobStatus(raw_status)
        except ValueError:
            status = JobStatus.UNKNOWN
        error_value = data.get("error")
        if isinstance(error_value, Mapping):
            error = str(error_value.get("message") or error_value.get("code") or "Unknown error")
        elif error_value is not None:
            error = str(error_value)
        else:
            error = None
        usage = data.get("usage") if isinstance(data.get("usage"), Mapping) else {}
        return cls(
            id=str(data.get("id") or ""),
            status=status,
            polling_url=str(data.get("polling_url") or ""),
            generation_id=str(data["generation_id"]) if data.get("generation_id") else None,
            unsigned_urls=_tuple_of_str(data.get("unsigned_urls")),
            usage=MappingProxyType(dict(usage)),
            error=error,
            raw=MappingProxyType(dict(data)),
            raw_status=raw_status,
        )

    @property
    def terminal(self) -> bool:
        return self.status.terminal

    @property
    def successful(self) -> bool:
        return self.status is JobStatus.COMPLETED

    @property
    def cost(self) -> Decimal | None:
        return _decimal(self.usage.get("cost"))


@dataclass(frozen=True, slots=True)
class CostEstimate:
    amount: Decimal | None
    basis: str
    exact: bool = False
    pricing_sku: str | None = None
    unit_price: Decimal | None = None
    currency: str = "USD"
    raw_pricing: Mapping[str, Decimal] = field(default_factory=dict)

    @property
    def available(self) -> bool:
        return self.amount is not None


def estimate_cost(model: VideoModel, request: VideoRequest) -> CostEstimate:
    """Estimate only when the advertised SKU has an unambiguous unit.

    OpenRouter model pricing keys are provider-dependent.  Unknown keys are
    deliberately surfaced without guessing, so a paid confirmation cannot show
    a misleading number.
    """

    pricing = model.pricing_skus
    if not pricing:
        return CostEstimate(None, "No pricing advertised", raw_pricing=pricing)

    resolution = request.resolution.lower() if request.resolution else None
    audio = request.generate_audio if request.generate_audio is not None else model.generate_audio

    def variants(stem: str) -> list[str]:
        """Most-specific first variants seen in the live video catalog."""

        result: list[str] = []
        audio_label = "with_audio" if audio is True else "without_audio" if audio is False else None
        if resolution and audio_label:
            result.extend(
                (f"{stem}_{resolution}_{audio_label}", f"{stem}_{audio_label}_{resolution}")
            )
        if audio_label:
            result.append(f"{stem}_{audio_label}")
        if resolution:
            result.extend((f"{stem}_{resolution}", f"{stem}-{resolution}"))
        result.append(stem)
        return result

    if request.duration is not None:
        # FLUX advertises integer cents per output second.
        for sku in variants("cents_per_second_output"):
            if sku in pricing:
                cents = pricing[sku]
                unit = cents / Decimal(100)
                return CostEstimate(
                    amount=unit * Decimal(request.duration),
                    basis=f"{request.duration}s × {cents}¢/video-second",
                    exact=True,
                    pricing_sku=sku,
                    unit_price=unit,
                    raw_pricing=pricing,
                )

        # Some providers distinguish text-to-video from image/reference jobs.
        dollar_stems: list[str] = []
        if not request.frame_images and not request.input_references:
            dollar_stems.append("text_to_video_duration_seconds")
        dollar_stems.extend(
            ("duration_seconds", "per-video-second", "per_video_second", "per_second")
        )
        for stem in dollar_stems:
            for sku in variants(stem):
                if sku in pricing:
                    unit = pricing[sku]
                    return CostEstimate(
                        amount=unit * Decimal(request.duration),
                        basis=f"{request.duration}s × ${unit}/video-second",
                        exact=True,
                        pricing_sku=sku,
                        unit_price=unit,
                        raw_pricing=pricing,
                    )

    # A fixed generation/request SKU is safe only when exactly one is present.
    fixed_keys = [key for key in ("generate", "per-video", "per_generation") if key in pricing]
    if len(pricing) == 1 and len(fixed_keys) == 1:
        sku = fixed_keys[0]
        amount = pricing[sku]
        return CostEstimate(
            amount=amount,
            basis=f"Advertised fixed generation price: ${amount}",
            exact=True,
            pricing_sku=sku,
            unit_price=amount,
            raw_pricing=pricing,
        )

    return CostEstimate(
        None,
        "Pricing SKUs use provider-specific units; inspect raw pricing before submitting",
        raw_pricing=pricing,
    )
