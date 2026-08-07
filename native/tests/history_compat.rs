use std::fs;
use std::path::Path;

use rusqlite::Connection;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tempfile::tempdir;
use video_harness::domain::{
    JobLocator, JobStatus, ProviderId, ProviderJobKey, VideoJob, VideoRequest,
};
use video_harness::history::{HistoryError, HistoryStore};

fn fixture(name: &str) -> Value {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = fs::read_to_string(manifest.join("fixtures").join(name)).expect("read fixture");
    serde_json::from_str(&text).expect("parse fixture JSON")
}

fn request(prompt: &str) -> VideoRequest {
    let mut request = VideoRequest::new("example/video-one", prompt).expect("request");
    request.duration = Some(4);
    request.resolution = Some("720p".into());
    request.aspect_ratio = Some("16:9".into());
    request.generate_audio = Some(false);
    request
}

fn job(id: &str, status: &str, cost: Option<&str>) -> VideoJob {
    let mut payload = json!({
        "id": id,
        "status": status,
        "polling_url": format!("/api/v1/videos/{id}"),
        "generation_id": format!("generation-{id}"),
        "provider_debug": "sk-secret-must-not-be-persisted"
    });
    if let Some(cost) = cost {
        payload["usage"] = json!({"cost": cost});
    }
    VideoJob::from_api(&payload).expect("job")
}

fn fal_job(id: &str, status: &str, cost: Option<&str>) -> VideoJob {
    let endpoint_id = "fal-ai/test-video";
    let status_url = format!("https://queue.fal.run/{endpoint_id}/requests/{id}/status");
    let mut usage = serde_json::Map::new();
    if let Some(cost) = cost {
        usage.insert("cost".into(), Value::String(cost.into()));
        usage.insert("currency".into(), Value::String("usd".into()));
    }
    VideoJob {
        provider_id: ProviderId::fal(),
        id: id.into(),
        status: JobStatus::from_raw(status),
        polling_url: status_url.clone(),
        generation_id: None,
        unsigned_urls: Vec::new(),
        usage,
        error: None,
        locator: JobLocator::Fal {
            endpoint_id: endpoint_id.into(),
            request_id: id.into(),
            status_url: Some(status_url),
            response_url: None,
        },
        artifacts: Vec::new(),
        raw: json!({"redacted": true}),
    }
}

#[test]
fn python_history_fixture_is_read_without_migration_or_data_loss() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let sql =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/history.sql"))
            .expect("read SQL fixture");
    Connection::open(&database)
        .expect("open fixture database")
        .execute_batch(&sql)
        .expect("load Python schema fixture");

    let store = HistoryStore::new(&database);
    let pending = store.pending().expect("read pending rows");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].job_id, "job-pending-fixture");
    assert_eq!(pending[0].status, "in_progress");
    assert_eq!(
        pending[0]
            .request
            .as_ref()
            .map(|request| request.prompt.as_str()),
        Some("A pending fixture")
    );

    let completed = store
        .get("job-complete-fixture")
        .expect("query completed row")
        .expect("completed row");
    assert!(completed.terminal());
    assert_eq!(completed.actual_cost(), Some(Decimal::new(85, 2)));
    assert_eq!(store.list(100).expect("list rows").len(), 2);
}

#[test]
fn first_native_open_creates_a_no_clobber_sqlite_backup_with_wal_content() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let sql =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/history.sql"))
            .expect("read SQL fixture");
    let source = Connection::open(&database).expect("open source database");
    source.execute_batch(&sql).expect("load Python fixture");
    source
        .execute_batch(
            "PRAGMA wal_autocheckpoint = 0;
             INSERT INTO jobs (
                 job_id, polling_url, status, created_at, updated_at
             ) VALUES (
                 'job-in-wal', '/api/v1/videos/job-in-wal', 'pending',
                 '2026-08-06T05:00:00+00:00', '2026-08-06T05:00:00+00:00'
             );",
        )
        .expect("commit WAL-only row");

    let store = HistoryStore::new(&database);
    let backup = store
        .ensure_pre_rust_backup()
        .expect("create pre-Rust backup")
        .expect("existing Python database needs a backup");
    assert_eq!(backup, store.pre_rust_backup_path());
    let backup_connection = Connection::open(&backup).expect("open backup");
    let backed_up_rows: i64 = backup_connection
        .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
        .expect("count backed-up rows");
    assert_eq!(backed_up_rows, 3);
    let native_tables: i64 = backup_connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'video_harness_native_meta'",
            [],
            |row| row.get(0),
        )
        .expect("inspect backup schema");
    assert_eq!(native_tables, 0);
    drop(backup_connection);

    store.initialize().expect("initialize native metadata");
    let original_backup_bytes = fs::read(&backup).expect("read one-time backup");
    source
        .execute(
            "UPDATE jobs SET status = 'failed' WHERE job_id = 'job-in-wal'",
            [],
        )
        .expect("mutate live database after backup");
    let existing = store
        .ensure_pre_rust_backup()
        .expect("check existing backup");
    assert!(existing.is_none() || existing.as_deref() == Some(backup.as_path()));
    assert_eq!(
        fs::read(&backup).expect("reread one-time backup"),
        original_backup_bytes
    );
}

