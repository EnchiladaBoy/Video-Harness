//! Crash-safe, provider-aware SQLite history with Python/OpenRouter rollback compatibility.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::backup::Backup;
use rusqlite::types::Type;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use rust_decimal::Decimal;
use serde_json::Value;
use thiserror::Error;

use crate::domain::{
    DomainError, JobLocator, JobStatus, OPENROUTER_PROVIDER_ID, ProviderId, ProviderJobKey,
    VideoJob, VideoRequest,
};

const SCHEMA_VERSION: i64 = 2;
const DEFAULT_CURRENCY: &str = "USD";

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("could not access video history: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("could not create or back up video history: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not serialize the saved video request or job locator: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid provider history value: {0}")]
    Domain(#[from] DomainError),
    #[error("history schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("request, job, and history provider ids must match")]
    ProviderMismatch,
    #[error("submitted jobs must include an id and valid polling locator")]
    IncompleteSubmittedJob,
    #[error("imported jobs must include an id")]
    IncompleteImportedJob,
    #[error("history record disappeared after it was saved")]
    MissingSavedRecord,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobRecord {
    pub provider_id: ProviderId,
    pub job_id: String,
    /// Compatibility/display projection of `locator`.
    pub polling_url: String,
    pub locator: JobLocator,
    pub status: String,
    pub request: Option<VideoRequest>,
    pub generation_id: Option<String>,
    pub output_path: Option<PathBuf>,
    pub cost: Option<Decimal>,
    pub currency: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl JobRecord {
    pub fn id(&self) -> &str {
        &self.job_id
    }

    pub fn remote_id(&self) -> &str {
        &self.job_id
    }

    pub fn key(&self) -> ProviderJobKey {
        ProviderJobKey {
            provider_id: self.provider_id.clone(),
            remote_job_id: self.job_id.clone(),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.output_path.as_deref()
    }

    pub fn actual_cost(&self) -> Option<Decimal> {
        self.cost
    }

    pub fn terminal(&self) -> bool {
        JobStatus::from_raw(self.status.clone()).terminal()
    }
}

#[derive(Debug, Clone)]
pub struct HistoryStore {
    path: PathBuf,
}

impl HistoryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pre_rust_backup_path(&self) -> PathBuf {
        backup_path(&self.path, "pre-rust-v1")
    }

    pub fn pre_provider_v2_backup_path(&self) -> PathBuf {
        backup_path(&self.path, "pre-provider-v2")
    }

    fn connect(&self) -> Result<Connection, HistoryError> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }

    pub fn initialize(&self) -> Result<(), HistoryError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let existed = self.path.exists();
        if existed {
            let version = schema_version(&self.path)?;
            reject_newer_schema(version)?;
            if version < SCHEMA_VERSION {
                // Keep the original Python/native rollback snapshot and add a
                // separate snapshot immediately before the provider migration.
                self.ensure_pre_rust_backup()?;
                self.ensure_pre_provider_v2_backup()?;
            }
        }

        let mut connection = self.connect()?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        migrate_and_reconcile(&mut connection)
    }

    pub fn ensure_pre_rust_backup(&self) -> Result<Option<PathBuf>, HistoryError> {
        ensure_online_backup(&self.path, &self.pre_rust_backup_path())
    }

    pub fn ensure_pre_provider_v2_backup(&self) -> Result<Option<PathBuf>, HistoryError> {
        ensure_online_backup(&self.path, &self.pre_provider_v2_backup_path())
    }

    fn ready_connection(&self) -> Result<Connection, HistoryError> {
        self.initialize()?;
        self.connect()
    }

    /// Legacy OpenRouter wrapper retained for Rust v0.1 and Python parity.
    pub fn create_job(
        &self,
        request: &VideoRequest,
        job: &VideoJob,
    ) -> Result<JobRecord, HistoryError> {
        self.create_provider_job(&ProviderId::openrouter(), request, job)
    }

    pub fn create_provider_job(
        &self,
        provider_id: &ProviderId,
        request: &VideoRequest,
        job: &VideoJob,
    ) -> Result<JobRecord, HistoryError> {
        validate_provider_values(provider_id, Some(request), job)?;
        if job.id.trim().is_empty() || !locator_is_complete(&job.locator) {
            return Err(HistoryError::IncompleteSubmittedJob);
        }

        let now = Utc::now().to_rfc3339();
        let normalized_request = normalized_request_json(request)?;
        let locator_json = serde_json::to_string(&job.locator)?;
        let cost = job.cost().map(|value| value.to_string());
        let currency = currency_for_job(job);
        let mut connection = self.ready_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        upsert_generation(
            &transaction,
            provider_id,
            job,
            &locator_json,
            Some(&normalized_request),
            cost.as_deref(),
            currency.as_deref(),
            &now,
        )?;
        if provider_id.as_str() == OPENROUTER_PROVIDER_ID {
            let legacy_request = serde_json::to_string(request)?;
            upsert_legacy_job(
                &transaction,
                job,
                &job.polling_url,
                Some(&legacy_request),
                cost.as_deref(),
                &now,
            )?;
        }
        transaction.commit()?;
        self.get_provider(&job.key())?
            .ok_or(HistoryError::MissingSavedRecord)
    }

    /// Legacy OpenRouter wrapper retained for existing callers.
    pub fn import_job(
        &self,
        job: &VideoJob,
        request: Option<&VideoRequest>,
    ) -> Result<JobRecord, HistoryError> {
        self.import_provider_job(&ProviderId::openrouter(), job, request)
    }

    pub fn import_provider_job(
        &self,
        provider_id: &ProviderId,
        job: &VideoJob,
        request: Option<&VideoRequest>,
    ) -> Result<JobRecord, HistoryError> {
        if let Some(request) = request {
            return self.create_provider_job(provider_id, request, job);
        }
        if job.id.trim().is_empty() {
            return Err(HistoryError::IncompleteImportedJob);
        }
        if &job.provider_id != provider_id {
            return Err(HistoryError::ProviderMismatch);
        }

        let locator = import_locator(provider_id, job)?;
        let locator_json = serde_json::to_string(&locator)?;
        let polling_url = polling_url_for_locator(&locator);
        let now = Utc::now().to_rfc3339();
        let cost = job.cost().map(|value| value.to_string());
        let currency = currency_for_job(job);
        let mut connection = self.ready_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_generation(
            &transaction,
            provider_id,
            job,
            &locator_json,
            None,
            cost.as_deref(),
            currency.as_deref(),
            &now,
        )?;
        if provider_id.as_str() == OPENROUTER_PROVIDER_ID {
            upsert_legacy_job(&transaction, job, &polling_url, None, cost.as_deref(), &now)?;
        }
        transaction.commit()?;
        let key = ProviderJobKey::new(provider_id.clone(), job.id.clone())?;
        self.get_provider(&key)?
            .ok_or(HistoryError::MissingSavedRecord)
    }

    /// Legacy OpenRouter wrapper retained for existing callers.
    pub fn update_job(
        &self,
        job: &VideoJob,
        output_path: Option<&Path>,
    ) -> Result<JobRecord, HistoryError> {
        self.update_provider_job(&ProviderId::openrouter(), job, output_path)
    }

    pub fn update_provider_job(
        &self,
        provider_id: &ProviderId,
        job: &VideoJob,
        output_path: Option<&Path>,
    ) -> Result<JobRecord, HistoryError> {
        if &job.provider_id != provider_id || job.id.trim().is_empty() {
            return Err(HistoryError::ProviderMismatch);
        }
        let key = ProviderJobKey::new(provider_id.clone(), job.id.clone())?;
        let existing = match self.get_provider(&key)? {
            Some(existing) => existing,
            None => self.import_provider_job(provider_id, job, None)?,
        };
        let locator = if locator_is_complete(&job.locator) {
            if job.locator.provider_id() != *provider_id {
                return Err(HistoryError::ProviderMismatch);
            }
            job.locator.clone()
        } else {
            existing.locator
        };
        let polling_url = polling_url_for_locator(&locator);
        let locator_json = serde_json::to_string(&locator)?;
        let updated_at = Utc::now().to_rfc3339();
        let output = output_path.map(|value| value.to_string_lossy().into_owned());
        let cost = job.cost().map(|value| value.to_string());
        let currency = currency_for_job(job);
        let mut connection = self.ready_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE generations SET
                polling_locator = ?1,
                status = ?2,
                generation_id = COALESCE(?3, generation_id),
                output_path = COALESCE(?4, output_path),
                cost = COALESCE(?5, cost),
                currency = COALESCE(?6, currency),
                error = ?7,
                updated_at = ?8
             WHERE provider_id = ?9 AND remote_job_id = ?10",
            params![
                locator_json,
                job.status.as_str(),
                job.generation_id,
                output,
                cost,
                currency,
                job.error,
                updated_at,
                provider_id.as_str(),
                job.id,
            ],
        )?;
        if provider_id.as_str() == OPENROUTER_PROVIDER_ID {
            transaction.execute(
                "UPDATE jobs SET
                    polling_url = ?1,
                    status = ?2,
                    generation_id = COALESCE(?3, generation_id),
                    output_path = COALESCE(?4, output_path),
                    cost = COALESCE(?5, cost),
                    error = ?6,
                    updated_at = ?7
                 WHERE job_id = ?8",
                params![
                    polling_url,
                    job.status.as_str(),
                    job.generation_id,
                    output_path.map(|value| value.to_string_lossy().into_owned()),
                    job.cost().map(|value| value.to_string()),
                    job.error,
                    updated_at,
                    job.id,
                ],
            )?;
        }
        transaction.commit()?;
        self.get_provider(&key)?
            .ok_or(HistoryError::MissingSavedRecord)
    }

    pub fn mark_downloaded(
        &self,
        job: &VideoJob,
        output_path: &Path,
    ) -> Result<JobRecord, HistoryError> {
        self.update_job(job, Some(output_path))
    }

    pub fn mark_provider_downloaded(
        &self,
        provider_id: &ProviderId,
        job: &VideoJob,
        output_path: &Path,
    ) -> Result<JobRecord, HistoryError> {
        self.update_provider_job(provider_id, job, Some(output_path))
    }

    /// Legacy OpenRouter lookup, now projected from the canonical v2 table.
    pub fn get(&self, job_id: &str) -> Result<Option<JobRecord>, HistoryError> {
        let key = ProviderJobKey::new(ProviderId::openrouter(), job_id)?;
        self.get_provider(&key)
    }

    pub fn get_provider(&self, key: &ProviderJobKey) -> Result<Option<JobRecord>, HistoryError> {
        let connection = self.ready_connection()?;
        connection
            .query_row(
                "SELECT provider_id, remote_job_id, polling_locator, status,
                        request_json, generation_id, output_path, cost, currency,
                        error, created_at, updated_at
                 FROM generations
                 WHERE provider_id = ?1 AND remote_job_id = ?2",
                params![key.provider_id.as_str(), key.remote_job_id],
                row_to_record,
            )
            .optional()
            .map_err(HistoryError::from)
    }

    /// Legacy OpenRouter-only listing.
    pub fn list(&self, limit: usize) -> Result<Vec<JobRecord>, HistoryError> {
        self.list_provider(&ProviderId::openrouter(), limit)
    }

    pub fn list_records(&self, limit: usize) -> Result<Vec<JobRecord>, HistoryError> {
        self.list(limit)
    }

    pub fn list_provider(
        &self,
        provider_id: &ProviderId,
        limit: usize,
    ) -> Result<Vec<JobRecord>, HistoryError> {
        query_records(
            &self.ready_connection()?,
            "SELECT provider_id, remote_job_id, polling_locator, status,
                    request_json, generation_id, output_path, cost, currency,
                    error, created_at, updated_at
             FROM generations WHERE provider_id = ?1
             ORDER BY created_at DESC LIMIT ?2",
            params![provider_id.as_str(), safe_limit(limit)],
        )
    }

    pub fn list_generations(&self, limit: usize) -> Result<Vec<JobRecord>, HistoryError> {
        query_records(
            &self.ready_connection()?,
            "SELECT provider_id, remote_job_id, polling_locator, status,
                    request_json, generation_id, output_path, cost, currency,
                    error, created_at, updated_at
             FROM generations ORDER BY created_at DESC LIMIT ?1",
            params![safe_limit(limit)],
        )
    }

    /// Legacy OpenRouter-only pending listing.
    pub fn pending(&self) -> Result<Vec<JobRecord>, HistoryError> {
        self.pending_provider(&ProviderId::openrouter())
    }

    pub fn pending_provider(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Vec<JobRecord>, HistoryError> {
        query_records(
            &self.ready_connection()?,
            "SELECT provider_id, remote_job_id, polling_locator, status,
                    request_json, generation_id, output_path, cost, currency,
                    error, created_at, updated_at
             FROM generations
             WHERE provider_id = ?1
               AND status NOT IN ('completed', 'failed', 'cancelled', 'expired')
             ORDER BY created_at DESC",
            params![provider_id.as_str()],
        )
    }

    pub fn pending_generations(&self) -> Result<Vec<JobRecord>, HistoryError> {
        query_records(
            &self.ready_connection()?,
            "SELECT provider_id, remote_job_id, polling_locator, status,
                    request_json, generation_id, output_path, cost, currency,
                    error, created_at, updated_at
             FROM generations
             WHERE status NOT IN ('completed', 'failed', 'cancelled', 'expired')
             ORDER BY created_at DESC",
            [],
        )
    }
}

fn reject_newer_schema(version: i64) -> Result<(), HistoryError> {
    if version > SCHEMA_VERSION {
        Err(HistoryError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        })
    } else {
        Ok(())
    }
}

