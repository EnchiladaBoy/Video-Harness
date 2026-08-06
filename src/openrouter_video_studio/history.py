"""Crash-safe local history for submitted OpenRouter video jobs."""

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any

from .models import JobStatus, VideoJob, VideoRequest


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _parse_time(value: str | None) -> datetime:
    if not value:
        return _utc_now()
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return _utc_now()
    return parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)


def _parse_cost(value: str | None) -> Decimal | None:
    if value is None:
        return None
    try:
        parsed = Decimal(value)
    except (InvalidOperation, ValueError):
        return None
    return parsed if parsed.is_finite() else None


@dataclass(frozen=True, slots=True)
class JobRecord:
    """The non-secret state needed to display or resume a generation."""

    job_id: str
    polling_url: str
    status: str
    request: VideoRequest | None = None
    generation_id: str | None = None
    output_path: Path | None = None
    cost: Decimal | None = None
    error: str | None = None
    created_at: datetime = datetime.min.replace(tzinfo=timezone.utc)
    updated_at: datetime = datetime.min.replace(tzinfo=timezone.utc)

    @property
    def id(self) -> str:
        return self.job_id

    @property
    def remote_id(self) -> str:
        return self.job_id

    @property
    def path(self) -> Path | None:
        return self.output_path

    @property
    def actual_cost(self) -> Decimal | None:
        return self.cost

    @property
    def terminal(self) -> bool:
        return self.status in {
            JobStatus.COMPLETED.value,
            JobStatus.FAILED.value,
            JobStatus.CANCELLED.value,
            JobStatus.EXPIRED.value,
        }


