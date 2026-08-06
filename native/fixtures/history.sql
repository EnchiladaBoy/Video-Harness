PRAGMA journal_mode = WAL;
CREATE TABLE jobs (
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
);
CREATE INDEX jobs_updated_idx ON jobs(updated_at DESC);
INSERT INTO jobs (
    job_id, polling_url, status, request_json, generation_id,
    output_path, cost, error, created_at, updated_at
) VALUES (
    'job-pending-fixture',
    '/api/v1/videos/job-pending-fixture',
    'in_progress',
    '{"model":"black-forest-labs/flux-3-video","prompt":"A pending fixture","duration":4,"resolution":"720p","aspect_ratio":"16:9","generate_audio":false}',
    'generation-pending-fixture',
    NULL,
    NULL,
    NULL,
    '2026-08-06T04:30:15+00:00',
    '2026-08-06T04:31:15+00:00'
);
INSERT INTO jobs (
    job_id, polling_url, status, request_json, generation_id,
    output_path, cost, error, created_at, updated_at
) VALUES (
    'job-complete-fixture',
    '/api/v1/videos/job-complete-fixture',
    'completed',
    '{"model":"black-forest-labs/flux-3-video","prompt":"A completed fixture","duration":5,"resolution":"720p"}',
    'generation-complete-fixture',
    '/home/alex/Videos/20260806-143015-a-completed-fixture-job-complete-fixtu.mp4',
    '0.85',
    NULL,
    '2026-08-06T04:20:15+00:00',
    '2026-08-06T04:25:15+00:00'
);

