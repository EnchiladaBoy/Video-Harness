# Compatibility fixtures

These files preserve the persisted and wire-level contracts used by Video Harness, including data written by the retired Python edition.

- `catalog.json` is the exact cached catalog envelope: `fetched_at` plus raw model `data`.
- `requests.json` covers omission of unset fields and the complete frame/reference/provider shape.
- `jobs.json` covers relative polling URLs, terminal states, unsigned downloads, decimal cost, and structured errors.
- `history.sql` creates the legacy Python v1 SQLite schema (`user_version` remains zero) with resumable and completed rows.
- `fake-video-harness.sh` is a harmless GUI executable used only to verify installer behavior in temporary directories.

Fixtures contain no real API key, signed production URL, or billable endpoint. Integration tests must use deterministic executors or loopback-only servers.
