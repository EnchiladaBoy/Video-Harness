from __future__ import annotations

import json
from pathlib import Path

import httpx
import pytest

from openrouter_video_studio.api import (
    DownloadError,
    OpenRouterClient,
    RequestValidationError,
    SubmissionUncertainError,
    UnsafeURLError,
)
from openrouter_video_studio.models import VideoRequest


@pytest.mark.asyncio
async def test_key_validation_is_authorized_but_public_catalog_is_not() -> None:
    seen: list[tuple[str, str | None]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append((request.url.path, request.headers.get("authorization")))
        if request.url.path.endswith("/key"):
            return httpx.Response(200, json={"data": {"label": "studio"}})
        return httpx.Response(200, json={"data": []})

    async with OpenRouterClient(
        "sk-test-secret", transport=httpx.MockTransport(handler)
    ) as client:
        key = await client.validate_key()
        catalog = await client.list_video_models()

    assert key.label == "studio"
    assert catalog.models == ()
    assert seen == [
        ("/api/v1/key", "Bearer sk-test-secret"),
        ("/api/v1/videos/models", None),
    ]


@pytest.mark.asyncio
async def test_submit_constructs_one_paid_post_with_expected_json() -> None:
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(
            200,
            json={
                "id": "job-123",
                "status": "pending",
                "polling_url": "/api/v1/videos/job-123",
            },
        )

    video_request = VideoRequest(
        model="example/video-one",
        prompt="A tiny cinema in the clouds",
        duration=4,
        resolution="720p",
        aspect_ratio="16:9",
        generate_audio=False,
    )
    async with OpenRouterClient(
        "sk-test-secret", transport=httpx.MockTransport(handler), max_retries=5
    ) as client:
        job = await client.submit(video_request)

    assert job.id == "job-123"
    assert len(requests) == 1
    request = requests[0]
    assert request.method == "POST"
    assert request.url == httpx.URL("https://openrouter.ai/api/v1/videos")
    assert request.headers["authorization"] == "Bearer sk-test-secret"
    assert json.loads(request.content) == video_request.to_payload()


@pytest.mark.asyncio
async def test_ambiguous_submission_network_failure_is_never_retried() -> None:
    attempts = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        raise httpx.ConnectError("connection lost", request=request)

    async with OpenRouterClient(
        "sk-test-secret",
        transport=httpx.MockTransport(handler),
        max_retries=9,
        backoff_base=0,
    ) as client:
        with pytest.raises(SubmissionUncertainError, match="may exist"):
            await client.submit(VideoRequest(model="example/video-one", prompt="test"))

    assert attempts == 1


@pytest.mark.asyncio
async def test_safe_catalog_read_retries_rate_limit_response() -> None:
    attempts = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            return httpx.Response(
                429,
                headers={"Retry-After": "0"},
                json={"error": {"message": "slow down"}},
            )
        return httpx.Response(200, json={"data": []})

    async with OpenRouterClient(
        "sk-test-secret",
        transport=httpx.MockTransport(handler),
        max_retries=2,
        backoff_base=0,
    ) as client:
        catalog = await client.list_video_models()

    assert catalog.models == ()
    assert attempts == 2


@pytest.mark.asyncio
async def test_polling_rejects_cross_origin_url_before_http_request() -> None:
    attempts = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        return httpx.Response(500)

    async with OpenRouterClient(
        "sk-test-secret", transport=httpx.MockTransport(handler)
    ) as client:
        with pytest.raises(UnsafeURLError, match="OpenRouter API"):
            await client.poll("https://attacker.example/api/v1/videos/job-1")

    assert attempts == 0


@pytest.mark.asyncio
async def test_download_redirect_drops_authorization_at_unsigned_origin(
    tmp_path: Path,
) -> None:
    seen: list[tuple[str, str | None]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append((str(request.url), request.headers.get("authorization")))
        if request.url.host == "openrouter.ai":
            return httpx.Response(
                302,
                headers={"Location": "https://cdn.example/video.mp4?signature=public"},
            )
        return httpx.Response(
            200,
            headers={"Content-Length": "11"},
            content=b"video-bytes",
        )

    destination = tmp_path / "movie.mp4"
    progress: list[tuple[int, int | None]] = []
    async with OpenRouterClient(
        "sk-test-secret", transport=httpx.MockTransport(handler)
    ) as client:
        result = await client.download(
            client.content_url("job-1"),
            destination,
            progress=lambda written, total: progress.append((written, total)),
        )

    assert result == destination
    assert destination.read_bytes() == b"video-bytes"
    assert not (tmp_path / "movie.mp4.part").exists()
    assert seen == [
        (
            "https://openrouter.ai/api/v1/videos/job-1/content?index=0",
            "Bearer sk-test-secret",
        ),
        ("https://cdn.example/video.mp4?signature=public", None),
    ]
    assert progress == [(11, 11)]


@pytest.mark.asyncio
async def test_api_error_message_redacts_the_api_key() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            400,
            json={"error": {"message": "provider echoed sk-test-secret"}},
        )

    async with OpenRouterClient(
        "sk-test-secret", transport=httpx.MockTransport(handler)
    ) as client:
        with pytest.raises(RequestValidationError) as captured:
            await client.submit(VideoRequest(model="example/video-one", prompt="test"))

    assert "sk-test-secret" not in str(captured.value)
    assert "[REDACTED]" in str(captured.value)


@pytest.mark.parametrize(
    "unsafe_url",
    [
        "http://cdn.example/video.mp4",
        "file:///tmp/video.mp4",
        "https://name:password@cdn.example/video.mp4",
    ],
)
@pytest.mark.asyncio
async def test_download_rejects_unsafe_url_without_http_request(
    unsafe_url: str, tmp_path: Path
) -> None:
    attempts = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        return httpx.Response(200, content=b"should not happen")

    async with OpenRouterClient(
        "sk-test-secret", transport=httpx.MockTransport(handler)
    ) as client:
        with pytest.raises(UnsafeURLError):
            await client.download(unsafe_url, tmp_path / "video.mp4")

    assert attempts == 0
    assert not (tmp_path / "video.mp4").exists()


@pytest.mark.parametrize("existing_name", ["video.mp4", "video.mp4.part"])
@pytest.mark.asyncio
async def test_download_refuses_to_overwrite_existing_files(
    existing_name: str, tmp_path: Path
) -> None:
    existing = tmp_path / existing_name
    existing.write_bytes(b"keep-me")
    attempts = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        return httpx.Response(200, content=b"replacement")

    async with OpenRouterClient(
        "sk-test-secret", transport=httpx.MockTransport(handler)
    ) as client:
        with pytest.raises(DownloadError, match="Refusing to overwrite"):
            await client.download(
                "https://cdn.example/video.mp4", tmp_path / "video.mp4"
            )

    assert attempts == 0
    assert existing.read_bytes() == b"keep-me"