class HistoryStore:
    """Small SQLite repository using one short-lived connection per operation."""

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self._initialized = False

    def _connect(self) -> sqlite3.Connection:
        connection = sqlite3.connect(self.path, timeout=5.0)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA busy_timeout = 5000")
        connection.execute("PRAGMA foreign_keys = ON")
        return connection

    def initialize(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self._connect() as connection:
            connection.execute("PRAGMA journal_mode = WAL")
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS jobs (
                    job_id TEXT PRIMARY KEY,
                    polling_url TEXT NOT NULL,
                    status TEXT NOT NULL,
                    request_json TEXT,
                    generation_id TEXT,
                    output_path TEXT,
                    cost TEXT,
                    error TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                )
                """
            )
            connection.execute(
                "CREATE INDEX IF NOT EXISTS jobs_updated_idx ON jobs(updated_at DESC)"
            )
        self._initialized = True

    def _ready(self) -> None:
        if not self._initialized:
            self.initialize()

    @staticmethod
    def _request_json(request: VideoRequest | None) -> str | None:
        if request is None:
            return None
        return json.dumps(request.to_payload(), ensure_ascii=False, separators=(",", ":"))

    def create_job(self, request: VideoRequest, job: VideoJob) -> JobRecord:
        """Persist a newly accepted paid job without ever storing credentials."""

        if not job.id or not job.polling_url:
            raise ValueError("A submitted job must include an id and polling URL")
        now = _utc_now().isoformat()
        self._ready()
        with self._connect() as connection:
            connection.execute(
                """
                INSERT INTO jobs (
                    job_id, polling_url, status, request_json, generation_id,
                    output_path, cost, error, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?, ?)
                ON CONFLICT(job_id) DO UPDATE SET
                    polling_url = excluded.polling_url,
                    status = excluded.status,
                    request_json = COALESCE(jobs.request_json, excluded.request_json),
                    generation_id = COALESCE(excluded.generation_id, jobs.generation_id),
                    cost = COALESCE(excluded.cost, jobs.cost),
                    error = excluded.error,
                    updated_at = excluded.updated_at
                """,
                (
                    job.id,
                    job.polling_url,
                    job.status.value,
                    self._request_json(request),
                    job.generation_id,
                    str(job.cost) if job.cost is not None else None,
                    job.error,
                    now,
                    now,
                ),
            )
        record = self.get(job.id)
        assert record is not None
        return record

    def import_job(
        self, job: VideoJob, request: VideoRequest | None = None
    ) -> JobRecord:
        """Persist a remotely existing job without creating a new generation."""

        if request is not None:
            return self.create_job(request, job)
        if not job.id:
            raise ValueError("An imported job must include an id")
        polling_url = job.polling_url or f"/api/v1/videos/{job.id}"
        now = _utc_now().isoformat()
        self._ready()
        with self._connect() as connection:
            connection.execute(
                """
                INSERT INTO jobs (
                    job_id, polling_url, status, request_json, generation_id,
                    output_path, cost, error, created_at, updated_at
                ) VALUES (?, ?, ?, NULL, ?, NULL, ?, ?, ?, ?)
                ON CONFLICT(job_id) DO UPDATE SET
                    polling_url = excluded.polling_url,
                    status = excluded.status,
                    generation_id = COALESCE(excluded.generation_id, jobs.generation_id),
                    cost = COALESCE(excluded.cost, jobs.cost),
                    error = excluded.error,
                    updated_at = excluded.updated_at
                """,
                (
                    job.id,
                    polling_url,
                    job.status.value,
                    job.generation_id,
                    str(job.cost) if job.cost is not None else None,
                    job.error,
                    now,
                    now,
                ),
            )
        record = self.get(job.id)
        assert record is not None
        return record

    def update_job(
        self, job: VideoJob, output_path: str | Path | None = None
    ) -> JobRecord:
        """Update status/cost and optionally the atomically completed output path."""

        self._ready()
        existing = self.get(job.id)
        if existing is None:
            existing = self.import_job(job)
        polling_url = job.polling_url or existing.polling_url
        updated = _utc_now().isoformat()
        with self._connect() as connection:
            connection.execute(
                """
                UPDATE jobs SET
                    polling_url = ?, status = ?,
                    generation_id = COALESCE(?, generation_id),
                    output_path = COALESCE(?, output_path),
                    cost = COALESCE(?, cost), error = ?, updated_at = ?
                WHERE job_id = ?
                """,
                (
                    polling_url,
                    job.status.value,
                    job.generation_id,
                    str(Path(output_path)) if output_path is not None else None,
                    str(job.cost) if job.cost is not None else None,
                    job.error,
                    updated,
                    job.id,
                ),
            )
        record = self.get(job.id)
        assert record is not None
        return record

    def mark_downloaded(
        self, job: VideoJob, output_path: str | Path
    ) -> JobRecord:
        return self.update_job(job, output_path=output_path)

    def get(self, job_id: str) -> JobRecord | None:
        self._ready()
        with self._connect() as connection:
            row = connection.execute(
                "SELECT * FROM jobs WHERE job_id = ?", (job_id,)
            ).fetchone()
        return self._from_row(row) if row is not None else None

    def list(self, *, limit: int = 500) -> list[JobRecord]:
        self._ready()
        safe_limit = max(1, min(int(limit), 5000))
        with self._connect() as connection:
            rows = connection.execute(
                "SELECT * FROM jobs ORDER BY created_at DESC LIMIT ?", (safe_limit,)
            ).fetchall()
        return [self._from_row(row) for row in rows]

    def list_records(self, *, limit: int = 500) -> list[JobRecord]:
        return self.list(limit=limit)

    def pending(self) -> list[JobRecord]:
        terminal = (
            JobStatus.COMPLETED.value,
            JobStatus.FAILED.value,
            JobStatus.CANCELLED.value,
            JobStatus.EXPIRED.value,
        )
        self._ready()
        with self._connect() as connection:
            rows = connection.execute(
                """
                SELECT * FROM jobs
                WHERE status NOT IN (?, ?, ?, ?)
                ORDER BY created_at DESC
                """,
                terminal,
            ).fetchall()
        return [self._from_row(row) for row in rows]

    @staticmethod
    def _from_row(row: sqlite3.Row) -> JobRecord:
        request: VideoRequest | None = None
        if row["request_json"]:
            try:
                value: Any = json.loads(row["request_json"])
                if isinstance(value, dict):
                    request = VideoRequest.from_payload(value)
            except (json.JSONDecodeError, TypeError, ValueError):
                request = None
        return JobRecord(
            job_id=str(row["job_id"]),
            polling_url=str(row["polling_url"]),
            status=str(row["status"]),
            request=request,
            generation_id=str(row["generation_id"]) if row["generation_id"] else None,
            output_path=Path(row["output_path"]) if row["output_path"] else None,
            cost=_parse_cost(row["cost"]),
            error=str(row["error"]) if row["error"] else None,
            created_at=_parse_time(row["created_at"]),
            updated_at=_parse_time(row["updated_at"]),
        )

    def close(self) -> None:
        """Kept for application lifecycle symmetry; connections are short-lived."""


__all__ = ["HistoryStore", "JobRecord"]
