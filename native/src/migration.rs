//! One-time, consent-gated import of the pre-Flatpak Video Harness state.
//!
//! The importer is deliberately separate from service startup. Callers must
//! assess it and collect consent before [`crate::config::AppPaths::ensure`] or
//! either SQLite store is initialized. Only the two application databases and
//! the known settings/catalog JSON files are imported. Provider credentials
//! remain in Secret Service and are never copied by this module.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::config::{APP_SETTINGS_SCHEMA_VERSION, AppPaths, AppSettings};
use crate::gui_state::{StoredDraftMedia, StoredMediaSource};

pub const FLATPAK_APP_ID: &str = "io.github.EnchiladaBoy.VideoHarness";
pub const LEGACY_APP_DIRECTORY: &str = "openrouter-video-studio";
pub const MIGRATION_MARKER_FILE: &str = "legacy-import-v1.json";

const MARKER_SCHEMA_VERSION: u32 = 1;
const SUPPORTED_HISTORY_SCHEMA_VERSION: i64 = 2;
const SUPPORTED_GUI_STATE_SCHEMA_VERSION: i64 = 1;
const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationDirectories {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl MigrationDirectories {
    pub fn legacy_for_home(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        Self {
            data_dir: home.join(".local/share").join(LEGACY_APP_DIRECTORY),
            cache_dir: home.join(".cache").join(LEGACY_APP_DIRECTORY),
            config_dir: home.join(".config").join(LEGACY_APP_DIRECTORY),
        }
    }

    pub fn from_app_paths(paths: &AppPaths) -> Self {
        Self {
            data_dir: paths.data_dir.clone(),
            cache_dir: paths.cache_dir.clone(),
            config_dir: paths.config_dir.clone(),
        }
    }

    fn all(&self) -> [&Path; 3] {
        [&self.data_dir, &self.cache_dir, &self.config_dir]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyMigrationConsent {
    Import,
    StartClean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyMigrationDecision {
    Imported,
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelinkReason {
    OutsideSandbox,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelinkRequired {
    pub draft_id: String,
    pub media_id: String,
    pub path: PathBuf,
    pub reason: RelinkReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LegacyMigrationReport {
    pub imported_history: bool,
    pub imported_gui_state: bool,
    /// Paths are relative to the legacy/target application config directory.
    pub config_files: Vec<PathBuf>,
    /// Paths are relative to the legacy/target application cache directory.
    pub cache_files: Vec<PathBuf>,
    #[serde(default)]
    pub relink_required: Vec<RelinkRequired>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyMigrationMarker {
    pub schema_version: u32,
    pub decision: LegacyMigrationDecision,
    pub completed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<LegacyMigrationReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyMigrationInventory {
    pub history_database: bool,
    pub gui_state_database: bool,
    pub config_file_count: usize,
    pub cache_file_count: usize,
}

impl LegacyMigrationInventory {
    pub fn is_empty(&self) -> bool {
        !self.history_database
            && !self.gui_state_database
            && self.config_file_count == 0
            && self.cache_file_count == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyMigrationEligibility {
    NotFlatpak,
    AlreadyDecided(LegacyMigrationMarker),
    TargetContainsData(PathBuf),
    NoLegacyData,
    NeedsConsent(LegacyMigrationInventory),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyMigrationSkipReason {
    NotFlatpak,
    TargetContainsData(PathBuf),
    NoLegacyData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyMigrationOutcome {
    Skipped(LegacyMigrationSkipReason),
    AlreadyDecided(LegacyMigrationMarker),
    Declined(LegacyMigrationMarker),
    Imported(LegacyMigrationReport),
}

#[derive(Debug, Error)]
pub enum LegacyMigrationError {
    #[error("HOME is unavailable; cannot locate the pre-Flatpak application data")]
    HomeUnavailable,
    #[error("legacy and Flatpak target storage overlap at {0}")]
    SourceTargetOverlap(PathBuf),
    #[error("unsafe legacy migration source at {path}: {reason}")]
    UnsafeSource { path: PathBuf, reason: String },
    #[error("could not {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read SQLite database {path}: {source}")]
    Database {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("SQLite integrity check failed for {path}: {detail}")]
    DatabaseIntegrity { path: PathBuf, detail: String },
    #[error("{database} schema version {found} is newer than this release supports ({supported})")]
    UnsupportedDatabaseSchema {
        database: &'static str,
        found: i64,
        supported: i64,
    },
    #[error("legacy JSON file {path} is invalid: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("legacy JSON file {path} is too large to import safely")]
    JsonTooLarge { path: PathBuf },
    #[error("legacy settings file {path} may contain credentials and was not imported")]
    CredentialInSettings { path: PathBuf },
    #[error("legacy settings file {path} has an invalid document shape")]
    InvalidSettingsDocument { path: PathBuf },
    #[error(
        "app settings schema version {found} is newer than this release supports ({supported})"
    )]
    UnsupportedAppSettingsSchema { found: u64, supported: u32 },
    #[error("migration marker {path} is invalid: {source}")]
    InvalidMarker {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "migration marker schema version {found} is newer than this release supports ({supported})"
    )]
    UnsupportedMarkerSchema { found: u32, supported: u32 },
    #[error("the target changed while legacy data was being imported: {0}")]
    TargetChanged(PathBuf),
    #[error("could not commit the staged migration: {commit}; rollback: {rollback}")]
    Commit { commit: String, rollback: String },
}

#[derive(Debug, Clone)]
pub struct LegacyMigration {
    flatpak: bool,
    source: MigrationDirectories,
    target: MigrationDirectories,
    videos_dir: PathBuf,
    additional_accessible_roots: Vec<PathBuf>,
}

impl LegacyMigration {
    /// Discover the exact pre-Flatpak paths. Custom legacy XDG roots are not
    /// guessed because the Flatpak manifest grants read-only access only to
    /// these default directories.
    pub fn discover(target: AppPaths) -> Result<Self, LegacyMigrationError> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(LegacyMigrationError::HomeUnavailable)?;
        let flatpak = env::var("FLATPAK_ID").ok().as_deref() == Some(FLATPAK_APP_ID);
        let mut accessible_roots = Vec::new();
        if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
            accessible_roots.push(PathBuf::from(runtime).join("doc"));
        }
        Ok(Self {
            flatpak,
            source: MigrationDirectories::legacy_for_home(home),
            target: MigrationDirectories::from_app_paths(&target),
            videos_dir: target.videos_dir,
            additional_accessible_roots: accessible_roots,
        })
    }

    /// Explicit constructor for startup wiring and deterministic tests.
    pub fn for_paths(
        flatpak: bool,
        source: MigrationDirectories,
        target: MigrationDirectories,
        videos_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            flatpak,
            source,
            target,
            videos_dir: videos_dir.into(),
            additional_accessible_roots: Vec::new(),
        }
    }

    pub fn with_accessible_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.additional_accessible_roots.push(root.into());
        self
    }

    pub fn source(&self) -> &MigrationDirectories {
        &self.source
    }

    pub fn target(&self) -> &MigrationDirectories {
        &self.target
    }

    pub fn marker_path(&self) -> PathBuf {
        self.target.config_dir.join(MIGRATION_MARKER_FILE)
    }

    pub fn marker(&self) -> Result<Option<LegacyMigrationMarker>, LegacyMigrationError> {
        let path = self.marker_path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LegacyMigrationError::Io {
                    action: "inspect migration marker",
                    path,
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(LegacyMigrationError::TargetChanged(path));
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) => {
                return Err(LegacyMigrationError::Io {
                    action: "read migration marker",
                    path,
                    source,
                });
            }
        };
        let marker: LegacyMigrationMarker = serde_json::from_slice(&bytes).map_err(|source| {
            LegacyMigrationError::InvalidMarker {
                path: path.clone(),
                source,
            }
        })?;
        if marker.schema_version > MARKER_SCHEMA_VERSION {
            return Err(LegacyMigrationError::UnsupportedMarkerSchema {
                found: marker.schema_version,
                supported: MARKER_SCHEMA_VERSION,
            });
        }
        Ok(Some(marker))
    }

    /// Relinks are persisted in the success marker so startup can continue to
    /// block Review until each local reference is reselected through a portal.
    pub fn pending_relinks(&self) -> Result<Vec<RelinkRequired>, LegacyMigrationError> {
        Ok(self
            .marker()?
            .and_then(|marker| marker.report)
            .map(|report| report.relink_required)
            .unwrap_or_default())
    }

    /// Remove a relink barrier after the GUI has successfully saved a portal
    /// replacement for this draft media item. The database update should be
    /// committed first; this marker update is intentionally the final step.
    pub fn acknowledge_relink(
        &self,
        draft_id: &str,
        media_id: &str,
    ) -> Result<bool, LegacyMigrationError> {
        let Some(mut marker) = self.marker()? else {
            return Ok(false);
        };
        if marker.decision != LegacyMigrationDecision::Imported {
            return Ok(false);
        }
        let Some(report) = marker.report.as_mut() else {
            return Ok(false);
        };
        let before = report.relink_required.len();
        report
            .relink_required
            .retain(|item| item.draft_id != draft_id || item.media_id != media_id);
        if report.relink_required.len() == before {
            return Ok(false);
        }
        write_marker_atomic(&self.marker_path(), &marker)?;
        Ok(true)
    }

    pub fn assess(&self) -> Result<LegacyMigrationEligibility, LegacyMigrationError> {
        if !self.flatpak {
            return Ok(LegacyMigrationEligibility::NotFlatpak);
        }
        self.validate_separate_storage()?;
        if let Some(marker) = self.marker()? {
            return Ok(LegacyMigrationEligibility::AlreadyDecided(marker));
        }
        if let Some(path) = first_target_entry(&self.target)? {
            return Ok(LegacyMigrationEligibility::TargetContainsData(path));
        }
        let inventory = self.inventory()?;
        if inventory.public.is_empty() {
            Ok(LegacyMigrationEligibility::NoLegacyData)
        } else {
            Ok(LegacyMigrationEligibility::NeedsConsent(inventory.public))
        }
    }

    pub fn run(
        &self,
        consent: LegacyMigrationConsent,
    ) -> Result<LegacyMigrationOutcome, LegacyMigrationError> {
        // Starting clean remains available even when the legacy source is
        // malformed or too new to inventory/import. It only inspects the
        // Flatpak target, then records the user's decision atomically.
        if consent == LegacyMigrationConsent::StartClean {
            if !self.flatpak {
                return Ok(LegacyMigrationOutcome::Skipped(
                    LegacyMigrationSkipReason::NotFlatpak,
                ));
            }
            self.validate_separate_storage()?;
            if let Some(marker) = self.marker()? {
                return Ok(LegacyMigrationOutcome::AlreadyDecided(marker));
            }
            if let Some(path) = first_target_entry(&self.target)? {
                return Ok(LegacyMigrationOutcome::Skipped(
                    LegacyMigrationSkipReason::TargetContainsData(path),
                ));
            }
            let marker = LegacyMigrationMarker {
                schema_version: MARKER_SCHEMA_VERSION,
                decision: LegacyMigrationDecision::Declined,
                completed_at: Utc::now(),
                report: None,
            };
            write_marker_atomic(&self.marker_path(), &marker)?;
            return Ok(LegacyMigrationOutcome::Declined(marker));
        }

        match self.assess()? {
            LegacyMigrationEligibility::NotFlatpak => {
                return Ok(LegacyMigrationOutcome::Skipped(
                    LegacyMigrationSkipReason::NotFlatpak,
                ));
            }
            LegacyMigrationEligibility::AlreadyDecided(marker) => {
                return Ok(LegacyMigrationOutcome::AlreadyDecided(marker));
            }
            LegacyMigrationEligibility::TargetContainsData(path) => {
                return Ok(LegacyMigrationOutcome::Skipped(
                    LegacyMigrationSkipReason::TargetContainsData(path),
                ));
            }
            LegacyMigrationEligibility::NoLegacyData => {
                return Ok(LegacyMigrationOutcome::Skipped(
                    LegacyMigrationSkipReason::NoLegacyData,
                ));
            }
            LegacyMigrationEligibility::NeedsConsent(_) => {}
        }

        // Recheck after consent. A second process must not be allowed to race
        // this one and have its freshly initialized database overwritten.
        if let Some(marker) = self.marker()? {
            return Ok(LegacyMigrationOutcome::AlreadyDecided(marker));
        }
        if let Some(path) = first_target_entry(&self.target)? {
            return Ok(LegacyMigrationOutcome::Skipped(
                LegacyMigrationSkipReason::TargetContainsData(path),
            ));
        }

        self.import()
    }

    fn validate_separate_storage(&self) -> Result<(), LegacyMigrationError> {
        for source in self.source.all() {
            for target in self.target.all() {
                if paths_overlap(source, target) {
                    return Err(LegacyMigrationError::SourceTargetOverlap(
                        source.to_path_buf(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn inventory(&self) -> Result<PrivateInventory, LegacyMigrationError> {
        let history = known_regular_file(&self.source.data_dir.join("history.sqlite3"))?;
        let gui_state = known_regular_file(&self.source.data_dir.join("gui-state.sqlite3"))?;
        let config_files = collect_json_assets(&self.source.config_dir, AssetClass::Config)?;
        let cache_files = collect_json_assets(&self.source.cache_dir, AssetClass::Cache)?;
        Ok(PrivateInventory {
            public: LegacyMigrationInventory {
                history_database: history,
                gui_state_database: gui_state,
                config_file_count: config_files.len(),
                cache_file_count: cache_files.len(),
            },
            config_files,
            cache_files,
        })
    }

    fn import(&self) -> Result<LegacyMigrationOutcome, LegacyMigrationError> {
        let inventory = self.inventory()?;
        let mut staged = StagedDirectories::create(&self.target)?;
        let import_result = (|| -> Result<LegacyMigrationReport, LegacyMigrationError> {
            let mut report = LegacyMigrationReport::default();

            if inventory.public.history_database {
                backup_sqlite_atomic(
                    &self.source.data_dir.join("history.sqlite3"),
                    &staged.data.join("history.sqlite3"),
                    "history",
                    SUPPORTED_HISTORY_SCHEMA_VERSION,
                )?;
                report.imported_history = true;
            }
            if inventory.public.gui_state_database {
                let destination = staged.data.join("gui-state.sqlite3");
                backup_sqlite_atomic(
                    &self.source.data_dir.join("gui-state.sqlite3"),
                    &destination,
                    "GUI state",
                    SUPPORTED_GUI_STATE_SCHEMA_VERSION,
                )?;
                report.imported_gui_state = true;
                report.relink_required = self.detect_relinks(&destination)?;
            }

            for asset in &inventory.config_files {
                validate_and_copy_json(asset, &staged.config, true)?;
                report.config_files.push(asset.relative.clone());
            }
            for asset in &inventory.cache_files {
                validate_and_copy_json(asset, &staged.cache, false)?;
                report.cache_files.push(asset.relative.clone());
            }
            report.config_files.sort();
            report.cache_files.sort();
            report.relink_required.sort_by(|left, right| {
                (&left.draft_id, &left.media_id, &left.path).cmp(&(
                    &right.draft_id,
                    &right.media_id,
                    &right.path,
                ))
            });

            let marker = LegacyMigrationMarker {
                schema_version: MARKER_SCHEMA_VERSION,
                decision: LegacyMigrationDecision::Imported,
                completed_at: Utc::now(),
                report: Some(report.clone()),
            };
            write_marker_atomic(&staged.config.join(MIGRATION_MARKER_FILE), &marker)?;
            Ok(report)
        })();

        let report = match import_result {
            Ok(report) => report,
            Err(error) => {
                staged.cleanup();
                return Err(error);
            }
        };

        // A final target check closes the window between validation/copying
        // and commit. Staged data is private and can simply be discarded.
        if let Some(path) = first_target_entry(&self.target)? {
            staged.cleanup();
            return Err(LegacyMigrationError::TargetChanged(path));
        }
        staged.commit(&self.target)?;
        Ok(LegacyMigrationOutcome::Imported(report))
    }

    fn detect_relinks(
        &self,
        gui_state_database: &Path,
    ) -> Result<Vec<RelinkRequired>, LegacyMigrationError> {
        let connection = Connection::open_with_flags(
            gui_state_database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| LegacyMigrationError::Database {
            path: gui_state_database.to_path_buf(),
            source,
        })?;
        let has_drafts: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'drafts')",
                [],
                |row| row.get(0),
            )
            .map_err(|source| LegacyMigrationError::Database {
                path: gui_state_database.to_path_buf(),
                source,
            })?;
        if !has_drafts {
            return Ok(Vec::new());
        }

        let mut statement = connection
            .prepare("SELECT id, media_json FROM drafts ORDER BY id")
            .map_err(|source| LegacyMigrationError::Database {
                path: gui_state_database.to_path_buf(),
                source,
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| LegacyMigrationError::Database {
                path: gui_state_database.to_path_buf(),
                source,
            })?;

        let accessible_roots = self.accessible_roots();
        let mut relinks = Vec::new();
        for row in rows {
            let (draft_id, media_json) = row.map_err(|source| LegacyMigrationError::Database {
                path: gui_state_database.to_path_buf(),
                source,
            })?;
            let media: Vec<StoredDraftMedia> =
                serde_json::from_str(&media_json).map_err(|source| {
                    LegacyMigrationError::InvalidJson {
                        path: gui_state_database.to_path_buf(),
                        source,
                    }
                })?;
            for item in media {
                let StoredMediaSource::LocalFile(path) = item.source else {
                    continue;
                };
                let reason = if !path.exists() {
                    Some(RelinkReason::Missing)
                } else if !path_is_within_any(&path, &accessible_roots) {
                    Some(RelinkReason::OutsideSandbox)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    relinks.push(RelinkRequired {
                        draft_id: draft_id.clone(),
                        media_id: item.id,
                        path,
                        reason,
                    });
                }
            }
        }
        Ok(relinks)
    }

    fn accessible_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![
            self.videos_dir.clone(),
            self.target.data_dir.clone(),
            self.target.cache_dir.clone(),
            self.target.config_dir.clone(),
            self.source.data_dir.clone(),
            self.source.cache_dir.clone(),
            self.source.config_dir.clone(),
        ];
        roots.extend(self.additional_accessible_roots.iter().cloned());
        roots
    }
}

#[derive(Debug)]
struct PrivateInventory {
    public: LegacyMigrationInventory,
    config_files: Vec<JsonAsset>,
    cache_files: Vec<JsonAsset>,
}

#[derive(Debug, Clone, Copy)]
enum AssetClass {
    Config,
    Cache,
}

#[derive(Debug)]
struct JsonAsset {
    source: PathBuf,
    relative: PathBuf,
    app_settings: bool,
}

fn collect_json_assets(
    root: &Path,
    class: AssetClass,
) -> Result<Vec<JsonAsset>, LegacyMigrationError> {
    let mut assets = Vec::new();
    let root_name = match class {
        AssetClass::Config => "model-settings.json",
        AssetClass::Cache => "video-models.json",
    };
    let root_file = root.join(root_name);
    if known_regular_file(&root_file)? {
        assets.push(JsonAsset {
            source: root_file,
            relative: PathBuf::from(root_name),
            app_settings: false,
        });
    }
    if matches!(class, AssetClass::Config) {
        let app_settings = root.join("app-settings.json");
        if known_regular_file(&app_settings)? {
            assets.push(JsonAsset {
                source: app_settings,
                relative: PathBuf::from("app-settings.json"),
                app_settings: true,
            });
        }
    }

    let providers = root.join("providers");
    let metadata = match fs::symlink_metadata(&providers) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(assets),
        Err(source) => {
            return Err(LegacyMigrationError::Io {
                action: "inspect legacy provider settings",
                path: providers,
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(LegacyMigrationError::UnsafeSource {
            path: providers,
            reason: "provider directory is a symbolic link".into(),
        });
    }
    if !metadata.is_dir() {
        return Err(LegacyMigrationError::UnsafeSource {
            path: providers,
            reason: "provider path is not a directory".into(),
        });
    }
    let entries = fs::read_dir(&providers).map_err(|source| LegacyMigrationError::Io {
        action: "list legacy provider settings",
        path: providers.clone(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| LegacyMigrationError::Io {
            action: "list legacy provider settings",
            path: providers.clone(),
            source,
        })?;
        let name = entry.file_name();
        let Some(slug) = name.to_str() else {
            continue;
        };
        if !valid_provider_slug(slug) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|source| LegacyMigrationError::Io {
                action: "inspect legacy provider settings",
                path: entry.path(),
                source,
            })?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let source = entry.path().join(root_name);
        if known_regular_file(&source)? {
            assets.push(JsonAsset {
                source,
                relative: PathBuf::from("providers").join(slug).join(root_name),
                app_settings: false,
            });
        }
    }
    assets.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(assets)
}

fn known_regular_file(path: &Path) -> Result<bool, LegacyMigrationError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(LegacyMigrationError::Io {
                action: "inspect legacy file",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(LegacyMigrationError::UnsafeSource {
            path: path.to_path_buf(),
            reason: "file is a symbolic link".into(),
        });
    }
    if !metadata.is_file() {
        return Err(LegacyMigrationError::UnsafeSource {
            path: path.to_path_buf(),
            reason: "path is not a regular file".into(),
        });
    }
    Ok(true)
}

fn validate_and_copy_json(
    asset: &JsonAsset,
    destination_root: &Path,
    reject_credentials: bool,
) -> Result<(), LegacyMigrationError> {
    let metadata = fs::metadata(&asset.source).map_err(|source| LegacyMigrationError::Io {
        action: "inspect legacy JSON",
        path: asset.source.clone(),
        source,
    })?;
    if metadata.len() > MAX_JSON_BYTES {
        return Err(LegacyMigrationError::JsonTooLarge {
            path: asset.source.clone(),
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    File::open(&asset.source)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|source| LegacyMigrationError::Io {
            action: "read legacy JSON",
            path: asset.source.clone(),
            source,
        })?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|source| LegacyMigrationError::InvalidJson {
            path: asset.source.clone(),
            source,
        })?;
    if reject_credentials && contains_credential_key(&value) {
        return Err(LegacyMigrationError::CredentialInSettings {
            path: asset.source.clone(),
        });
    }
    if reject_credentials && !value.is_object() {
        return Err(LegacyMigrationError::InvalidSettingsDocument {
            path: asset.source.clone(),
        });
    }
    if asset.app_settings {
        let settings: AppSettings =
            serde_json::from_value(value).map_err(|source| LegacyMigrationError::InvalidJson {
                path: asset.source.clone(),
                source,
            })?;
        if settings.schema_version != APP_SETTINGS_SCHEMA_VERSION {
            return Err(LegacyMigrationError::UnsupportedAppSettingsSchema {
                found: u64::from(settings.schema_version),
                supported: APP_SETTINGS_SCHEMA_VERSION,
            });
        }
    }
    write_bytes_atomic(&destination_root.join(&asset.relative), &bytes)
}

fn contains_credential_key(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            let normalized = key
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>();
            matches!(
                normalized.as_str(),
                "apikey"
                    | "accesstoken"
                    | "refreshtoken"
                    | "authorization"
                    | "bearertoken"
                    | "secret"
                    | "password"
                    | "privatekey"
            ) || contains_credential_key(value)
        }),
        Value::Array(values) => values.iter().any(contains_credential_key),
        _ => false,
    }
}

fn backup_sqlite_atomic(
    source_path: &Path,
    destination_path: &Path,
    database: &'static str,
    supported_schema: i64,
) -> Result<(), LegacyMigrationError> {
    known_regular_file(source_path)?;
    if let Some(parent) = destination_path.parent() {
        create_private_dir_all(parent)?;
    }
    let temporary = unique_sibling(destination_path, "sqlite-stage")?;
    let result = (|| -> Result<(), LegacyMigrationError> {
        let source = Connection::open_with_flags(
            source_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| LegacyMigrationError::Database {
            path: source_path.to_path_buf(),
            source,
        })?;
        source
            .busy_timeout(Duration::from_secs(5))
            .map_err(|source| LegacyMigrationError::Database {
                path: source_path.to_path_buf(),
                source,
            })?;
        let found: i64 = source
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|source| LegacyMigrationError::Database {
                path: source_path.to_path_buf(),
                source,
            })?;
        if found > supported_schema {
            return Err(LegacyMigrationError::UnsupportedDatabaseSchema {
                database,
                found,
                supported: supported_schema,
            });
        }
        ensure_database_integrity(&source, source_path)?;

        let mut destination =
            Connection::open(&temporary).map_err(|source| LegacyMigrationError::Database {
                path: temporary.clone(),
                source,
            })?;
        {
            let backup = Backup::new(&source, &mut destination).map_err(|source| {
                LegacyMigrationError::Database {
                    path: source_path.to_path_buf(),
                    source,
                }
            })?;
            backup
                .run_to_completion(128, Duration::from_millis(10), None)
                .map_err(|source| LegacyMigrationError::Database {
                    path: source_path.to_path_buf(),
                    source,
                })?;
        }
        ensure_database_integrity(&destination, &temporary)?;
        drop(destination);
        drop(source);
        #[cfg(unix)]
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).map_err(|source| {
            LegacyMigrationError::Io {
                action: "set migrated database permissions",
                path: temporary.clone(),
                source,
            }
        })?;
        File::open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(|source| LegacyMigrationError::Io {
                action: "sync migrated database",
                path: temporary.clone(),
                source,
            })?;
        fs::rename(&temporary, destination_path).map_err(|source| LegacyMigrationError::Io {
            action: "publish migrated database in staging",
            path: destination_path.to_path_buf(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_database_integrity(
    connection: &Connection,
    path: &Path,
) -> Result<(), LegacyMigrationError> {
    let result: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|source| LegacyMigrationError::Database {
            path: path.to_path_buf(),
            source,
        })?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(LegacyMigrationError::DatabaseIntegrity {
            path: path.to_path_buf(),
            detail: result,
        })
    }
}

fn first_target_entry(
    target: &MigrationDirectories,
) -> Result<Option<PathBuf>, LegacyMigrationError> {
    for directory in target.all() {
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(LegacyMigrationError::Io {
                    action: "inspect migration target",
                    path: directory.to_path_buf(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(Some(directory.to_path_buf()));
        }
        let mut entries = fs::read_dir(directory).map_err(|source| LegacyMigrationError::Io {
            action: "list migration target",
            path: directory.to_path_buf(),
            source,
        })?;
        if let Some(entry) = entries.next() {
            let entry = entry.map_err(|source| LegacyMigrationError::Io {
                action: "list migration target",
                path: directory.to_path_buf(),
                source,
            })?;
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn write_marker_atomic(
    path: &Path,
    marker: &LegacyMigrationMarker,
) -> Result<(), LegacyMigrationError> {
    let mut bytes = serde_json::to_vec_pretty(marker).map_err(|source| {
        LegacyMigrationError::InvalidMarker {
            path: path.to_path_buf(),
            source,
        }
    })?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), LegacyMigrationError> {
    let Some(parent) = path.parent() else {
        return Err(LegacyMigrationError::UnsafeSource {
            path: path.to_path_buf(),
            reason: "destination has no parent directory".into(),
        });
    };
    create_private_dir_all(parent)?;
    let temporary = unique_sibling(path, "copy-stage")?;
    let result = (|| -> Result<(), LegacyMigrationError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| LegacyMigrationError::Io {
                action: "create staged file",
                path: temporary.clone(),
                source,
            })?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| LegacyMigrationError::Io {
                action: "set staged file permissions",
                path: temporary.clone(),
                source,
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| LegacyMigrationError::Io {
                action: "write staged file",
                path: temporary.clone(),
                source,
            })?;
        drop(file);
        fs::rename(&temporary, path).map_err(|source| LegacyMigrationError::Io {
            action: "publish staged file",
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_private_dir_all(path: &Path) -> Result<(), LegacyMigrationError> {
    fs::create_dir_all(path).map_err(|source| LegacyMigrationError::Io {
        action: "create migration staging directory",
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        LegacyMigrationError::Io {
            action: "set migration directory permissions",
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

fn unique_sibling(path: &Path, label: &str) -> Result<PathBuf, LegacyMigrationError> {
    let parent = path
        .parent()
        .ok_or_else(|| LegacyMigrationError::UnsafeSource {
            path: path.to_path_buf(),
            reason: "path has no parent directory".into(),
        })?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("video-harness");
    for counter in 0..=u16::MAX {
        let candidate = parent.join(format!(".{name}.{label}-{}-{counter}", process::id()));
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(source) => {
                return Err(LegacyMigrationError::Io {
                    action: "inspect migration temporary path",
                    path: candidate,
                    source,
                });
            }
        }
    }
    Err(LegacyMigrationError::UnsafeSource {
        path: path.to_path_buf(),
        reason: "could not allocate a unique migration staging path".into(),
    })
}

#[derive(Debug)]
struct StagedDirectories {
    data: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    active: bool,
}

impl StagedDirectories {
    fn create(target: &MigrationDirectories) -> Result<Self, LegacyMigrationError> {
        let mut created = Vec::new();
        let result = (|| -> Result<Self, LegacyMigrationError> {
            let data = create_stage_sibling(&target.data_dir)?;
            created.push(data.clone());
            let cache = create_stage_sibling(&target.cache_dir)?;
            created.push(cache.clone());
            let config = create_stage_sibling(&target.config_dir)?;
            created.push(config.clone());
            Ok(Self {
                data,
                cache,
                config,
                active: true,
            })
        })();
        if result.is_err() {
            for path in created {
                let _ = fs::remove_dir_all(path);
            }
        }
        result
    }

    fn cleanup(&mut self) {
        if !self.active {
            return;
        }
        for path in [&self.data, &self.cache, &self.config] {
            let _ = fs::remove_dir_all(path);
        }
        self.active = false;
    }

    fn commit(&mut self, target: &MigrationDirectories) -> Result<(), LegacyMigrationError> {
        let pairs = [
            (&self.data, &target.data_dir),
            (&self.cache, &target.cache_dir),
            // The success marker becomes visible last.
            (&self.config, &target.config_dir),
        ];
        let mut empty_backups = Vec::new();
        let reservation = (|| -> Result<(), LegacyMigrationError> {
            for (_, target_path) in &pairs {
                match fs::symlink_metadata(target_path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                        if fs::read_dir(target_path)
                            .map_err(|source| LegacyMigrationError::Io {
                                action: "recheck migration target",
                                path: (*target_path).clone(),
                                source,
                            })?
                            .next()
                            .is_some()
                        {
                            return Err(LegacyMigrationError::TargetChanged(
                                (*target_path).clone(),
                            ));
                        }
                        let backup = unique_sibling(target_path, "empty-before-import")?;
                        fs::rename(target_path, &backup).map_err(|source| {
                            LegacyMigrationError::Io {
                                action: "reserve empty migration target",
                                path: (*target_path).clone(),
                                source,
                            }
                        })?;
                        empty_backups.push(((*target_path).clone(), backup));
                    }
                    Ok(_) => {
                        return Err(LegacyMigrationError::TargetChanged((*target_path).clone()));
                    }
                    Err(source) => {
                        return Err(LegacyMigrationError::Io {
                            action: "recheck migration target",
                            path: (*target_path).clone(),
                            source,
                        });
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = reservation {
            let mut rollback_errors = Vec::new();
            restore_empty_targets(&empty_backups, &mut rollback_errors);
            if rollback_errors.is_empty() {
                return Err(error);
            }
            return Err(LegacyMigrationError::Commit {
                commit: error.to_string(),
                rollback: rollback_errors.join("; "),
            });
        }

        let mut committed: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (stage, target_path) in pairs {
            if let Err(error) = fs::rename(stage, target_path) {
                let mut rollback_errors = Vec::new();
                for (committed_target, committed_stage) in committed.iter().rev() {
                    if let Err(rollback) = fs::rename(committed_target, committed_stage) {
                        rollback_errors.push(format!(
                            "{} -> {}: {rollback}",
                            committed_target.display(),
                            committed_stage.display()
                        ));
                    }
                }
                restore_empty_targets(&empty_backups, &mut rollback_errors);
                return Err(LegacyMigrationError::Commit {
                    commit: format!("{} -> {}: {error}", stage.display(), target_path.display()),
                    rollback: if rollback_errors.is_empty() {
                        "completed".into()
                    } else {
                        rollback_errors.join("; ")
                    },
                });
            }
            committed.push((target_path.clone(), stage.clone()));
        }
        for (_, backup) in empty_backups {
            // The imported targets are already committed. Failure to remove an
            // empty, hidden backup is harmless and must not turn success into
            // a retry that could overwrite the imported state.
            let _ = fs::remove_dir(backup);
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for StagedDirectories {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn create_stage_sibling(target: &Path) -> Result<PathBuf, LegacyMigrationError> {
    let parent = target
        .parent()
        .ok_or_else(|| LegacyMigrationError::UnsafeSource {
            path: target.to_path_buf(),
            reason: "target directory has no parent".into(),
        })?;
    create_private_dir_all(parent)?;
    let stage = unique_sibling(target, "legacy-import-stage")?;
    fs::create_dir(&stage).map_err(|source| LegacyMigrationError::Io {
        action: "create migration staging directory",
        path: stage.clone(),
        source,
    })?;
    #[cfg(unix)]
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700)).map_err(|source| {
        LegacyMigrationError::Io {
            action: "set migration staging permissions",
            path: stage.clone(),
            source,
        }
    })?;
    Ok(stage)
}

fn restore_empty_targets(backups: &[(PathBuf, PathBuf)], errors: &mut Vec<String>) {
    for (original, backup) in backups.iter().rev() {
        if let Err(error) = fs::rename(backup, original) {
            errors.push(format!(
                "{} -> {}: {error}",
                backup.display(),
                original.display()
            ));
        }
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = normalized_absolute(left);
    let right = normalized_absolute(right);
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

fn path_is_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    let candidate = fs::canonicalize(path).unwrap_or_else(|_| normalized_absolute(path));
    roots.iter().any(|root| {
        let root = fs::canonicalize(root).unwrap_or_else(|_| normalized_absolute(root));
        candidate.starts_with(root)
    })
}

fn normalized_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn valid_provider_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use rusqlite::{Connection, params};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        root: TempDir,
        migration: LegacyMigration,
    }

    impl Fixture {
        fn new(flatpak: bool) -> Self {
            let root = tempfile::tempdir().expect("temporary directory");
            let source = MigrationDirectories::legacy_for_home(root.path().join("home"));
            let sandbox = root.path().join("sandbox");
            let target = MigrationDirectories {
                data_dir: sandbox.join("data/openrouter-video-studio"),
                cache_dir: sandbox.join("cache/openrouter-video-studio"),
                config_dir: sandbox.join("config/openrouter-video-studio"),
            };
            let videos = root.path().join("home/Videos");
            let migration = LegacyMigration::for_paths(flatpak, source, target, videos);
            Self { root, migration }
        }

        fn write_history(&self, schema: i64) {
            fs::create_dir_all(&self.migration.source.data_dir).expect("create legacy data");
            let connection =
                Connection::open(self.migration.source.data_dir.join("history.sqlite3"))
                    .expect("open legacy history");
            connection
                .execute_batch(
                    "CREATE TABLE jobs (job_id TEXT PRIMARY KEY, status TEXT NOT NULL);
                     INSERT INTO jobs VALUES ('job-1', 'processing');",
                )
                .expect("seed history");
            connection
                .pragma_update(None, "user_version", schema)
                .expect("set history schema");
        }

        fn write_gui_state(&self, outside: Option<&Path>) {
            fs::create_dir_all(&self.migration.source.data_dir).expect("create legacy data");
            let connection =
                Connection::open(self.migration.source.data_dir.join("gui-state.sqlite3"))
                    .expect("open GUI state");
            connection
                .execute_batch(
                    "CREATE TABLE drafts (
                         id TEXT PRIMARY KEY NOT NULL,
                         media_json TEXT NOT NULL
                     );
                     CREATE TABLE uncertain_submissions (
                         provider_id TEXT NOT NULL,
                         draft_fingerprint TEXT NOT NULL
                     );
                     CREATE TABLE resumable_jobs (
                         provider_id TEXT NOT NULL,
                         remote_job_id TEXT NOT NULL
                     );",
                )
                .expect("seed GUI schema");
            let media = outside.map_or_else(Vec::new, |path| {
                vec![StoredDraftMedia {
                    id: "legacy-frame".into(),
                    role: "input_image".into(),
                    source: StoredMediaSource::LocalFile(path.to_path_buf()),
                }]
            });
            connection
                .execute(
                    "INSERT INTO drafts VALUES ('current', ?1)",
                    [serde_json::to_string(&media).expect("serialize media")],
                )
                .expect("seed draft");
            connection
                .execute(
                    "INSERT INTO uncertain_submissions VALUES ('openrouter', 'digest')",
                    [],
                )
                .expect("seed safety hold");
            connection
                .execute(
                    "INSERT INTO resumable_jobs VALUES ('openrouter', 'remote-1')",
                    [],
                )
                .expect("seed resumable job");
            connection
                .pragma_update(None, "user_version", SUPPORTED_GUI_STATE_SCHEMA_VERSION)
                .expect("set GUI schema");
        }

        fn snapshot_source(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            fn collect(root: &Path, at: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
                let Ok(entries) = fs::read_dir(at) else {
                    return;
                };
                for entry in entries {
                    let entry = entry.expect("source entry");
                    let path = entry.path();
                    if path.is_dir() {
                        collect(root, &path, output);
                    } else {
                        output.insert(
                            path.strip_prefix(root)
                                .expect("relative source")
                                .to_path_buf(),
                            fs::read(path).expect("read source file"),
                        );
                    }
                }
            }
            let home = self.root.path().join("home");
            let mut output = BTreeMap::new();
            collect(&home, &home, &mut output);
            output
        }
    }

    #[test]
    fn migration_is_flatpak_only_and_requires_an_empty_target() {
        let fixture = Fixture::new(false);
        fixture.write_history(SUPPORTED_HISTORY_SCHEMA_VERSION);
        assert_eq!(
            fixture.migration.assess().expect("assess host run"),
            LegacyMigrationEligibility::NotFlatpak
        );

        let fixture = Fixture::new(true);
        fixture.write_history(SUPPORTED_HISTORY_SCHEMA_VERSION);
        fs::create_dir_all(&fixture.migration.target.data_dir).expect("target data directory");
        fs::write(fixture.migration.target.data_dir.join("existing"), b"mine")
            .expect("existing target data");
        assert!(matches!(
            fixture.migration.assess().expect("assess occupied target"),
            LegacyMigrationEligibility::TargetContainsData(path)
                if path.ends_with("existing")
        ));
    }

    #[test]
    fn decline_is_persistent_and_never_changes_the_source() {
        let fixture = Fixture::new(true);
        fixture.write_history(SUPPORTED_HISTORY_SCHEMA_VERSION);
        fs::create_dir_all(&fixture.migration.source.config_dir).expect("legacy config");
        fs::write(
            fixture
                .migration
                .source
                .config_dir
                .join("model-settings.json"),
            b"{}",
        )
        .expect("legacy settings");
        let before = fixture.snapshot_source();

        let marker = match fixture
            .migration
            .run(LegacyMigrationConsent::StartClean)
            .expect("decline migration")
        {
            LegacyMigrationOutcome::Declined(marker) => marker,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(marker.decision, LegacyMigrationDecision::Declined);
        assert_eq!(fixture.snapshot_source(), before);
        assert!(!fixture.migration.target.data_dir.exists());
        assert!(!fixture.migration.target.cache_dir.exists());
        assert_eq!(
            fixture
                .migration
                .run(LegacyMigrationConsent::Import)
                .expect("honor prior decision"),
            LegacyMigrationOutcome::AlreadyDecided(marker)
        );
    }

    #[test]
    fn import_uses_online_backups_preserves_safety_state_and_ignores_credentials() {
        let fixture = Fixture::new(true);
        fixture.write_history(SUPPORTED_HISTORY_SCHEMA_VERSION);
        fixture.write_gui_state(None);
        fs::create_dir_all(fixture.migration.source.config_dir.join("providers/fal"))
            .expect("legacy provider config");
        fs::write(
            fixture
                .migration
                .source
                .config_dir
                .join("app-settings.json"),
            br#"{"schema_version":1,"default_provider":"fal"}"#,
        )
        .expect("legacy app settings");
        fs::write(
            fixture
                .migration
                .source
                .config_dir
                .join("providers/fal/model-settings.json"),
            br#"{"fal/model":{"duration":5}}"#,
        )
        .expect("legacy provider settings");
        fs::write(
            fixture.migration.source.config_dir.join("credentials.json"),
            br#"{"api_key":"must-not-migrate"}"#,
        )
        .expect("legacy credential decoy");
        fs::create_dir_all(&fixture.migration.source.cache_dir).expect("legacy cache");
        fs::write(
            fixture.migration.source.cache_dir.join("video-models.json"),
            br#"{"models":[]}"#,
        )
        .expect("legacy catalog");
        let before = fixture.snapshot_source();

        let report = match fixture
            .migration
            .run(LegacyMigrationConsent::Import)
            .expect("import migration")
        {
            LegacyMigrationOutcome::Imported(report) => report,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(report.imported_history);
        assert!(report.imported_gui_state);
        assert_eq!(report.config_files.len(), 2);
        assert_eq!(report.cache_files, vec![PathBuf::from("video-models.json")]);
        assert_eq!(fixture.snapshot_source(), before);
        assert!(fixture.migration.marker_path().is_file());
        assert!(
            !fixture
                .migration
                .target
                .config_dir
                .join("credentials.json")
                .exists()
        );

        let history = Connection::open(fixture.migration.target.data_dir.join("history.sqlite3"))
            .expect("open imported history");
        let status: String = history
            .query_row(
                "SELECT status FROM jobs WHERE job_id = 'job-1'",
                [],
                |row| row.get(0),
            )
            .expect("read imported job");
        assert_eq!(status, "processing");

        let gui = Connection::open(fixture.migration.target.data_dir.join("gui-state.sqlite3"))
            .expect("open imported GUI state");
        let holds: i64 = gui
            .query_row("SELECT COUNT(*) FROM uncertain_submissions", [], |row| {
                row.get(0)
            })
            .expect("count imported safety holds");
        let resumable: i64 = gui
            .query_row("SELECT COUNT(*) FROM resumable_jobs", [], |row| row.get(0))
            .expect("count imported resumable jobs");
        assert_eq!((holds, resumable), (1, 1));
    }

    #[test]
    fn imported_draft_reports_missing_and_outside_sandbox_media() {
        let fixture = Fixture::new(true);
        let outside = fixture.root.path().join("Pictures/reference.png");
        fs::create_dir_all(outside.parent().expect("outside parent")).expect("outside directory");
        fs::write(&outside, b"image").expect("outside media");
        fixture.write_gui_state(Some(&outside));

        let report = match fixture
            .migration
            .run(LegacyMigrationConsent::Import)
            .expect("import GUI draft")
        {
            LegacyMigrationOutcome::Imported(report) => report,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(report.relink_required.len(), 1);
        assert_eq!(
            report.relink_required[0],
            RelinkRequired {
                draft_id: "current".into(),
                media_id: "legacy-frame".into(),
                path: outside,
                reason: RelinkReason::OutsideSandbox,
            }
        );
        assert_eq!(
            fixture
                .migration
                .pending_relinks()
                .expect("persisted relinks"),
            report.relink_required
        );
        assert!(
            fixture
                .migration
                .acknowledge_relink("current", "legacy-frame")
                .expect("acknowledge relink")
        );
        assert!(
            fixture
                .migration
                .pending_relinks()
                .expect("cleared relinks")
                .is_empty()
        );
    }

    #[test]
    fn accessible_video_media_does_not_require_relinking() {
        let fixture = Fixture::new(true);
        let media = fixture.root.path().join("home/Videos/reference.mp4");
        fs::create_dir_all(media.parent().expect("video parent")).expect("video directory");
        fs::write(&media, b"video").expect("video media");
        fixture.write_gui_state(Some(&media));
        let report = match fixture
            .migration
            .run(LegacyMigrationConsent::Import)
            .expect("import GUI draft")
        {
            LegacyMigrationOutcome::Imported(report) => report,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(report.relink_required.is_empty());
    }

    #[test]
    fn corrupt_database_fails_without_partial_target_and_can_retry() {
        let fixture = Fixture::new(true);
        fs::create_dir_all(&fixture.migration.source.data_dir).expect("legacy data");
        let history_path = fixture.migration.source.data_dir.join("history.sqlite3");
        fs::write(&history_path, b"not a sqlite database").expect("corrupt history");
        assert!(matches!(
            fixture.migration.run(LegacyMigrationConsent::Import),
            Err(LegacyMigrationError::Database { .. })
                | Err(LegacyMigrationError::DatabaseIntegrity { .. })
        ));
        assert!(
            first_target_entry(&fixture.migration.target)
                .expect("inspect failed target")
                .is_none()
        );
        assert!(!fixture.migration.marker_path().exists());

        fs::remove_file(&history_path).expect("remove corrupt history");
        fixture.write_history(SUPPORTED_HISTORY_SCHEMA_VERSION);
        assert!(matches!(
            fixture
                .migration
                .run(LegacyMigrationConsent::Import)
                .expect("retry migration"),
            LegacyMigrationOutcome::Imported(_)
        ));
    }

    #[test]
    fn corrupt_source_can_be_declined_after_import_failure() {
        let fixture = Fixture::new(true);
        fs::create_dir_all(&fixture.migration.source.data_dir).expect("legacy data");
        fs::write(
            fixture.migration.source.data_dir.join("history.sqlite3"),
            b"not a sqlite database",
        )
        .expect("corrupt history");
        assert!(
            fixture
                .migration
                .run(LegacyMigrationConsent::Import)
                .is_err()
        );
        assert!(matches!(
            fixture
                .migration
                .run(LegacyMigrationConsent::StartClean)
                .expect("start clean after failure"),
            LegacyMigrationOutcome::Declined(LegacyMigrationMarker {
                decision: LegacyMigrationDecision::Declined,
                ..
            })
        ));
        assert_eq!(
            fs::read(fixture.migration.source.data_dir.join("history.sqlite3"))
                .expect("legacy database unchanged"),
            b"not a sqlite database"
        );
    }

    #[test]
    fn newer_database_schema_and_invalid_json_fail_closed() {
        let fixture = Fixture::new(true);
        fixture.write_history(SUPPORTED_HISTORY_SCHEMA_VERSION + 1);
        assert!(matches!(
            fixture.migration.run(LegacyMigrationConsent::Import),
            Err(LegacyMigrationError::UnsupportedDatabaseSchema {
                database: "history",
                ..
            })
        ));
        assert!(
            first_target_entry(&fixture.migration.target)
                .expect("inspect failed target")
                .is_none()
        );

        fs::remove_file(fixture.migration.source.data_dir.join("history.sqlite3"))
            .expect("remove newer history");
        fs::create_dir_all(&fixture.migration.source.config_dir).expect("legacy config");
        fs::write(
            fixture
                .migration
                .source
                .config_dir
                .join("model-settings.json"),
            b"{broken",
        )
        .expect("invalid settings");
        assert!(matches!(
            fixture.migration.run(LegacyMigrationConsent::Import),
            Err(LegacyMigrationError::InvalidJson { .. })
        ));
        assert!(
            first_target_entry(&fixture.migration.target)
                .expect("inspect failed target")
                .is_none()
        );
        assert!(!fixture.migration.marker_path().exists());
    }

    #[test]
    fn credential_keys_in_known_settings_are_rejected_without_copying() {
        let fixture = Fixture::new(true);
        fs::create_dir_all(&fixture.migration.source.config_dir).expect("legacy config");
        fs::write(
            fixture
                .migration
                .source
                .config_dir
                .join("model-settings.json"),
            serde_json::to_vec(&json!({"model": {"api_key": "secret"}}))
                .expect("credential fixture"),
        )
        .expect("credential settings");
        assert!(matches!(
            fixture.migration.run(LegacyMigrationConsent::Import),
            Err(LegacyMigrationError::CredentialInSettings { .. })
        ));
        assert!(
            first_target_entry(&fixture.migration.target)
                .expect("inspect target")
                .is_none()
        );
    }

    #[test]
    fn newer_app_settings_are_not_committed() {
        let fixture = Fixture::new(true);
        fs::create_dir_all(&fixture.migration.source.config_dir).expect("legacy config");
        fs::write(
            fixture
                .migration
                .source
                .config_dir
                .join("app-settings.json"),
            br#"{"schema_version":99,"default_provider":"openrouter"}"#,
        )
        .expect("newer app settings");
        assert!(matches!(
            fixture.migration.run(LegacyMigrationConsent::Import),
            Err(LegacyMigrationError::UnsupportedAppSettingsSchema { found: 99, .. })
        ));
        assert!(
            first_target_entry(&fixture.migration.target)
                .expect("inspect target")
                .is_none()
        );
    }

    #[test]
    fn imported_sqlite_is_a_snapshot_not_a_plain_file_copy() {
        let fixture = Fixture::new(true);
        fs::create_dir_all(&fixture.migration.source.data_dir).expect("legacy data");
        let path = fixture.migration.source.data_dir.join("history.sqlite3");
        let connection = Connection::open(&path).expect("history connection");
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .expect("enable WAL");
        connection
            .execute("CREATE TABLE jobs (id TEXT PRIMARY KEY)", [])
            .expect("create jobs");
        connection
            .execute("INSERT INTO jobs VALUES (?1)", params!["wal-job"])
            .expect("insert WAL job");
        connection
            .pragma_update(None, "user_version", SUPPORTED_HISTORY_SCHEMA_VERSION)
            .expect("history schema");

        assert!(matches!(
            fixture
                .migration
                .run(LegacyMigrationConsent::Import)
                .expect("import live WAL database"),
            LegacyMigrationOutcome::Imported(_)
        ));
        let imported = Connection::open(fixture.migration.target.data_dir.join("history.sqlite3"))
            .expect("open imported database");
        let id: String = imported
            .query_row("SELECT id FROM jobs", [], |row| row.get(0))
            .expect("WAL row was backed up");
        assert_eq!(id, "wal-job");
    }
}