fn schema_version(path: &Path) -> Result<i64, HistoryError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn migrate_and_reconcile(connection: &mut Connection) -> Result<(), HistoryError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    reject_newer_schema(version)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS jobs (
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
         CREATE INDEX IF NOT EXISTS jobs_updated_idx ON jobs(updated_at DESC);
         CREATE TABLE IF NOT EXISTS generations (
             provider_id TEXT NOT NULL,
             remote_job_id TEXT NOT NULL,
             polling_locator TEXT NOT NULL,
             status TEXT NOT NULL,
             request_json TEXT,
             generation_id TEXT,
             output_path TEXT,
             cost TEXT,
             currency TEXT,
             error TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             PRIMARY KEY (provider_id, remote_job_id)
         );
         CREATE INDEX IF NOT EXISTS generations_updated_idx
             ON generations(updated_at DESC);",
    )?;
    reconcile_legacy_jobs(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

#[derive(Debug)]
struct LegacyRow {
    job_id: String,
    polling_url: String,
    status: String,
    request_json: Option<String>,
    generation_id: Option<String>,
    output_path: Option<String>,
    cost: Option<String>,
    error: Option<String>,
    created_at: String,
    updated_at: String,
}

fn reconcile_legacy_jobs(transaction: &Transaction<'_>) -> Result<(), HistoryError> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT job_id, polling_url, status, request_json, generation_id,
                    output_path, cost, error, created_at, updated_at FROM jobs",
        )?;
        statement
            .query_map([], |row| {
                Ok(LegacyRow {
                    job_id: row.get(0)?,
                    polling_url: row.get(1)?,
                    status: row.get(2)?,
                    request_json: row.get(3)?,
                    generation_id: row.get(4)?,
                    output_path: row.get(5)?,
                    cost: row.get(6)?,
                    error: row.get(7)?,
                    created_at: row.get(8)?,
                    updated_at: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    for row in rows {
        let locator = serde_json::to_string(&JobLocator::OpenRouter {
            polling_url: row.polling_url,
        })?;
        let request_json = normalize_legacy_request(row.request_json);
        let currency = row.cost.as_ref().map(|_| DEFAULT_CURRENCY);
        transaction.execute(
            "INSERT INTO generations (
                 provider_id, remote_job_id, polling_locator, status, request_json,
                 generation_id, output_path, cost, currency, error, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(provider_id, remote_job_id) DO UPDATE SET
                 polling_locator = excluded.polling_locator,
                 status = excluded.status,
                 request_json = COALESCE(excluded.request_json, generations.request_json),
                 generation_id = COALESCE(excluded.generation_id, generations.generation_id),
                 output_path = COALESCE(excluded.output_path, generations.output_path),
                 cost = COALESCE(excluded.cost, generations.cost),
                 currency = COALESCE(excluded.currency, generations.currency),
                 error = excluded.error,
                 updated_at = excluded.updated_at
             WHERE excluded.updated_at > generations.updated_at",
            params![
                OPENROUTER_PROVIDER_ID,
                row.job_id,
                locator,
                row.status,
                request_json,
                row.generation_id,
                row.output_path,
                row.cost,
                currency,
                row.error,
                row.created_at,
                row.updated_at,
            ],
        )?;
    }
    Ok(())
}

fn normalize_legacy_request(request_json: Option<String>) -> Option<String> {
    let text = request_json?;
    let Ok(mut request) = serde_json::from_str::<VideoRequest>(&text) else {
        return Some(text);
    };
    request.provider_id = ProviderId::openrouter();
    normalized_request_json(&request).ok().or(Some(text))
}

fn normalized_request_json(request: &VideoRequest) -> Result<String, HistoryError> {
    let mut value = request.to_payload()?;
    let object = value.as_object_mut().ok_or_else(|| {
        HistoryError::Domain(DomainError::Validation(
            "normalized video request must be an object".into(),
        ))
    })?;
    if let Some(adapter_options) = object.remove("provider") {
        object.insert("adapter_options".into(), adapter_options);
    }
    object.insert(
        "provider_id".into(),
        Value::String(request.provider_id.as_str().into()),
    );
    Ok(serde_json::to_string(&value)?)
}

fn validate_provider_values(
    provider_id: &ProviderId,
    request: Option<&VideoRequest>,
    job: &VideoJob,
) -> Result<(), HistoryError> {
    if &job.provider_id != provider_id
        || request.is_some_and(|request| &request.provider_id != provider_id)
        || job.locator.provider_id() != *provider_id
    {
        return Err(HistoryError::ProviderMismatch);
    }
    ProviderJobKey::new(provider_id.clone(), job.id.clone())?;
    job.locator.validate()?;
    Ok(())
}

fn import_locator(provider_id: &ProviderId, job: &VideoJob) -> Result<JobLocator, HistoryError> {
    let locator = if provider_id.as_str() == OPENROUTER_PROVIDER_ID && job.polling_url.is_empty() {
        JobLocator::OpenRouter {
            polling_url: format!("/api/v1/videos/{}", job.id),
        }
    } else {
        job.locator.clone()
    };
    if locator.provider_id() != *provider_id {
        return Err(HistoryError::ProviderMismatch);
    }
    locator.validate()?;
    Ok(locator)
}

fn locator_is_complete(locator: &JobLocator) -> bool {
    locator.validate().is_ok()
}

fn polling_url_for_locator(locator: &JobLocator) -> String {
    match locator {
        JobLocator::OpenRouter { polling_url } => polling_url.clone(),
        JobLocator::Fal {
            endpoint_id,
            request_id,
            status_url,
            response_url,
        } => status_url
            .clone()
            .or_else(|| response_url.clone())
            .unwrap_or_else(|| format!("{endpoint_id}/requests/{request_id}")),
    }
}

fn currency_for_job(job: &VideoJob) -> Option<String> {
    job.usage
        .get("currency")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_ascii_uppercase())
        .or_else(|| job.cost().is_some().then(|| DEFAULT_CURRENCY.into()))
}

#[allow(clippy::too_many_arguments)]
fn upsert_generation(
    transaction: &Transaction<'_>,
    provider_id: &ProviderId,
    job: &VideoJob,
    locator_json: &str,
    request_json: Option<&str>,
    cost: Option<&str>,
    currency: Option<&str>,
    now: &str,
) -> Result<(), HistoryError> {
    transaction.execute(
        "INSERT INTO generations (
             provider_id, remote_job_id, polling_locator, status, request_json,
             generation_id, output_path, cost, currency, error, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?10)
         ON CONFLICT(provider_id, remote_job_id) DO UPDATE SET
             polling_locator = excluded.polling_locator,
             status = excluded.status,
             request_json = COALESCE(generations.request_json, excluded.request_json),
             generation_id = COALESCE(excluded.generation_id, generations.generation_id),
             cost = COALESCE(excluded.cost, generations.cost),
             currency = COALESCE(excluded.currency, generations.currency),
             error = excluded.error,
             updated_at = excluded.updated_at",
        params![
            provider_id.as_str(),
            job.id,
            locator_json,
            job.status.as_str(),
            request_json,
            job.generation_id,
            cost,
            currency,
            job.error,
            now,
        ],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn upsert_legacy_job(
    transaction: &Transaction<'_>,
    job: &VideoJob,
    polling_url: &str,
    request_json: Option<&str>,
    cost: Option<&str>,
    now: &str,
) -> Result<(), HistoryError> {
    transaction.execute(
        "INSERT INTO jobs (
             job_id, polling_url, status, request_json, generation_id,
             output_path, cost, error, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?8)
         ON CONFLICT(job_id) DO UPDATE SET
             polling_url = excluded.polling_url,
             status = excluded.status,
             request_json = COALESCE(jobs.request_json, excluded.request_json),
             generation_id = COALESCE(excluded.generation_id, jobs.generation_id),
             cost = COALESCE(excluded.cost, jobs.cost),
             error = excluded.error,
             updated_at = excluded.updated_at",
        params![
            job.id,
            polling_url,
            job.status.as_str(),
            request_json,
            job.generation_id,
            cost,
            job.error,
            now,
        ],
    )?;
    Ok(())
}

fn query_records<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<JobRecord>, HistoryError> {
    let mut statement = connection.prepare(sql)?;
    Ok(statement
        .query_map(params, row_to_record)?
        .collect::<Result<Vec<_>, _>>()?)
}

fn row_to_record(row: &Row<'_>) -> rusqlite::Result<JobRecord> {
    let provider_text: String = row.get("provider_id")?;
    let provider_id = ProviderId::new(provider_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
    })?;
    let locator_text: String = row.get("polling_locator")?;
    let locator = serde_json::from_str::<JobLocator>(&locator_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
    })?;
    if locator.provider_id() != provider_id {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let request_json: Option<String> = row.get("request_json")?;
    let request = request_json.and_then(|text| {
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| VideoRequest::from_payload(&value).ok())
            .map(|mut request| {
                request.provider_id = provider_id.clone();
                request
            })
    });
    let cost_text: Option<String> = row.get("cost")?;
    let created_text: String = row.get("created_at")?;
    let updated_text: String = row.get("updated_at")?;
    Ok(JobRecord {
        provider_id,
        job_id: row.get("remote_job_id")?,
        polling_url: polling_url_for_locator(&locator),
        locator,
        status: row.get("status")?,
        request,
        generation_id: row.get("generation_id")?,
        output_path: row
            .get::<_, Option<String>>("output_path")?
            .map(PathBuf::from),
        cost: cost_text.and_then(|value| Decimal::from_str(&value).ok()),
        currency: row.get("currency")?,
        error: row.get("error")?,
        created_at: parse_time(&created_text),
        updated_at: parse_time(&updated_text),
    })
}

fn safe_limit(limit: usize) -> i64 {
    limit.clamp(1, 5_000) as i64
}

fn parse_time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn backup_path(database: &Path, label: &str) -> PathBuf {
    let file_name = database
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("history.sqlite3");
    database.with_file_name(format!("{file_name}.{label}.bak"))
}

fn ensure_online_backup(
    source_path: &Path,
    backup_path: &Path,
) -> Result<Option<PathBuf>, HistoryError> {
    if !source_path.exists() {
        return Ok(None);
    }
    if backup_path.exists() {
        return Ok(Some(backup_path.to_owned()));
    }
    let temporary = unique_backup_temporary(backup_path);
    let result = (|| -> Result<(), HistoryError> {
        let source = Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut destination = Connection::open(&temporary)?;
        {
            let backup = Backup::new(&source, &mut destination)?;
            backup.run_to_completion(128, Duration::from_millis(10), None)?;
        }
        drop(destination);
        drop(source);
        match fs::hard_link(&temporary, backup_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(HistoryError::Io(error)),
        }
        Ok(())
    })();
    let cleanup = fs::remove_file(&temporary);
    result?;
    cleanup?;
    Ok(Some(backup_path.to_owned()))
}

fn unique_backup_temporary(backup_path: &Path) -> PathBuf {
    let name = backup_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("history.sqlite3.pre-provider-v2.bak");
    let process = std::process::id();
    for index in 0u32.. {
        let candidate = backup_path.with_file_name(format!("{name}.tmp-{process}-{index}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u32 temporary backup suffixes exhausted")
}
