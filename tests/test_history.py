from __future__ import annotations

import sqlite3
from decimal import Decimal
from pathlib import Path

from openrouter_video_studio.history import HistoryStore
from openrouter_video_studio.models import JobStatus, VideoJob, VideoRequest


def request(prompt: str = "Clouds drift over a tiny cinema") -> VideoRequest:
    return VideoRequest(
        model="example/video-one",
        prompt=prompt,
        duration=4,
        resolution="720p",
        aspect_ratio="16:9",
        generate_audio=False,
    )


def job(
    job_id: str = "job-1",
    status: JobStatus = JobStatus.PENDING,
    *,
    cost: str | None = None,
    error: str | None = None,
) -> VideoJob:
    usage = {"cost": cost} if cost is not None else {}
    return VideoJob(
        id=job_id,
        status=status,
        polling_url=f"/api/v1/videos/{job_id}",
        generation_id=f"generation-{job_id}",
        usage=usage,
        error=error,
        raw={"provider_debug": "sk-secret-must-not-be-persisted"},
    )


def test_create_and_reopen_history_round_trips_resume_state(tmp_path: Path) -> None:
    database = tmp_path / "state" / "history.sqlite3"
    store = HistoryStore(database)

    created = store.create_job(request(), job())
    reopened = HistoryStore(database).get("job-1")

    assert created.job_id == "job-1"
    assert reopened is not None
    assert reopened.request == request()
    assert reopened.polling_url == "/api/v1/videos/job-1"
    assert reopened.status == "pending"
    assert reopened.created_at.tzinfo is not None


def test_status_cost_and_output_path_update_preserve_original_request(
    tmp_path: Path,
) -> None:
    store = HistoryStore(tmp_path / "history.sqlite3")
    original_request = request()
    store.create_job(original_request, job())
    output = tmp_path / "Videos" / "finished.mp4"

    updated = store.update_job(
        job("job-1", JobStatus.COMPLETED, cost="0.68"),
        output_path=output,
    )

    assert updated.status == "completed"
    assert updated.terminal is True
    assert updated.cost == Decimal("0.68")
    assert updated.actual_cost == Decimal("0.68")
    assert updated.output_path == output
    assert updated.request == original_request


def test_pending_excludes_every_terminal_status(tmp_path: Path) -> None:
    store = HistoryStore(tmp_path / "history.sqlite3")
    statuses = (
        JobStatus.PENDING,
        JobStatus.IN_PROGRESS,
        JobStatus.COMPLETED,
        JobStatus.FAILED,
        JobStatus.CANCELLED,
        JobStatus.EXPIRED,
    )
    for index, status in enumerate(statuses):
        store.create_job(request(f"prompt {index}"), job(f"job-{index}", status))

    assert {record.status for record in store.pending()} == {
        "pending",
        "in_progress",
    }
    assert len(store.list()) == len(statuses)


def test_import_job_never_requires_or_invents_a_local_request(tmp_path: Path) -> None:
    store = HistoryStore(tmp_path / "history.sqlite3")
    imported = VideoJob(
        id="imported-1",
        status=JobStatus.IN_PROGRESS,
        polling_url="",
    )

    record = store.import_job(imported)

    assert record.job_id == "imported-1"
    assert record.polling_url == "/api/v1/videos/imported-1"
    assert record.request is None
    assert record in store.pending()


def test_history_schema_excludes_api_credentials_and_remote_raw_payload(
    tmp_path: Path,
) -> None:
    database = tmp_path / "history.sqlite3"
    store = HistoryStore(database)
    store.create_job(request(), job())

    with sqlite3.connect(database) as connection:
        columns = {
            row[1] for row in connection.execute("PRAGMA table_info(jobs)").fetchall()
        }
        row_text = repr(connection.execute("SELECT * FROM jobs").fetchone())

    assert "api_key" not in columns
    assert "authorization" not in columns
    assert "raw" not in columns
    assert "sk-secret-must-not-be-persisted" not in row_text


def test_malformed_saved_request_does_not_break_history_listing(tmp_path: Path) -> None:
    database = tmp_path / "history.sqlite3"
    store = HistoryStore(database)
    store.create_job(request(), job())
    with sqlite3.connect(database) as connection:
        connection.execute(
            "UPDATE jobs SET request_json = ? WHERE job_id = ?",
            ("{not valid json", "job-1"),
        )

    records = HistoryStore(database).list()

    assert len(records) == 1
    assert records[0].job_id == "job-1"
    assert records[0].request is None