#[test]
fn create_reopen_and_complete_preserves_original_request() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("state/history.sqlite3");
    let store = HistoryStore::new(&database);
    let original = request("Clouds drift over a tiny cinema");
    let created = store
        .create_job(&original, &job("job-1", "pending", None))
        .expect("create history row");
    assert_eq!(created.job_id, "job-1");

    let reopened = HistoryStore::new(&database)
        .get("job-1")
        .expect("reopen history")
        .expect("saved row");
    assert_eq!(reopened.request.as_ref(), Some(&original));
    assert_eq!(reopened.polling_url, "/api/v1/videos/job-1");

    let output = directory.path().join("Videos/finished.mp4");
    let updated = store
        .update_job(&job("job-1", "completed", Some("0.68")), Some(&output))
        .expect("complete history row");
    assert_eq!(updated.status, "completed");
    assert!(updated.terminal());
    assert_eq!(updated.cost, Some(Decimal::new(68, 2)));
    assert_eq!(updated.output_path.as_deref(), Some(output.as_path()));
    assert_eq!(updated.request.as_ref(), Some(&original));
}

#[test]
fn pending_excludes_all_terminal_statuses_and_import_needs_no_request() {
    let directory = tempdir().expect("temporary directory");
    let store = HistoryStore::new(directory.path().join("history.sqlite3"));
    for (index, status) in [
        "pending",
        "in_progress",
        "completed",
        "failed",
        "cancelled",
        "expired",
    ]
    .into_iter()
    .enumerate()
    {
        store
            .create_job(
                &request(&format!("prompt {index}")),
                &job(&format!("job-{index}"), status, None),
            )
            .expect("create status row");
    }
    let statuses = store
        .pending()
        .expect("pending rows")
        .into_iter()
        .map(|record| record.status)
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [String::from("in_progress"), String::from("pending")]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(statuses, expected);

    let imported = VideoJob::from_api(&json!({
        "id": "imported-1",
        "status": "in_progress"
    }))
    .expect("imported job");
    let record = store
        .import_job(&imported, None)
        .expect("import existing job");
    assert_eq!(record.polling_url, "/api/v1/videos/imported-1");
    assert!(record.request.is_none());
}

#[test]
fn history_schema_and_rows_never_persist_api_keys_or_remote_raw_payloads() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let store = HistoryStore::new(&database);
    store
        .create_job(&request("safe"), &job("job-secret", "pending", None))
        .expect("save row");

    let connection = Connection::open(&database).expect("inspect database");
    let mut statement = connection
        .prepare("PRAGMA table_info(jobs)")
        .expect("prepare schema query");
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("read columns");
    assert!(
        !columns
            .iter()
            .any(|name| { matches!(name.as_str(), "api_key" | "authorization" | "raw") })
    );
    let row_text = connection
        .query_row(
            "SELECT job_id, polling_url, status, request_json, generation_id, output_path, cost, error, created_at, updated_at FROM jobs WHERE job_id = 'job-secret'",
            [],
            |row| {
                let mut values = Vec::new();
                for index in 0..10 {
                    values.push(row.get::<_, Option<String>>(index)?);
                }
                Ok(format!("{values:?}"))
            },
        )
        .expect("read saved row");
    assert!(!row_text.contains("sk-secret-must-not-be-persisted"));
}

#[test]
fn malformed_python_request_json_does_not_break_listing() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let store = HistoryStore::new(&database);
    store
        .create_job(&request("safe"), &job("job-1", "pending", None))
        .expect("create row");
    Connection::open(&database)
        .expect("open database")
        .execute(
            "UPDATE jobs SET request_json = ?1, updated_at = ?2 WHERE job_id = ?3",
            ("{not valid json", "2099-01-01T00:00:00+00:00", "job-1"),
        )
        .expect("corrupt request fixture");

    let records = HistoryStore::new(&database).list(100).expect("list rows");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].job_id, "job-1");
    assert!(records[0].request.is_none());
}

