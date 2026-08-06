"""Asynchronous, origin-safe OpenRouter video API client."""

from __future__ import annotations

import asyncio
import inspect
import json
import re
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import Any, Mapping
from urllib.parse import quote, urljoin, urlsplit

import httpx

from .models import VideoCatalog, VideoJob, VideoRequest


DEFAULT_BASE_URL = "https://openrouter.ai/api/v1"
DEFAULT_APP_TITLE = "OpenRouter Video Studio"
RETRYABLE_STATUS_CODES = frozenset({408, 425, 429, 500, 502, 503, 504})
MAX_REDIRECTS = 5


class OpenRouterError(Exception):
    """Base class for errors safe to show in the interface."""

    def __init__(
        self,
        message: str,
        *,
        status_code: int | None = None,
        code: str | None = None,
        details: Mapping[str, Any] | None = None,
        retry_after: float | None = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status_code = status_code
        self.code = code
        self.details = dict(details or {})
        self.retry_after = retry_after


class AuthenticationError(OpenRouterError):
    pass


class InsufficientCreditsError(OpenRouterError):
    pass


class RequestValidationError(OpenRouterError):
    pass


class ContentPolicyError(OpenRouterError):
    pass


class ResourceNotFoundError(OpenRouterError):
    pass


class RateLimitError(OpenRouterError):
    pass


class ProviderError(OpenRouterError):
    pass


class NetworkError(OpenRouterError):
    pass


class SubmissionUncertainError(NetworkError):
    """The paid POST may have reached the server and must not be retried blindly."""


class ResponseFormatError(OpenRouterError):
    pass


class UnsafeURLError(OpenRouterError):
    pass


class DownloadError(OpenRouterError):
    pass


@dataclass(frozen=True, slots=True)
class KeyInfo:
    label: str = ""
    limit: Decimal | None = None
    limit_remaining: Decimal | None = None
    limit_reset: str | None = None
    usage: Decimal | None = None
    is_free_tier: bool = False
    expires_at: str | None = None
    raw: Mapping[str, Any] = field(default_factory=dict, repr=False, compare=False)

    @classmethod
    def from_api(cls, payload: Mapping[str, Any]) -> "KeyInfo":
        data = payload.get("data")
        if not isinstance(data, Mapping):
            raise ResponseFormatError("Key validation response did not contain a data object")
        return cls(
            label=str(data.get("label") or ""),
            limit=_as_decimal(data.get("limit")),
            limit_remaining=_as_decimal(data.get("limit_remaining")),
            limit_reset=str(data["limit_reset"]) if data.get("limit_reset") else None,
            usage=_as_decimal(data.get("usage")),
            is_free_tier=bool(data.get("is_free_tier", False)),
            expires_at=str(data["expires_at"]) if data.get("expires_at") else None,
            raw=dict(data),
        )


ProgressCallback = Callable[[int, int | None], Awaitable[None] | None]


def _as_decimal(value: Any) -> Decimal | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        result = Decimal(str(value))
    except (InvalidOperation, TypeError, ValueError):
        return None
    return result if result.is_finite() else None


def _origin(url: str) -> tuple[str, str, int | None]:
    parsed = urlsplit(url)
    scheme = parsed.scheme.lower()
    host = (parsed.hostname or "").lower()
    port = parsed.port
    if port is None:
        port = 443 if scheme == "https" else 80 if scheme == "http" else None
    return scheme, host, port


def _retry_after(response: httpx.Response) -> float | None:
    value = response.headers.get("Retry-After")
    if not value:
        return None
    try:
        return max(0.0, min(float(value), 60.0))
    except ValueError:
        try:
            date = parsedate_to_datetime(value)
            if date.tzinfo is None:
                date = date.replace(tzinfo=timezone.utc)
            delay = (date - datetime.now(timezone.utc)).total_seconds()
            return max(0.0, min(delay, 60.0))
        except (TypeError, ValueError, OverflowError):
            return None


class OpenRouterClient:
    """Small async client tailored to OpenRouter's asynchronous video workflow.

    The client deliberately has no default Authorization header.  Every request
    makes an origin/path decision first, preventing a key from being forwarded
    to provider-owned unsigned URLs or cross-origin redirects.
    """

    def __init__(
        self,
        api_key: str,
        *,
        base_url: str = DEFAULT_BASE_URL,
        http_referer: str | None = None,
        app_title: str = DEFAULT_APP_TITLE,
        timeout: float | httpx.Timeout = 60.0,
        max_retries: int = 3,
        backoff_base: float = 1.0,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        if not api_key.strip():
            raise ValueError("An OpenRouter API key is required")
        parsed = urlsplit(base_url)
        if parsed.scheme.lower() != "https" or not parsed.hostname:
            raise ValueError("base_url must be an HTTPS URL")
        if parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError("base_url must not contain credentials, a query, or a fragment")
        self._api_key = api_key.strip()
        self.base_url = base_url.rstrip("/")
        self._api_origin = _origin(self.base_url)
        self._api_path = urlsplit(self.base_url).path.rstrip("/")
        self.max_retries = max(0, int(max_retries))
        self.backoff_base = max(0.0, float(backoff_base))
        base_headers = {
            "Accept": "application/json",
            "User-Agent": "openrouter-video-studio/0.1",
        }
        if http_referer:
            base_headers["HTTP-Referer"] = http_referer
        if app_title:
            base_headers["X-Title"] = app_title
        self._client = httpx.AsyncClient(
            headers=base_headers,
            timeout=timeout,
            transport=transport,
            follow_redirects=False,
        )
        self._closed = False

    async def __aenter__(self) -> "OpenRouterClient":
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        if not self._closed:
            self._closed = True
            await self._client.aclose()

    def is_openrouter_api_url(self, url: str) -> bool:
        """Return whether it is safe to attach the user's API key to ``url``."""

        try:
            parsed = urlsplit(url)
            path = parsed.path.rstrip("/")
            return (
                _origin(url) == self._api_origin
                and parsed.scheme.lower() == "https"
                and (path == self._api_path or path.startswith(self._api_path + "/"))
                and not parsed.username
                and not parsed.password
            )
        except ValueError:
            return False

    def _headers(self, url: str, *, authorize: bool) -> dict[str, str]:
        if authorize:
            if not self.is_openrouter_api_url(url):
                raise UnsafeURLError("Refusing to send authorization outside the OpenRouter API")
            return {"Authorization": f"Bearer {self._api_key}"}
        return {}

    def _api_url(self, path: str) -> str:
        return f"{self.base_url}/{path.lstrip('/')}"

    def _resolve_polling_url(self, polling_url_or_job_id: str) -> str:
        value = polling_url_or_job_id.strip()
        if not value:
            raise ValueError("A polling URL or job ID is required")
        parsed = urlsplit(value)
        if parsed.scheme:
            url = value
        elif value.startswith("/"):
            scheme, host, port = self._api_origin
            authority = host if port == 443 else f"{host}:{port}"
            url = f"{scheme}://{authority}{value}"
        elif "/" not in value:
            url = self._api_url(f"videos/{quote(value, safe='')}")
        else:
            url = urljoin(self.base_url + "/", value)
        if not self.is_openrouter_api_url(url):
            raise UnsafeURLError("Polling URL is not an OpenRouter API URL")
        if not urlsplit(url).path.startswith(self._api_path + "/videos/"):
            raise UnsafeURLError("Polling URL is not a video job URL")
        return url

    async def validate_key(self) -> KeyInfo:
        payload = await self._request_json(
            "GET", self._api_url("key"), authorize=True, retry_safe=True
        )
        return KeyInfo.from_api(payload)

    async def list_video_models(self) -> VideoCatalog:
        # The catalog is public.  Avoid sending a credential where none is needed.
        payload = await self._request_json(
            "GET", self._api_url("videos/models"), authorize=False, retry_safe=True
        )
        try:
            return VideoCatalog.from_api(payload)
        except ValueError as exc:
            raise ResponseFormatError(str(exc)) from exc

    async def submit(self, request: VideoRequest) -> VideoJob:
        """Submit exactly once; ambiguous POST failures are never auto-retried."""

        payload = await self._request_json(
            "POST",
            self._api_url("videos"),
            authorize=True,
            retry_safe=False,
            json_body=request.to_payload(),
        )
        job = VideoJob.from_api(payload)
        if not job.id or not job.polling_url:
            raise ResponseFormatError("Video submission response is missing id or polling_url")
        # Validate now, before untrusted response data is persisted or polled.
        self._resolve_polling_url(job.polling_url)
        return job

    async def poll(self, polling_url_or_job_id: str) -> VideoJob:
        url = self._resolve_polling_url(polling_url_or_job_id)
        payload = await self._request_json("GET", url, authorize=True, retry_safe=True)
        job = VideoJob.from_api(payload)
        if not job.id:
            raise ResponseFormatError("Video status response is missing the job id")
        return job

    def content_url(self, job_id: str, *, index: int = 0) -> str:
        if not job_id.strip():
            raise ValueError("job_id is required")
        if index < 0:
            raise ValueError("index must be non-negative")
        return self._api_url(f"videos/{quote(job_id.strip(), safe='')}/content?index={index}")

    async def download(
        self,
        url: str,
        destination: str | Path,
        *,
        progress: ProgressCallback | None = None,
    ) -> Path:
        """Download to ``.part`` and atomically promote a non-empty response."""

        target = Path(destination)
        target.parent.mkdir(parents=True, exist_ok=True)
        partial = target.with_name(target.name + ".part")
        self._validate_download_url(url)
        if target.exists() or partial.exists():
            raise DownloadError(f"Refusing to overwrite an existing download: {target}")

        last_error: OpenRouterError | None = None
        for attempt in range(self.max_retries + 1):
            try:
                await self._download_once(url, partial, progress)
                if partial.stat().st_size <= 0:
                    raise DownloadError("OpenRouter returned an empty video file")
                if target.exists():
                    raise DownloadError(f"Refusing to overwrite an existing video: {target}")
                partial.replace(target)
                return target
            except (httpx.TransportError, httpx.StreamError) as exc:
                last_error = NetworkError("Network connection interrupted while downloading video")
                last_error.__cause__ = exc
            except OpenRouterError as exc:
                last_error = exc
                if not self._is_retryable_error(exc):
                    self._remove_partial(partial)
                    raise
            except OSError as exc:
                self._remove_partial(partial)
                raise DownloadError(f"Could not save video: {exc}") from exc

            self._remove_partial(partial)
            if attempt >= self.max_retries:
                assert last_error is not None
                raise last_error
            await asyncio.sleep(self._delay(attempt, getattr(last_error, "retry_after", None)))

        raise DownloadError("Video download failed")  # pragma: no cover

    async def _download_once(
        self,
        url: str,
        partial: Path,
        progress: ProgressCallback | None,
    ) -> None:
        response = await self._open_download_response(url)
        try:
            if response.status_code >= 400:
                await response.aread()
                raise self._error_from_response(response)
            try:
                total = int(response.headers["Content-Length"])
            except (KeyError, TypeError, ValueError):
                total = None
            written = 0
            with partial.open("wb") as output:
                async for chunk in response.aiter_bytes():
                    if not chunk:
                        continue
                    output.write(chunk)
                    written += len(chunk)
                    if progress:
                        result = progress(written, total)
                        if inspect.isawaitable(result):
                            await result
            if written == 0:
                raise DownloadError("OpenRouter returned an empty video file")
        finally:
            await response.aclose()

    async def _open_download_response(self, url: str) -> httpx.Response:
        current = url
        for redirect_count in range(MAX_REDIRECTS + 1):
            self._validate_download_url(current)
            authorize = self.is_openrouter_api_url(current) and urlsplit(current).path.startswith(
                self._api_path + "/videos/"
            )
            request = self._client.build_request(
                "GET", current, headers=self._headers(current, authorize=authorize)
            )
            response = await self._client.send(request, stream=True, follow_redirects=False)
            if response.status_code not in {301, 302, 303, 307, 308}:
                return response
            location = response.headers.get("Location")
            if not location:
                await response.aclose()
                raise DownloadError("Video download redirect did not include a destination")
            next_url = str(response.url.join(location))
            await response.aclose()
            current = next_url
            if redirect_count == MAX_REDIRECTS:
                raise DownloadError("Video download exceeded the redirect limit")
        raise DownloadError("Video download exceeded the redirect limit")  # pragma: no cover

    @staticmethod
    def _validate_download_url(url: str) -> None:
        parsed = urlsplit(url)
        if parsed.scheme.lower() != "https" or not parsed.hostname:
            raise UnsafeURLError("Video downloads must use HTTPS")
        if parsed.username or parsed.password:
            raise UnsafeURLError("Video download URL must not contain embedded credentials")

    @staticmethod
    def _remove_partial(path: Path) -> None:
        try:
            path.unlink(missing_ok=True)
        except OSError:
            pass

    async def _request_json(
        self,
        method: str,
        url: str,
        *,
        authorize: bool,
        retry_safe: bool,
        json_body: Mapping[str, Any] | None = None,
    ) -> Mapping[str, Any]:
        for attempt in range(self.max_retries + 1):
            try:
                response = await self._client.request(
                    method,
                    url,
                    headers=self._headers(url, authorize=authorize),
                    json=json_body,
                )
            except httpx.TransportError as exc:
                if not retry_safe:
                    raise SubmissionUncertainError(
                        "Connection failed during submission. The job may exist; do not submit again until history is checked."
                    ) from exc
                if attempt >= self.max_retries:
                    raise NetworkError("Could not reach OpenRouter") from exc
                await asyncio.sleep(self._delay(attempt))
                continue

            if response.status_code >= 400:
                error = self._error_from_response(response)
                if (
                    retry_safe
                    and response.status_code in RETRYABLE_STATUS_CODES
                    and attempt < self.max_retries
                ):
                    await asyncio.sleep(self._delay(attempt, error.retry_after))
                    continue
                raise error
            try:
                payload = response.json()
            except (json.JSONDecodeError, UnicodeDecodeError, ValueError) as exc:
                raise ResponseFormatError("OpenRouter returned an invalid JSON response") from exc
            if not isinstance(payload, Mapping):
                raise ResponseFormatError("OpenRouter returned a non-object JSON response")
            return payload
        raise NetworkError("Could not reach OpenRouter")  # pragma: no cover

    def _error_from_response(self, response: httpx.Response) -> OpenRouterError:
        message, code, details = self._extract_error(response)
        kwargs = {
            "status_code": response.status_code,
            "code": code,
            "details": details,
            "retry_after": _retry_after(response),
        }
        status = response.status_code
        if status == 401:
            return AuthenticationError("API key was rejected by OpenRouter", **kwargs)
        if status == 402:
            return InsufficientCreditsError(
                "OpenRouter credits are insufficient for this request", **kwargs
            )
        policy_hint = f"{code or ''} {message}".lower()
        if status == 403 or any(
            token in policy_hint
            for token in ("content policy", "content_policy", "moderation", "safety policy")
        ):
            return ContentPolicyError(
                message or "OpenRouter denied this request because of an account or content policy",
                **kwargs,
            )
        if status == 400 or status == 422:
            return RequestValidationError(message or "OpenRouter rejected the request", **kwargs)
        if status == 404:
            return ResourceNotFoundError(message or "OpenRouter resource was not found", **kwargs)
        if status == 429:
            return RateLimitError("OpenRouter rate limit reached; try again shortly", **kwargs)
        if status >= 500:
            return ProviderError(
                message or "OpenRouter or the video provider is temporarily unavailable", **kwargs
            )
        return OpenRouterError(message or f"OpenRouter request failed ({status})", **kwargs)

    def _extract_error(
        self, response: httpx.Response
    ) -> tuple[str, str | None, Mapping[str, Any]]:
        code: str | None = None
        details: Mapping[str, Any] = {}
        try:
            payload = response.json()
        except (json.JSONDecodeError, UnicodeDecodeError, ValueError):
            text = re.sub(r"\s+", " ", response.text).strip()[:500]
            return self._redact(text), None, details
        if not isinstance(payload, Mapping):
            return "", None, details
        error = payload.get("error", payload)
        if isinstance(error, Mapping):
            raw_code = error.get("code")
            code = self._redact(str(raw_code)) if raw_code is not None else None
            raw_details = error.get("metadata") or error.get("details")
            details = (
                self._redact_mapping(raw_details) if isinstance(raw_details, Mapping) else {}
            )
            message = str(error.get("message") or error.get("error") or "")
        else:
            message = str(error or "")
        return self._redact(message[:1000]), code, details

    def _redact(self, value: str) -> str:
        return value.replace(self._api_key, "[REDACTED]") if self._api_key else value

    def _redact_mapping(self, value: Mapping[str, Any]) -> Mapping[str, Any]:
        def clean(item: Any) -> Any:
            if isinstance(item, str):
                return self._redact(item)
            if isinstance(item, Mapping):
                return {str(key): clean(nested) for key, nested in item.items()}
            if isinstance(item, (list, tuple)):
                return [clean(nested) for nested in item]
            return item

        return {str(key): clean(item) for key, item in value.items()}

    def _delay(self, attempt: int, retry_after: float | None = None) -> float:
        if retry_after is not None:
            return retry_after
        return min(self.backoff_base * (2**attempt), 30.0)

    @staticmethod
    def _is_retryable_error(error: OpenRouterError) -> bool:
        return isinstance(error, NetworkError) or error.status_code in RETRYABLE_STATUS_CODES


__all__ = [
    "AuthenticationError",
    "ContentPolicyError",
    "DownloadError",
    "InsufficientCreditsError",
    "KeyInfo",
    "NetworkError",
    "OpenRouterClient",
    "OpenRouterError",
    "ProviderError",
    "RateLimitError",
    "RequestValidationError",
    "ResourceNotFoundError",
    "ResponseFormatError",
    "SubmissionUncertainError",
    "UnsafeURLError",
]
