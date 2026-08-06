//! GUI-only persistence kept separate from the compatible generation history.
//!
//! This database deliberately stores references to source media, never copies
//! of source files. Credentials belong in [`crate::credentials`] and are
//! rejected if they appear in draft settings.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::domain::{
    GenerationDraft, JobLocator, MediaRole, MediaSource, ProviderId, ProviderJobKey,
};

const SCHEMA_VERSION: i64 = 1;
const DEFAULT_DRAFT_ID: &str = "current";
const DRAFT_EDITOR_STATE_KEY: &str = "__video_harness_editor_v1";
const MAX_SEED_TEXT_BYTES: usize = 1_024;
const MAX_ADVANCED_JSON_TEXT_BYTES: usize = 512 * 1_024;
const MAX_SCHEMA_TEXT_FIELDS: usize = 256;
const MAX_SCHEMA_FIELD_NAME_BYTES: usize = 512;
const MAX_SCHEMA_TEXT_VALUE_BYTES: usize = 64 * 1_024;
pub const UNCERTAIN_SUBMISSION_MESSAGE: &str = "A paid submission may have reached this provider. Check the provider dashboard before allowing this exact draft again.";

#[derive(Debug, Error)]
pub enum GuiStateError {
    #[error("could not access GUI state: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("could not create the GUI state directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not serialize GUI state: {0}")]
    Json(#[from] serde_json::Error),
    #[error("GUI state schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("draft settings may not contain credential fields")]
    CredentialInDraft,
    #[error("invalid GUI state value: {0}")]
    InvalidValue(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum StoredMediaSource {
    LocalFile(PathBuf),
    RemoteUrl(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDraftMedia {
    pub id: String,
    pub role: String,
    pub source: StoredMediaSource,
}

/// Exact text held by composer widgets alongside the typed generation draft.
///
/// These values may be temporarily invalid and therefore must never enter the
/// provider-facing [`GenerationDraft`]. Debug output is deliberately redacted
/// because an editor can contain text pasted by the user before validation.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftEditorState {
    #[serde(default)]
    pub seed_text: String,
    #[serde(default)]
    pub advanced_json_text: String,
    #[serde(default)]
    pub schema_text: BTreeMap<String, String>,
}

impl fmt::Debug for DraftEditorState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DraftEditorState")
            .field("seed_text", &"[REDACTED]")
            .field("advanced_json_text", &"[REDACTED]")
            .field("schema_text_fields", &self.schema_text.len())
            .finish()
    }
}

impl DraftEditorState {
    pub fn validate(&self) -> Result<(), GuiStateError> {
        validate_editor_text("seed", &self.seed_text, MAX_SEED_TEXT_BYTES)?;
        validate_editor_text(
            "advanced JSON",
            &self.advanced_json_text,
            MAX_ADVANCED_JSON_TEXT_BYTES,
        )?;
        if self.schema_text.len() > MAX_SCHEMA_TEXT_FIELDS {
            return Err(GuiStateError::InvalidValue(
                "draft has too many schema editor fields".into(),
            ));
        }
        if raw_text_contains_credential_field(&self.seed_text)
            || raw_text_contains_credential_field(&self.advanced_json_text)
        {
            return Err(GuiStateError::CredentialInDraft);
        }
        for (name, value) in &self.schema_text {
            validate_editor_text("schema field name", name, MAX_SCHEMA_FIELD_NAME_BYTES)?;
            validate_editor_text("schema field value", value, MAX_SCHEMA_TEXT_VALUE_BYTES)?;
            if name.trim().is_empty() {
                return Err(GuiStateError::InvalidValue(
                    "draft schema editor field name cannot be empty".into(),
                ));
            }
            if is_credential_field_name(name) || raw_text_contains_credential_field(value) {
                return Err(GuiStateError::CredentialInDraft);
            }
        }
        Ok(())
    }

    /// Check all raw editor keys and values without exposing `secret` in an
    /// error or Debug representation. Empty values are not credentials.
    pub fn contains_secret(&self, secret: &str) -> bool {
        !secret.is_empty()
            && (self.seed_text.contains(secret)
                || self.advanced_json_text.contains(secret)
                || self
                    .schema_text
                    .iter()
                    .any(|(name, value)| name.contains(secret) || value.contains(secret)))
    }
}

/// Autosaved editing state. It contains local paths and URLs, but never media
/// bytes. `settings` is the provider/model controls JSON shown in the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredDraft {
    pub revision: u64,
    pub provider_id: ProviderId,
    pub model_id: String,
    pub prompt: String,
    #[serde(default = "empty_object")]
    pub settings: Value,
    /// Missing for legacy rows written before exact editor text was retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_state: Option<DraftEditorState>,
    #[serde(default)]
    pub media: Vec<StoredDraftMedia>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUploadReceipt {
    pub provider_id: ProviderId,
    pub source_sha256: String,
    pub source_path: PathBuf,
    pub remote_url: String,
    pub content_type: String,
    pub byte_length: u64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl StoredUploadReceipt {
    pub fn usable_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationMediaAssociation {
    pub key: ProviderJobKey,
    pub position: usize,
    pub draft_media_id: String,
    pub role: String,
    pub source: StoredMediaSource,
    pub resolved_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumableJob {
    pub key: ProviderJobKey,
    pub locator: JobLocator,
    pub accepted_at: DateTime<Utc>,
    pub monitoring_paused: bool,
}

/// A credential-free, digest-only paid-submission safety barrier.
///
/// The fingerprint covers every editable billable input, including reference
/// identities, but the serialized inputs themselves are never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncertainSubmissionRecord {
    pub provider_id: ProviderId,
    pub draft_fingerprint: String,
    pub recorded_at: DateTime<Utc>,
    pub message: String,
}

impl UncertainSubmissionRecord {
    pub fn new(
        provider_id: ProviderId,
        draft_fingerprint: impl Into<String>,
        recorded_at: DateTime<Utc>,
    ) -> Self {
        Self {
            provider_id,
            draft_fingerprint: draft_fingerprint.into(),
            recorded_at,
            message: UNCERTAIN_SUBMISSION_MESSAGE.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginUncertainSubmission {
    Inserted(UncertainSubmissionRecord),
    Existing(UncertainSubmissionRecord),
}

#[derive(Debug, Clone)]
pub struct GuiStateStore {
    path: PathBuf,
}

impl GuiStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn connect(&self) -> Result<Connection, GuiStateError> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }

    pub fn initialize(&self) -> Result<(), GuiStateError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut connection = self.connect()?;
        #[cfg(unix)]
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(GuiStateError::UnsupportedSchemaVersion {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS drafts (
                 id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL,
                 provider_id TEXT NOT NULL,
                 model_id TEXT NOT NULL,
                 prompt TEXT NOT NULL,
                 settings_json TEXT NOT NULL,
                 media_json TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS upload_receipts (
                 provider_id TEXT NOT NULL,
                 source_sha256 TEXT NOT NULL,
                 source_path TEXT NOT NULL,
                 remote_url TEXT NOT NULL,
                 content_type TEXT NOT NULL,
                 byte_length INTEGER NOT NULL,
                 created_at TEXT NOT NULL,
                 expires_at TEXT NOT NULL,
                 PRIMARY KEY (provider_id, source_sha256)
             );
             CREATE TABLE IF NOT EXISTS generation_media (
                 provider_id TEXT NOT NULL,
                 remote_job_id TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 draft_media_id TEXT NOT NULL,
                 role TEXT NOT NULL,
                 source_json TEXT NOT NULL,
                 resolved_url TEXT NOT NULL,
                 PRIMARY KEY (provider_id, remote_job_id, position)
             );
             CREATE TABLE IF NOT EXISTS resumable_jobs (
                 provider_id TEXT NOT NULL,
                 remote_job_id TEXT NOT NULL,
                 locator_json TEXT NOT NULL,
                 accepted_at TEXT NOT NULL,
                 monitoring_paused INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (provider_id, remote_job_id)
             );
             CREATE TABLE IF NOT EXISTS uncertain_submissions (
                 provider_id TEXT NOT NULL,
                 draft_fingerprint TEXT NOT NULL,
                 recorded_at TEXT NOT NULL,
                 message TEXT NOT NULL,
                 PRIMARY KEY (provider_id, draft_fingerprint)
             );",
        )?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(())
    }

    fn ready_connection(&self) -> Result<Connection, GuiStateError> {
        self.initialize()?;
        self.connect()
    }

    pub fn save_draft(&self, draft: &StoredDraft) -> Result<(), GuiStateError> {
        validate_draft(draft)?;
        let settings_json = serde_json::to_string(&encoded_draft_settings(draft)?)?;
        let media_json = serde_json::to_string(&draft.media)?;
        let revision = i64::try_from(draft.revision)
            .map_err(|_| GuiStateError::InvalidValue("draft revision is too large".into()))?;
        let connection = self.ready_connection()?;
        connection.execute(
            "INSERT INTO drafts
                 (id, revision, provider_id, model_id, prompt, settings_json, media_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 revision = excluded.revision,
                 provider_id = excluded.provider_id,
                 model_id = excluded.model_id,
                 prompt = excluded.prompt,
                 settings_json = excluded.settings_json,
                 media_json = excluded.media_json,
                 updated_at = excluded.updated_at",
            params![
                DEFAULT_DRAFT_ID,
                revision,
                draft.provider_id.as_str(),
                draft.model_id,
                draft.prompt,
                settings_json,
                media_json,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn load_draft(&self) -> Result<Option<StoredDraft>, GuiStateError> {
        let connection = self.ready_connection()?;
        let row = connection
            .query_row(
                "SELECT revision, provider_id, model_id, prompt, settings_json, media_json
                 FROM drafts WHERE id = ?1",
                [DEFAULT_DRAFT_ID],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((revision, provider_id, model_id, prompt, settings_json, media_json)) = row else {
            return Ok(None);
        };
        let persisted_settings: Value = serde_json::from_str(&settings_json)?;
        let (settings, editor_state) = decoded_draft_settings(persisted_settings)?;
        let draft = StoredDraft {
            revision: u64::try_from(revision)
                .map_err(|_| GuiStateError::InvalidValue("negative draft revision".into()))?,
            provider_id: ProviderId::new(provider_id)?,
            model_id,
            prompt,
            settings,
            editor_state,
            media: serde_json::from_str(&media_json)?,
        };
        validate_draft(&draft)?;
        Ok(Some(draft))
    }

    pub fn save_upload_receipt(&self, receipt: &StoredUploadReceipt) -> Result<(), GuiStateError> {
        validate_receipt(receipt)?;
        let byte_length = i64::try_from(receipt.byte_length)
            .map_err(|_| GuiStateError::InvalidValue("upload byte length is too large".into()))?;
        let connection = self.ready_connection()?;
        connection.execute(
            "INSERT INTO upload_receipts
                 (provider_id, source_sha256, source_path, remote_url, content_type,
                  byte_length, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(provider_id, source_sha256) DO UPDATE SET
                 source_path = excluded.source_path,
                 remote_url = excluded.remote_url,
                 content_type = excluded.content_type,
                 byte_length = excluded.byte_length,
                 created_at = excluded.created_at,
                 expires_at = excluded.expires_at",
            params![
                receipt.provider_id.as_str(),
                receipt.source_sha256,
                receipt.source_path.to_string_lossy(),
                receipt.remote_url,
                receipt.content_type,
                byte_length,
                receipt.created_at.to_rfc3339(),
                receipt.expires_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn usable_upload_receipt(
        &self,
        provider_id: &ProviderId,
        source_sha256: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<StoredUploadReceipt>, GuiStateError> {
        let connection = self.ready_connection()?;
        let row = connection
            .query_row(
                "SELECT source_path, remote_url, content_type, byte_length, created_at, expires_at
                 FROM upload_receipts WHERE provider_id = ?1 AND source_sha256 = ?2",
                params![provider_id.as_str(), source_sha256],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((source_path, remote_url, content_type, byte_length, created_at, expires_at)) =
            row
        else {
            return Ok(None);
        };
        let receipt = StoredUploadReceipt {
            provider_id: provider_id.clone(),
            source_sha256: source_sha256.to_owned(),
            source_path: PathBuf::from(source_path),
            remote_url,
            content_type,
            byte_length: u64::try_from(byte_length)
                .map_err(|_| GuiStateError::InvalidValue("negative upload byte length".into()))?,
            created_at: parse_timestamp(&created_at)?,
            expires_at: parse_timestamp(&expires_at)?,
        };
        Ok(receipt.usable_at(now).then_some(receipt))
    }

    /// Return the newest unexpired receipt for a local path. The provider
    /// re-hashes the file before reuse, so a changed file cannot reuse this
    /// candidate merely because its path stayed the same.
    pub fn usable_upload_receipt_for_path(
        &self,
        provider_id: &ProviderId,
        source_path: &Path,
        now: DateTime<Utc>,
    ) -> Result<Option<StoredUploadReceipt>, GuiStateError> {
        let connection = self.ready_connection()?;
        let row = connection
            .query_row(
                "SELECT source_sha256, remote_url, content_type, byte_length, created_at, expires_at
                 FROM upload_receipts
                 WHERE provider_id = ?1 AND source_path = ?2 AND expires_at > ?3
                 ORDER BY created_at DESC LIMIT 1",
                params![
                    provider_id.as_str(),
                    source_path.to_string_lossy(),
                    now.to_rfc3339(),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((source_sha256, remote_url, content_type, byte_length, created_at, expires_at)) =
            row
        else {
            return Ok(None);
        };
        Ok(Some(StoredUploadReceipt {
            provider_id: provider_id.clone(),
            source_sha256,
            source_path: source_path.to_path_buf(),
            remote_url,
            content_type,
            byte_length: u64::try_from(byte_length)
                .map_err(|_| GuiStateError::InvalidValue("negative upload byte length".into()))?,
            created_at: parse_timestamp(&created_at)?,
            expires_at: parse_timestamp(&expires_at)?,
        }))
    }

    pub fn replace_generation_media(
        &self,
        key: &ProviderJobKey,
        media: &[GenerationMediaAssociation],
    ) -> Result<(), GuiStateError> {
        if media.iter().any(|item| item.key != *key) {
            return Err(GuiStateError::InvalidValue(
                "generation media must belong to the same provider job".into(),
            ));
        }
        let mut connection = self.ready_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM generation_media WHERE provider_id = ?1 AND remote_job_id = ?2",
            params![key.provider_id.as_str(), key.remote_job_id],
        )?;
        for item in media {
            let position = i64::try_from(item.position)
                .map_err(|_| GuiStateError::InvalidValue("media position is too large".into()))?;
            transaction.execute(
                "INSERT INTO generation_media
                     (provider_id, remote_job_id, position, draft_media_id, role, source_json, resolved_url)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    key.provider_id.as_str(),
                    key.remote_job_id,
                    position,
                    item.draft_media_id,
                    item.role,
                    serde_json::to_string(&item.source)?,
                    item.resolved_url,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn generation_media(
        &self,
        key: &ProviderJobKey,
    ) -> Result<Vec<GenerationMediaAssociation>, GuiStateError> {
        let connection = self.ready_connection()?;
        let mut statement = connection.prepare(
            "SELECT position, draft_media_id, role, source_json, resolved_url
             FROM generation_media WHERE provider_id = ?1 AND remote_job_id = ?2
             ORDER BY position ASC",
        )?;
        let rows = statement.query_map(
            params![key.provider_id.as_str(), key.remote_job_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?;
        let mut media = Vec::new();
        for row in rows {
            let (position, draft_media_id, role, source_json, resolved_url) = row?;
            media.push(GenerationMediaAssociation {
                key: key.clone(),
                position: usize::try_from(position)
                    .map_err(|_| GuiStateError::InvalidValue("negative media position".into()))?,
                draft_media_id,
                role,
                source: serde_json::from_str(&source_json)?,
                resolved_url,
            });
        }
        Ok(media)
    }

    pub fn save_resumable_job(&self, job: &ResumableJob) -> Result<(), GuiStateError> {
        if job.locator.provider_id() != job.key.provider_id
            || job.locator.remote_job_id() != job.key.remote_job_id
        {
            return Err(GuiStateError::InvalidValue(
                "resumable job key and locator do not match".into(),
            ));
        }
        let connection = self.ready_connection()?;
        connection.execute(
            "INSERT INTO resumable_jobs
                 (provider_id, remote_job_id, locator_json, accepted_at, monitoring_paused)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider_id, remote_job_id) DO UPDATE SET
                 locator_json = excluded.locator_json,
                 accepted_at = excluded.accepted_at,
                 monitoring_paused = excluded.monitoring_paused",
            params![
                job.key.provider_id.as_str(),
                job.key.remote_job_id,
                serde_json::to_string(&job.locator)?,
                job.accepted_at.to_rfc3339(),
                job.monitoring_paused,
            ],
        )?;
        Ok(())
    }

    pub fn set_monitoring_paused(
        &self,
        key: &ProviderJobKey,
        paused: bool,
    ) -> Result<bool, GuiStateError> {
        let connection = self.ready_connection()?;
        let changed = connection.execute(
            "UPDATE resumable_jobs SET monitoring_paused = ?3
             WHERE provider_id = ?1 AND remote_job_id = ?2",
            params![key.provider_id.as_str(), key.remote_job_id, paused],
        )?;
        Ok(changed > 0)
    }

    pub fn remove_resumable_job(&self, key: &ProviderJobKey) -> Result<bool, GuiStateError> {
        let connection = self.ready_connection()?;
        let changed = connection.execute(
            "DELETE FROM resumable_jobs WHERE provider_id = ?1 AND remote_job_id = ?2",
            params![key.provider_id.as_str(), key.remote_job_id],
        )?;
        Ok(changed > 0)
    }

    pub fn resumable_jobs(&self) -> Result<Vec<ResumableJob>, GuiStateError> {
        let connection = self.ready_connection()?;
        let mut statement = connection.prepare(
            "SELECT provider_id, remote_job_id, locator_json, accepted_at, monitoring_paused
             FROM resumable_jobs ORDER BY accepted_at ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            let (provider_id, remote_job_id, locator_json, accepted_at, monitoring_paused) = row?;
            let provider_id = ProviderId::new(provider_id)?;
            let locator: JobLocator = serde_json::from_str(&locator_json)?;
            let job = ResumableJob {
                key: ProviderJobKey {
                    provider_id,
                    remote_job_id,
                },
                locator,
                accepted_at: parse_timestamp(&accepted_at)?,
                monitoring_paused,
            };
            if job.locator.provider_id() != job.key.provider_id
                || job.locator.remote_job_id() != job.key.remote_job_id
            {
                return Err(GuiStateError::InvalidValue(
                    "saved resumable job key and locator do not match".into(),
                ));
            }
            jobs.push(job);
        }
        Ok(jobs)
    }

    /// Atomically create the pre-submit outbox row or return the unresolved
    /// row that already blocks this exact provider/draft pair.
    pub fn begin_uncertain_submission(
        &self,
        record: &UncertainSubmissionRecord,
    ) -> Result<BeginUncertainSubmission, GuiStateError> {
        validate_uncertain_submission(record)?;
        let mut connection = self.ready_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO uncertain_submissions
                 (provider_id, draft_fingerprint, recorded_at, message)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                record.provider_id.as_str(),
                record.draft_fingerprint,
                record.recorded_at.to_rfc3339(),
                record.message,
            ],
        )? > 0;
        let saved = query_uncertain_submission(
            &transaction,
            &record.provider_id,
            &record.draft_fingerprint,
        )?
        .ok_or_else(|| {
            GuiStateError::InvalidValue("uncertain submission row disappeared while saving".into())
        })?;
        transaction.commit()?;
        Ok(if inserted {
            BeginUncertainSubmission::Inserted(saved)
        } else {
            BeginUncertainSubmission::Existing(saved)
        })
    }

    pub fn uncertain_submission(
        &self,
        provider_id: &ProviderId,
        draft_fingerprint: &str,
    ) -> Result<Option<UncertainSubmissionRecord>, GuiStateError> {
        validate_fingerprint(draft_fingerprint)?;
        let connection = self.ready_connection()?;
        query_uncertain_submission(&connection, provider_id, draft_fingerprint)
    }

    pub fn clear_uncertain_submission(
        &self,
        provider_id: &ProviderId,
        draft_fingerprint: &str,
    ) -> Result<bool, GuiStateError> {
        validate_fingerprint(draft_fingerprint)?;
        let connection = self.ready_connection()?;
        let changed = connection.execute(
            "DELETE FROM uncertain_submissions
             WHERE provider_id = ?1 AND draft_fingerprint = ?2",
            params![provider_id.as_str(), draft_fingerprint],
        )?;
        Ok(changed > 0)
    }

    pub fn uncertain_submissions(&self) -> Result<Vec<UncertainSubmissionRecord>, GuiStateError> {
        let connection = self.ready_connection()?;
        let mut statement = connection.prepare(
            "SELECT provider_id, draft_fingerprint, recorded_at, message
             FROM uncertain_submissions
             ORDER BY recorded_at ASC, provider_id ASC, draft_fingerprint ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (provider_id, draft_fingerprint, recorded_at, message) = row?;
            let record = UncertainSubmissionRecord {
                provider_id: ProviderId::new(provider_id)?,
                draft_fingerprint,
                recorded_at: parse_timestamp(&recorded_at)?,
                message,
            };
            validate_uncertain_submission(&record)?;
            records.push(record);
        }
        Ok(records)
    }
}

/// Return a stable semantic digest without reading or retaining media bytes.
/// JSON object order, Unicode composition, and cosmetic string whitespace do
/// not change the result. Local references use their canonical path identity.
pub fn generation_draft_fingerprint(draft: &GenerationDraft) -> Result<String, GuiStateError> {
    let media = draft
        .media
        .iter()
        .map(|item| {
            let source = match &item.source {
                MediaSource::LocalFile { path } => json!({
                    "kind": "local_file",
                    "path_identity": canonical_path_identity(path),
                }),
                MediaSource::RemoteUrl { url } => json!({
                    "kind": "remote_url",
                    "url": canonical_text(url),
                }),
            };
            json!({
                "role": match item.role {
                    MediaRole::StartFrame => "start_frame",
                    MediaRole::EndFrame => "end_frame",
                    MediaRole::Reference => "reference",
                },
                "source": source,
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "fingerprint_version": 1,
        "provider_id": draft.provider_id.as_str(),
        "model": canonical_text(&draft.model),
        "prompt": canonical_text(&draft.prompt),
        "duration": draft.duration,
        "resolution": draft.resolution.as_deref().map(canonical_text),
        "aspect_ratio": draft.aspect_ratio.as_deref().map(canonical_text),
        "size": draft.size.as_deref().map(canonical_text),
        "generate_audio": draft.generate_audio,
        "seed": draft.seed,
        "media": media,
        "adapter_options": draft.adapter_options.as_ref().map(canonical_json),
    });
    fingerprint_value(&value)
}

fn fingerprint_value(value: &Value) -> Result<String, GuiStateError> {
    let serialized = serde_json::to_vec(value)?;
    let digest = Sha256::digest(serialized);
    Ok(format!("{digest:x}"))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.nfc().collect::<String>(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::String(value) => Value::String(canonical_text(value)),
        other => other.clone(),
    }
}

fn canonical_text(value: &str) -> String {
    value
        .nfc()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn canonical_path_identity(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .nfc()
        .collect()
}

fn query_uncertain_submission(
    connection: &Connection,
    provider_id: &ProviderId,
    draft_fingerprint: &str,
) -> Result<Option<UncertainSubmissionRecord>, GuiStateError> {
    let row = connection
        .query_row(
            "SELECT recorded_at, message FROM uncertain_submissions
             WHERE provider_id = ?1 AND draft_fingerprint = ?2",
            params![provider_id.as_str(), draft_fingerprint],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((recorded_at, message)) = row else {
        return Ok(None);
    };
    let record = UncertainSubmissionRecord {
        provider_id: provider_id.clone(),
        draft_fingerprint: draft_fingerprint.into(),
        recorded_at: parse_timestamp(&recorded_at)?,
        message,
    };
    validate_uncertain_submission(&record)?;
    Ok(Some(record))
}

fn validate_draft(draft: &StoredDraft) -> Result<(), GuiStateError> {
    if !draft.settings.is_object() {
        return Err(GuiStateError::InvalidValue(
            "draft settings must be a JSON object".into(),
        ));
    }
    if contains_credential_field(&draft.settings) {
        return Err(GuiStateError::CredentialInDraft);
    }
    if draft.settings.get(DRAFT_EDITOR_STATE_KEY).is_some() {
        return Err(GuiStateError::InvalidValue(
            "draft settings use a reserved editor-state key".into(),
        ));
    }
    if let Some(editor_state) = &draft.editor_state {
        editor_state.validate()?;
    }
    for media in &draft.media {
        if media.id.trim().is_empty() || media.role.trim().is_empty() {
            return Err(GuiStateError::InvalidValue(
                "draft media id and role cannot be empty".into(),
            ));
        }
    }
    Ok(())
}

fn encoded_draft_settings(draft: &StoredDraft) -> Result<Value, GuiStateError> {
    let mut settings = draft.settings.clone();
    if let Some(editor_state) = &draft.editor_state {
        settings
            .as_object_mut()
            .ok_or_else(|| {
                GuiStateError::InvalidValue("draft settings must be a JSON object".into())
            })?
            .insert(
                DRAFT_EDITOR_STATE_KEY.into(),
                serde_json::to_value(editor_state)?,
            );
    }
    Ok(settings)
}

fn decoded_draft_settings(
    mut settings: Value,
) -> Result<(Value, Option<DraftEditorState>), GuiStateError> {
    let object = settings.as_object_mut().ok_or_else(|| {
        GuiStateError::InvalidValue("draft settings must be a JSON object".into())
    })?;
    let editor_state = object
        .remove(DRAFT_EDITOR_STATE_KEY)
        .map(serde_json::from_value)
        .transpose()?;
    Ok((settings, editor_state))
}

fn validate_uncertain_submission(record: &UncertainSubmissionRecord) -> Result<(), GuiStateError> {
    validate_fingerprint(&record.draft_fingerprint)?;
    if record.message != UNCERTAIN_SUBMISSION_MESSAGE {
        return Err(GuiStateError::InvalidValue(
            "uncertain submission warning must not contain provider response data".into(),
        ));
    }
    Ok(())
}

fn validate_fingerprint(value: &str) -> Result<(), GuiStateError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(GuiStateError::InvalidValue(
            "draft fingerprint must be a lowercase SHA-256 digest".into(),
        ));
    }
    Ok(())
}

fn validate_receipt(receipt: &StoredUploadReceipt) -> Result<(), GuiStateError> {
    let sha = receipt.source_sha256.as_bytes();
    if sha.len() != 64 || !sha.iter().all(u8::is_ascii_hexdigit) {
        return Err(GuiStateError::InvalidValue(
            "upload receipt requires a SHA-256 hex digest".into(),
        ));
    }
    if !receipt.source_path.is_absolute() {
        return Err(GuiStateError::InvalidValue(
            "upload source path must be absolute".into(),
        ));
    }
    if !receipt.remote_url.starts_with("https://") {
        return Err(GuiStateError::InvalidValue(
            "upload receipt URL must use HTTPS".into(),
        ));
    }
    if receipt.expires_at <= receipt.created_at {
        return Err(GuiStateError::InvalidValue(
            "upload receipt expiry must follow creation".into(),
        ));
    }
    Ok(())
}

fn is_credential_field_name(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    if matches!(
        normalized.as_str(),
        "apikey"
            | "authorization"
            | "credential"
            | "credentials"
            | "secret"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "clientsecret"
            | "secretkey"
            | "privatekey"
            | "authtoken"
            | "bearertoken"
            | "falkey"
    ) {
        return true;
    }

    // Provider/environment spellings and camelCase UI fields normalize to
    // these suffixes. Do not treat every occurrence of "token" as a secret:
    // generation controls such as max_tokens and token_count are metadata.
    normalized.starts_with("authorization")
        || normalized.ends_with("apikey")
        || normalized.ends_with("secret")
        || (normalized.ends_with("token")
            && !matches!(
                normalized.as_str(),
                "maxtoken" | "mintoken" | "numtoken" | "tokencount" | "tokenlimit"
            ))
}

fn contains_credential_field(value: &Value) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| is_credential_field_name(key) || contains_credential_field(value)),
        Value::Array(values) => values.iter().any(contains_credential_field),
        _ => false,
    }
}

fn validate_editor_text(label: &str, value: &str, max_bytes: usize) -> Result<(), GuiStateError> {
    if value.len() > max_bytes {
        return Err(GuiStateError::InvalidValue(format!(
            "draft {label} editor text is too large"
        )));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(GuiStateError::InvalidValue(format!(
            "draft {label} editor text contains unsupported control characters"
        )));
    }
    Ok(())
}

fn raw_text_contains_credential_field(text: &str) -> bool {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return contains_credential_field(&value);
    }

    let bytes = text.as_bytes();
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
            continue;
        }

        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b':' | b'='
                if raw_key_before_assignment(text, index)
                    .is_some_and(|key| is_credential_field_name(&key)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn raw_key_before_assignment(text: &str, assignment: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut end = assignment;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    if matches!(bytes[end - 1], b'\'' | b'"') {
        let quote = bytes[end - 1];
        let closing_quote = end - 1;
        let mut opening_quote = closing_quote;
        while opening_quote > 0 {
            opening_quote -= 1;
            if bytes[opening_quote] != quote || quote_is_escaped(bytes, opening_quote) {
                continue;
            }
            if !raw_key_has_assignment_boundary(bytes, opening_quote) {
                return None;
            }
            return if quote == b'"' {
                serde_json::from_str::<String>(&text[opening_quote..end]).ok()
            } else {
                Some(text[opening_quote + 1..closing_quote].to_owned())
            };
        }
        return None;
    }

    let mut start = end;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric()
            || matches!(bytes[start - 1], b'_' | b'-' | b'.'))
    {
        start -= 1;
    }
    (start < end && raw_key_has_assignment_boundary(bytes, start))
        .then(|| text[start..end].to_owned())
}

fn raw_key_has_assignment_boundary(bytes: &[u8], start: usize) -> bool {
    start == 0
        || bytes[start - 1].is_ascii_whitespace()
        || matches!(bytes[start - 1], b'{' | b'[' | b'(' | b',' | b';')
}

fn quote_is_escaped(bytes: &[u8], quote: usize) -> bool {
    let mut backslashes = 0;
    let mut cursor = quote;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, GuiStateError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| GuiStateError::InvalidValue(format!("invalid timestamp {value}")))
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

impl From<crate::domain::DomainError> for GuiStateError {
    fn from(error: crate::domain::DomainError) -> Self {
        Self::InvalidValue(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use chrono::TimeDelta;
    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn store() -> (tempfile::TempDir, GuiStateStore) {
        let directory = tempdir().expect("temp dir");
        let store = GuiStateStore::new(directory.path().join("gui-state.sqlite3"));
        (directory, store)
    }

    fn draft() -> StoredDraft {
        StoredDraft {
            revision: 7,
            provider_id: ProviderId::fal(),
            model_id: "fal-ai/sample/video".into(),
            prompt: "A tiny robot watering flowers".into(),
            settings: json!({"duration": 5, "aspect_ratio": "16:9"}),
            editor_state: None,
            media: vec![
                StoredDraftMedia {
                    id: "start".into(),
                    role: "start_frame".into(),
                    source: StoredMediaSource::LocalFile(PathBuf::from("/tmp/start.png")),
                },
                StoredDraftMedia {
                    id: "reference".into(),
                    role: "reference".into(),
                    source: StoredMediaSource::RemoteUrl("https://example.test/ref.png".into()),
                },
            ],
        }
    }

    #[test]
    fn draft_round_trips_paths_without_file_contents() {
        let (_directory, store) = store();
        let expected = draft();
        store.save_draft(&expected).expect("save draft");
        assert_eq!(store.load_draft().expect("load draft"), Some(expected));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(store.path())
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_editor_text_round_trips_exactly_without_schema_migration() {
        let (directory, store) = store();
        let mut expected = draft();
        expected.editor_state = Some(DraftEditorState {
            seed_text: "-".into(),
            advanced_json_text: "{\n  \"guidance_scale\": 1.\n".into(),
            schema_text: BTreeMap::from([
                ("motion_strength".into(), "-.".into()),
                ("freeform_note".into(), "unfinished [ text".into()),
            ]),
        });

        store.save_draft(&expected).expect("save raw editor state");
        drop(store);
        let reopened = GuiStateStore::new(directory.path().join("gui-state.sqlite3"));
        assert_eq!(
            reopened.load_draft().expect("reload raw editor state"),
            Some(expected)
        );
        let connection = Connection::open(reopened.path()).expect("open state database");
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
            SCHEMA_VERSION,
            "editor text is encoded in settings_json without a SQL migration"
        );
    }

    #[test]
    fn malformed_editor_json_with_credential_key_fails_closed() {
        let (_directory, store) = store();
        let mut unsafe_draft = draft();
        unsafe_draft.editor_state = Some(DraftEditorState {
            advanced_json_text: "{\"nested\": {\"api-key\": \"do-not-save\"".into(),
            ..DraftEditorState::default()
        });

        assert!(matches!(
            store.save_draft(&unsafe_draft),
            Err(GuiStateError::CredentialInDraft)
        ));
        assert_eq!(store.load_draft().expect("load rejected draft"), None);
    }

    #[test]
    fn credential_names_cover_provider_and_auth_conventions_without_token_controls() {
        for name in [
            "OPENROUTER_API_KEY",
            "FAL_KEY",
            "FAL_API_KEY",
            "x-api-key",
            "openRouterApiKey",
            "clientSecret",
            "webhookSecret",
            "accessToken",
            "customToken",
            "authorization",
            "authorizationHeader",
        ] {
            assert!(
                is_credential_field_name(name),
                "expected {name:?} to be credential-like"
            );
        }

        for name in [
            "max_tokens",
            "token_count",
            "output_tokens",
            "maxToken",
            "tokenLimit",
            "motion_strength",
        ] {
            assert!(
                !is_credential_field_name(name),
                "expected {name:?} to remain a normal generation control"
            );
        }
    }

    #[test]
    fn raw_credential_assignments_support_json_shell_and_config_syntaxes() {
        for text in [
            r#"{"OPENROUTER_API_KEY":"value"}"#,
            r#"{"nested":{"FAL_API_KEY":"value"}}"#,
            "{'FAL_KEY': 'value'",
            "'x-api-key' = 'value'",
            "\"openRouterApiKey\" = \"value",
            "clientSecret: value",
            "accessToken = value",
            "authorizationHeader=Bearer value",
            "export OPENROUTER_API_KEY=value",
        ] {
            assert!(
                raw_text_contains_credential_field(text),
                "expected credential assignment in {text:?}"
            );
        }

        for text in [
            r#"{"max_tokens":128,"token_count":4}"#,
            "max_tokens = 128\ntoken_count: 4",
            r#"{"note":"authorization: cinematic"}"#,
            r#"{"note":"OPENROUTER_API_KEY=value"}"#,
            r#"note = "authorizationHeader: cinematic""#,
        ] {
            assert!(
                !raw_text_contains_credential_field(text),
                "unexpected credential assignment in {text:?}"
            );
        }
    }

    #[test]
    fn schema_editor_credential_names_and_oversized_values_fail_closed() {
        let (_directory, store) = store();
        let mut credential = draft();
        credential.editor_state = Some(DraftEditorState {
            schema_text: BTreeMap::from([("access-token".into(), "not persisted".into())]),
            ..DraftEditorState::default()
        });
        assert!(matches!(
            store.save_draft(&credential),
            Err(GuiStateError::CredentialInDraft)
        ));

        let mut oversized = draft();
        oversized.editor_state = Some(DraftEditorState {
            seed_text: "0".repeat(MAX_SEED_TEXT_BYTES + 1),
            ..DraftEditorState::default()
        });
        assert!(matches!(
            store.save_draft(&oversized),
            Err(GuiStateError::InvalidValue(_))
        ));
    }

    #[test]
    fn editor_debug_output_never_contains_raw_text() {
        let editor = DraftEditorState {
            seed_text: "seed-marker".into(),
            advanced_json_text: "advanced-marker".into(),
            schema_text: BTreeMap::from([("field-marker".into(), "value-marker".into())]),
        };
        let debug = format!("{editor:?}");
        for marker in [
            "seed-marker",
            "advanced-marker",
            "field-marker",
            "value-marker",
        ] {
            assert!(!debug.contains(marker));
        }
    }

    #[test]
    fn draft_rejects_nested_credentials() {
        let (_directory, store) = store();
        let mut unsafe_draft = draft();
        unsafe_draft.settings = json!({"advanced": {"api-key": "do-not-save"}});
        assert!(matches!(
            store.save_draft(&unsafe_draft),
            Err(GuiStateError::CredentialInDraft)
        ));
    }

    #[test]
    fn in_progress_draft_allows_an_empty_model() {
        let (_directory, store) = store();
        let mut in_progress = draft();
        in_progress.model_id.clear();
        in_progress.prompt = "Prompt entered before catalogs loaded".into();
        store.save_draft(&in_progress).expect("save composer draft");
        assert_eq!(
            store.load_draft().expect("load composer draft"),
            Some(in_progress)
        );
    }

    #[test]
    fn upload_receipt_is_reused_only_before_expiry() {
        let (_directory, store) = store();
        let now = Utc::now();
        let receipt = StoredUploadReceipt {
            provider_id: ProviderId::fal(),
            source_sha256: "a".repeat(64),
            source_path: PathBuf::from("/tmp/start.png"),
            remote_url: "https://v3.fal.media/files/start.png".into(),
            content_type: "image/png".into(),
            byte_length: 42,
            created_at: now,
            expires_at: now + TimeDelta::hours(24),
        };
        store.save_upload_receipt(&receipt).expect("save receipt");
        assert_eq!(
            store
                .usable_upload_receipt(&ProviderId::fal(), &"a".repeat(64), now)
                .expect("active lookup"),
            Some(receipt.clone())
        );
        assert_eq!(
            store
                .usable_upload_receipt(
                    &ProviderId::fal(),
                    &"a".repeat(64),
                    now + TimeDelta::hours(25),
                )
                .expect("expired lookup"),
            None
        );
    }

    #[test]
    fn generation_media_and_resumable_jobs_round_trip() {
        let (_directory, store) = store();
        let key = ProviderJobKey {
            provider_id: ProviderId::fal(),
            remote_job_id: "request-1".into(),
        };
        let locator = JobLocator::Fal {
            endpoint_id: "model".into(),
            request_id: "request-1".into(),
            status_url: Some("https://queue.fal.run/model/requests/request-1/status".into()),
            response_url: Some("https://queue.fal.run/model/requests/request-1".into()),
        };
        let saved = ResumableJob {
            key: key.clone(),
            locator,
            accepted_at: Utc::now(),
            monitoring_paused: false,
        };
        store.save_resumable_job(&saved).expect("save resumable");
        store
            .set_monitoring_paused(&key, true)
            .expect("pause resumable");
        let jobs = store.resumable_jobs().expect("load jobs");
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].monitoring_paused);

        let media = vec![GenerationMediaAssociation {
            key: key.clone(),
            position: 0,
            draft_media_id: "start".into(),
            role: "start_frame".into(),
            source: StoredMediaSource::LocalFile(PathBuf::from("/tmp/start.png")),
            resolved_url: "https://v3.fal.media/files/start.png".into(),
        }];
        store
            .replace_generation_media(&key, &media)
            .expect("save generation media");
        assert_eq!(store.generation_media(&key).expect("load media"), media);
        assert!(store.remove_resumable_job(&key).expect("remove job"));
        assert!(store.resumable_jobs().expect("load empty jobs").is_empty());
    }

    #[test]
    fn uncertain_submissions_are_a_digest_only_set() {
        let (_directory, store) = store();
        let first =
            UncertainSubmissionRecord::new(ProviderId::openrouter(), "a".repeat(64), Utc::now());
        let second = UncertainSubmissionRecord::new(
            ProviderId::fal(),
            "b".repeat(64),
            Utc::now() + TimeDelta::seconds(1),
        );
        assert!(matches!(
            store
                .begin_uncertain_submission(&first)
                .expect("begin first"),
            BeginUncertainSubmission::Inserted(ref saved) if saved == &first
        ));
        assert!(matches!(
            store
                .begin_uncertain_submission(&second)
                .expect("begin second"),
            BeginUncertainSubmission::Inserted(ref saved) if saved == &second
        ));

        let replacement = UncertainSubmissionRecord::new(
            ProviderId::openrouter(),
            "a".repeat(64),
            first.recorded_at + TimeDelta::minutes(2),
        );
        assert!(matches!(
            store
                .begin_uncertain_submission(&replacement)
                .expect("repeat first"),
            BeginUncertainSubmission::Existing(ref saved) if saved == &first
        ));
        assert_eq!(
            store.uncertain_submissions().expect("load set"),
            vec![first.clone(), second.clone()]
        );
        assert!(
            store
                .clear_uncertain_submission(&first.provider_id, &first.draft_fingerprint)
                .expect("clear first")
        );
        assert_eq!(
            store.uncertain_submissions().expect("load remainder"),
            vec![second]
        );
    }

    #[test]
    fn uncertain_submission_rejects_provider_response_text() {
        let (_directory, store) = store();
        let mut unsafe_record =
            UncertainSubmissionRecord::new(ProviderId::openrouter(), "c".repeat(64), Utc::now());
        unsafe_record.message = "server echoed a credential".into();
        assert!(matches!(
            store.begin_uncertain_submission(&unsafe_record),
            Err(GuiStateError::InvalidValue(_))
        ));
    }

    #[test]
    fn semantic_fingerprint_ignores_cosmetic_edits_and_json_key_order() {
        let mut first = GenerationDraft::new(
            ProviderId::fal(),
            " fal-ai/sample/video ",
            "  A  tiny\nrobot watering flowers  ",
        )
        .expect("draft");
        first.adapter_options = Some(json!({
            "camera": {"motion": " slow   pan ", "strength": 1},
            "negative_prompt": " blur  "
        }));
        first.media.push(
            crate::domain::DraftMedia::remote(
                "https://example.test/reference.png",
                MediaRole::Reference,
            )
            .expect("remote media"),
        );

        let mut same = GenerationDraft::new(
            ProviderId::fal(),
            "fal-ai/sample/video",
            "A tiny robot watering flowers",
        )
        .expect("same draft");
        same.adapter_options = Some(
            serde_json::from_str(
                r#"{"negative_prompt":"blur","camera":{"strength":1,"motion":"slow pan"}}"#,
            )
            .expect("reordered JSON"),
        );
        same.media = first.media.clone();
        assert_eq!(
            generation_draft_fingerprint(&first).expect("first fingerprint"),
            generation_draft_fingerprint(&same).expect("same fingerprint")
        );

        same.prompt.push_str(" at sunset");
        assert_ne!(
            generation_draft_fingerprint(&first).expect("original fingerprint"),
            generation_draft_fingerprint(&same).expect("distinct fingerprint")
        );
    }

    #[test]
    fn fingerprint_uses_canonical_local_path_identity_without_file_bytes() {
        let directory = tempdir().expect("temp dir");
        let image = directory.path().join("frame.png");
        File::create(&image).expect("create reference");
        let dotted = directory.path().join(".").join("frame.png");
        let mut first = GenerationDraft::new(ProviderId::fal(), "model", "prompt").expect("draft");
        first.media.push(crate::domain::DraftMedia::local(
            image,
            MediaRole::StartFrame,
        ));
        let mut same = first.clone();
        same.media[0].source = MediaSource::local(dotted);
        assert_eq!(
            generation_draft_fingerprint(&first).expect("first fingerprint"),
            generation_draft_fingerprint(&same).expect("same file identity")
        );
    }

    #[test]
    fn newer_schema_is_not_modified() {
        let (_directory, store) = store();
        let connection = Connection::open(store.path()).expect("open sqlite");
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("set version");
        drop(connection);
        assert!(matches!(
            store.initialize(),
            Err(GuiStateError::UnsupportedSchemaVersion { .. })
        ));
    }
}