#[test]
fn completed_job_fixture_maps_status_for_history_helpers() {
    let jobs = fixture("jobs.json");
    let completed = VideoJob::from_api(&jobs["completed"]).expect("completed fixture");
    assert_eq!(completed.status, JobStatus::Completed);
}

#[test]
fn provider_v2_migration_backs_up_and_preserves_the_legacy_schema() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let sql =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/history.sql"))
            .expect("read SQL fixture");
    Connection::open(&database)
        .expect("open fixture database")
        .execute_batch(&sql)
        .expect("load fixture");

    let store = HistoryStore::new(&database);
    store.initialize().expect("migrate to provider schema");
    assert!(store.pre_provider_v2_backup_path().is_file());

    let connection = Connection::open(&database).expect("inspect migrated database");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read schema version");
    assert_eq!(version, 2);
    let legacy_columns = connection
        .prepare("PRAGMA table_info(jobs)")
        .expect("prepare legacy schema")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query legacy columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("read legacy columns");
    assert_eq!(
        legacy_columns,
        [
            "job_id",
            "polling_url",
            "status",
            "request_json",
            "generation_id",
            "output_path",
            "cost",
            "error",
            "created_at",
            "updated_at",
        ]
    );
    let migrated: (i64, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT count(*),
                    max(CASE WHEN remote_job_id = 'job-pending-fixture' THEN currency END),
                    max(CASE WHEN remote_job_id = 'job-complete-fixture' THEN currency END)
             FROM generations WHERE provider_id = 'openrouter'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("inspect migration");
    assert_eq!(migrated, (2, None, Some("USD".into())));

    let backup = Connection::open(store.pre_provider_v2_backup_path()).expect("open v1 backup");
    let backup_version: i64 = backup
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read backup version");
    assert_eq!(backup_version, 0);
    let v2_tables: i64 = backup
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='generations'",
            [],
            |row| row.get(0),
        )
        .expect("inspect backup tables");
    assert_eq!(v2_tables, 0);
}

#[test]
fn startup_reconciles_newer_python_rows_into_openrouter_generations() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let store = HistoryStore::new(&database);
    store.initialize().expect("initialize v2");

    Connection::open(&database)
        .expect("open as legacy Python")
        .execute(
            "INSERT INTO jobs (
                 job_id, polling_url, status, request_json, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                "python-new",
                "/api/v1/videos/python-new",
                "in_progress",
                r#"{"model":"example/video-one","prompt":"written by Python"}"#,
                "2026-08-06T06:00:00+00:00",
                "2026-08-06T06:01:00+00:00",
            ),
        )
        .expect("write legacy row");

    let record = store
        .get("python-new")
        .expect("reconcile at startup")
        .expect("reconciled generation");
    assert_eq!(record.provider_id, ProviderId::openrouter());
    assert_eq!(record.status, "in_progress");
    assert_eq!(
        record
            .request
            .as_ref()
            .map(|request| request.prompt.as_str()),
        Some("written by Python")
    );
}

#[test]
fn composite_keys_isolate_fal_and_openrouter_and_fal_never_touches_jobs() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let store = HistoryStore::new(&database);
    let shared_id = "same-remote-id";
    store
        .create_job(&request("OpenRouter"), &job(shared_id, "pending", None))
        .expect("save OpenRouter job");
    let fal_request =
        VideoRequest::for_provider(ProviderId::fal(), "fal-ai/test-video", "fal request")
            .expect("fal request");
    store
        .create_provider_job(
            &ProviderId::fal(),
            &fal_request,
            &fal_job(shared_id, "in_progress", Some("1.25")),
        )
        .expect("save fal job");

    let records = store.list_generations(100).expect("list both providers");
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .any(|record| record.provider_id == ProviderId::fal())
    );
    let connection = Connection::open(&database).expect("inspect tables");
    let legacy_rows: i64 = connection
        .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
        .expect("count legacy rows");
    assert_eq!(legacy_rows, 1);
    let fal_record = records
        .iter()
        .find(|record| record.provider_id == ProviderId::fal())
        .expect("fal record");
    assert_eq!(fal_record.currency.as_deref(), Some("USD"));
    assert!(matches!(fal_record.locator, JobLocator::Fal { .. }));
}

#[test]
fn deleting_openrouter_history_removes_both_projections_without_resurrection() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let store = HistoryStore::new(&database);
    let key =
        ProviderJobKey::new(ProviderId::openrouter(), "delete-me").expect("OpenRouter history key");
    store
        .create_job(
            &request("A render to remove"),
            &job(&key.remote_job_id, "completed", Some("0.42")),
        )
        .expect("save OpenRouter job");

    assert!(store.delete_provider(&key).expect("delete history"));
    assert!(!store.delete_provider(&key).expect("repeat deletion"));

    let connection = Connection::open(&database).expect("inspect deleted history");
    let rows: (i64, i64) = connection
        .query_row(
            "SELECT
                 (SELECT count(*) FROM generations
                    WHERE provider_id = 'openrouter' AND remote_job_id = 'delete-me'),
                 (SELECT count(*) FROM jobs WHERE job_id = 'delete-me')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("count both history projections");
    assert_eq!(rows, (0, 0));
    drop(connection);

    let reopened = HistoryStore::new(&database);
    reopened.initialize().expect("reconcile reopened history");
    assert!(
        reopened
            .get_provider(&key)
            .expect("read reopened history")
            .is_none()
    );
}

#[test]
fn provider_qualified_deletion_preserves_the_same_remote_id_at_other_providers() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let store = HistoryStore::new(&database);
    let shared_id = "shared-delete-id";
    let openrouter_key =
        ProviderJobKey::new(ProviderId::openrouter(), shared_id).expect("OpenRouter key");
    let fal_key = ProviderJobKey::new(ProviderId::fal(), shared_id).expect("fal key");
    store
        .create_job(
            &request("OpenRouter copy"),
            &job(shared_id, "completed", None),
        )
        .expect("save OpenRouter job");
    let fal_request =
        VideoRequest::for_provider(ProviderId::fal(), "fal-ai/test-video", "fal copy")
            .expect("fal request");
    store
        .create_provider_job(
            &ProviderId::fal(),
            &fal_request,
            &fal_job(shared_id, "completed", None),
        )
        .expect("save fal job");

    assert!(
        store
            .delete_provider(&openrouter_key)
            .expect("delete only OpenRouter")
    );
    assert!(
        store
            .get_provider(&openrouter_key)
            .expect("query OpenRouter")
            .is_none()
    );
    assert!(store.get_provider(&fal_key).expect("query fal").is_some());

    store
        .create_job(
            &request("OpenRouter replacement"),
            &job(shared_id, "completed", None),
        )
        .expect("restore OpenRouter job");
    assert!(store.delete_provider(&fal_key).expect("delete only fal"));
    assert!(
        store
            .get_provider(&fal_key)
            .expect("query deleted fal")
            .is_none()
    );
    assert!(
        store
            .get_provider(&openrouter_key)
            .expect("query preserved OpenRouter")
            .is_some()
    );
}

#[test]
fn openrouter_generation_and_legacy_write_roll_back_together() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    let store = HistoryStore::new(&database);
    store.initialize().expect("initialize");
    Connection::open(&database)
        .expect("open database")
        .execute_batch(
            "CREATE TRIGGER reject_legacy_job BEFORE INSERT ON jobs
             BEGIN SELECT RAISE(ABORT, 'fixture rejection'); END;",
        )
        .expect("install rejection trigger");

    assert!(
        store
            .create_job(
                &request("transactional"),
                &job("rolled-back", "pending", None)
            )
            .is_err()
    );
    let connection = Connection::open(&database).expect("inspect rollback");
    let generation_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM generations WHERE remote_job_id = 'rolled-back'",
            [],
            |row| row.get(0),
        )
        .expect("count canonical rows");
    assert_eq!(generation_rows, 0);
}

#[test]
fn newer_history_schema_is_rejected_without_backup_or_mutation() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("history.sqlite3");
    Connection::open(&database)
        .expect("open future database")
        .execute_batch("PRAGMA user_version = 3; CREATE TABLE future_only (value TEXT);")
        .expect("create future schema");
    let store = HistoryStore::new(&database);
    assert!(matches!(
        store.initialize(),
        Err(HistoryError::UnsupportedSchemaVersion {
            found: 3,
            supported: 2
        })
    ));
    assert!(!store.pre_provider_v2_backup_path().exists());
    let connection = Connection::open(&database).expect("reopen future database");
    let generation_tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='generations'",
            [],
            |row| row.get(0),
        )
        .expect("inspect schema");
    assert_eq!(generation_tables, 0);
}
