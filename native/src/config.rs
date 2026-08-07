//! XDG-compatible application paths and safe video output names.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

use crate::atomic::replace_file;
use crate::domain::{OPENROUTER_PROVIDER_ID, ProviderId};

pub const APP_NAME: &str = "openrouter-video-studio";
pub const DEFAULT_VIDEO_SUFFIX: &str = ".mp4";
pub const APP_SETTINGS_FILE: &str = "app-settings.json";
pub const APP_SETTINGS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("HOME is unavailable; cannot discover application directories")]
    HomeUnavailable,
    #[error("could not create application directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not access model settings: {0}")]
    SettingsIo(#[from] std::io::Error),
    #[error("model settings contain invalid JSON: {0}")]
    SettingsJson(#[from] serde_json::Error),
    #[error("model settings must be a JSON object")]
    InvalidSettings,
    #[error("model settings may not contain credential fields")]
    CredentialInSettings,
    #[error("invalid provider id: {0}")]
    InvalidProvider(String),
    #[error(
        "app settings schema version {found} is unsupported; this build supports version {supported}"
    )]
    UnsupportedAppSettingsVersion { found: u32, supported: u32 },
}

pub type ModelSettingsMap = BTreeMap<String, Value>;

/// Small application-wide preferences. Provider/model-specific controls remain
/// in the existing model-settings files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "current_app_settings_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_provider")]
    pub default_provider: ProviderId,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: APP_SETTINGS_SCHEMA_VERSION,
            default_provider: ProviderId::openrouter(),
        }
    }
}

fn default_provider() -> ProviderId {
    ProviderId::openrouter()
}

const fn current_app_settings_schema_version() -> u32 {
    APP_SETTINGS_SCHEMA_VERSION
}

pub fn load_model_settings(path: &Path) -> Result<ModelSettingsMap, ConfigError> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(ConfigError::SettingsIo(error)),
    };
    let value: Value = serde_json::from_slice(&contents)?;
    if contains_credential_field(&value) {
        return Err(ConfigError::CredentialInSettings);
    }
    let object = value.as_object().ok_or(ConfigError::InvalidSettings)?;
    Ok(object
        .iter()
        .filter(|(_, settings)| settings.is_object())
        .map(|(model, settings)| (model.clone(), settings.clone()))
        .collect())
}

pub fn save_model_settings(
    path: &Path,
    model_id: &str,
    settings: Value,
) -> Result<(), ConfigError> {
    if model_id.trim().is_empty() || !settings.is_object() {
        return Err(ConfigError::InvalidSettings);
    }
    if contains_credential_field(&settings) {
        return Err(ConfigError::CredentialInSettings);
    }
    let mut all = load_model_settings(path)?;
    all.insert(model_id.to_owned(), settings);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_json_atomic(path, &all)?;
    Ok(())
}

fn contains_credential_field(value: &Value) -> bool {
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
                    | "authorization"
                    | "accesstoken"
                    | "authtoken"
                    | "bearertoken"
                    | "password"
                    | "secret"
                    | "secretkey"
            ) || contains_credential_field(value)
        }),
        Value::Array(values) => values.iter().any(contains_credential_field),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

pub fn load_app_settings(path: &Path) -> Result<AppSettings, ConfigError> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AppSettings::default());
        }
        Err(error) => return Err(ConfigError::SettingsIo(error)),
    };
    let settings: AppSettings = serde_json::from_slice(&contents)?;
    validate_app_settings(&settings)?;
    Ok(settings)
}

pub fn save_app_settings(path: &Path, settings: &AppSettings) -> Result<(), ConfigError> {
    validate_app_settings(settings)?;
    // Startup may deliberately fall back to defaults when settings cannot be
    // read. Never let that fallback overwrite a document owned by a newer
    // release (or malformed content that the user may need to recover).
    match fs::metadata(path) {
        Ok(_) => {
            load_app_settings(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ConfigError::SettingsIo(error)),
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_json_atomic(path, settings)
}

fn validate_app_settings(settings: &AppSettings) -> Result<(), ConfigError> {
    if settings.schema_version != APP_SETTINGS_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedAppSettingsVersion {
            found: settings.schema_version,
            supported: APP_SETTINGS_SCHEMA_VERSION,
        });
    }
    validated_provider_slug(settings.default_provider.as_str())?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
    pub videos_dir: PathBuf,
}

impl AppPaths {
    /// Construct application paths supplied by a platform integration.
    ///
    /// This performs no environment discovery and does not create any of the
    /// directories. Call [`Self::ensure`] when the paths are ready for use.
    pub fn new(
        data_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        config_dir: impl Into<PathBuf>,
        videos_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            data_dir: data_dir.into(),
            cache_dir: cache_dir.into(),
            config_dir: config_dir.into(),
            videos_dir: videos_dir.into(),
        }
    }

    pub fn discover() -> Result<Self, ConfigError> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(ConfigError::HomeUnavailable)?;
        Ok(Self::discover_from(home))
    }

    pub fn discover_from(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let data_home = xdg_dir("XDG_DATA_HOME", &home.join(".local/share"), &home);
        let cache_home = xdg_dir("XDG_CACHE_HOME", &home.join(".cache"), &home);
        let config_home = xdg_dir("XDG_CONFIG_HOME", &home.join(".config"), &home);
        let videos_dir = discover_videos_dir_from(&home, &config_home);
        Self::new(
            data_home.join(APP_NAME),
            cache_home.join(APP_NAME),
            config_home.join(APP_NAME),
            videos_dir,
        )
    }

    pub fn history_db(&self) -> PathBuf {
        self.data_dir.join("history.sqlite3")
    }

    /// GUI editing and upload state lives beside, but never inside, the
    /// compatibility-sensitive generation history database.
    pub fn gui_state_db(&self) -> PathBuf {
        self.data_dir.join("gui-state.sqlite3")
    }

    pub fn catalog_cache(&self) -> PathBuf {
        self.cache_dir.join("video-models.json")
    }

    /// Provider-scoped catalog cache. OpenRouter deliberately retains its
    /// original path so a rollback can continue to use the same catalog.
    pub fn provider_catalog_cache(&self, provider_id: &ProviderId) -> Result<PathBuf, ConfigError> {
        let slug = validated_provider_slug(provider_id.as_str())?;
        if slug == OPENROUTER_PROVIDER_ID {
            Ok(self.catalog_cache())
        } else {
            Ok(self
                .cache_dir
                .join("providers")
                .join(slug)
                .join("video-models.json"))
        }
    }

    pub fn model_settings(&self) -> PathBuf {
        self.config_dir.join("model-settings.json")
    }

    /// Provider-scoped model controls. OpenRouter deliberately retains its
    /// original path for Python and Rust v0.1 compatibility.
    pub fn provider_model_settings(
        &self,
        provider_id: &ProviderId,
    ) -> Result<PathBuf, ConfigError> {
        let slug = validated_provider_slug(provider_id.as_str())?;
        if slug == OPENROUTER_PROVIDER_ID {
            Ok(self.model_settings())
        } else {
            Ok(self
                .config_dir
                .join("providers")
                .join(slug)
                .join("model-settings.json"))
        }
    }

    pub fn app_settings(&self) -> PathBuf {
        self.config_dir.join(APP_SETTINGS_FILE)
    }

    pub fn ensure(self) -> Result<Self, ConfigError> {
        self.ensure_dirs()?;
        Ok(self)
    }

    pub fn ensure_dirs(&self) -> Result<(), ConfigError> {
        for path in [&self.data_dir, &self.cache_dir, &self.config_dir] {
            fs::create_dir_all(path).map_err(|source| ConfigError::CreateDirectory {
                path: path.clone(),
                source,
            })?;
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
                ConfigError::CreateDirectory {
                    path: path.clone(),
                    source,
                }
            })?;
        }
        // The user's normal Videos folder is intentionally not made private or
        // otherwise re-permissioned by this application.
        fs::create_dir_all(&self.videos_dir).map_err(|source| ConfigError::CreateDirectory {
            path: self.videos_dir.clone(),
            source,
        })?;
        Ok(())
    }
}

/// Validate the path component independently of the domain type. Keeping this
/// check at the filesystem boundary prevents future or corrupted identifiers
/// from escaping the provider namespace.
pub fn validated_provider_slug(value: &str) -> Result<&str, ConfigError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(value)
    } else {
        Err(ConfigError::InvalidProvider(value.to_owned()))
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), ConfigError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("settings.json");
    let process = std::process::id();
    let mut last_collision = None;
    for index in 0u16..=u16::MAX {
        let temporary = path.with_file_name(format!(".{file_name}.tmp-{process}-{index}"));
        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary);
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(ConfigError::SettingsIo(error)),
        };
        let result = (|| -> Result<(), std::io::Error> {
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary, path)?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigError::SettingsIo(error));
        }
        return Ok(());
    }
    Err(ConfigError::SettingsIo(last_collision.unwrap_or_else(
        || {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a settings temporary file",
            )
        },
    )))
}

fn xdg_dir(variable: &str, fallback: &Path, home: &Path) -> PathBuf {
    env::var_os(variable)
        .and_then(|value| expand_tilde(PathBuf::from(value), home))
        .filter(|value| value.is_absolute())
        .unwrap_or_else(|| fallback.to_owned())
}

fn expand_tilde(value: PathBuf, home: &Path) -> Option<PathBuf> {
    let text = value.to_str()?;
    if text == "~" {
        return Some(home.to_owned());
    }
    if let Some(suffix) = text.strip_prefix("~/") {
        return Some(home.join(suffix));
    }
    Some(value)
}

pub fn discover_videos_dir() -> Result<PathBuf, ConfigError> {
    let paths = AppPaths::discover()?;
    Ok(paths.videos_dir)
}

pub fn discover_videos_dir_from(home: &Path, config_home: &Path) -> PathBuf {
    if let Some(value) = env::var_os("XDG_VIDEOS_DIR")
        && let Some(path) = parse_user_dir(&value.to_string_lossy(), home)
    {
        return path;
    }

    if let Ok(contents) = fs::read_to_string(config_home.join("user-dirs.dirs")) {
        for line in contents.lines().map(str::trim) {
            if let Some(value) = line.strip_prefix("XDG_VIDEOS_DIR=")
                && let Some(path) = parse_user_dir(value, home)
            {
                return path;
            }
        }
    }
    home.join("Videos")
}

fn parse_user_dir(value: &str, home: &Path) -> Option<PathBuf> {
    let mut value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value = &value[1..value.len() - 1];
    }
    let home_text = home.to_str()?;
    let expanded = value
        .replace("${HOME}", home_text)
        .replace("$HOME", home_text);
    if expanded.contains('$') || expanded.contains('`') {
        return None;
    }
    expand_tilde(PathBuf::from(expanded), home).filter(|path| path.is_absolute())
}

pub fn slugify_prompt(prompt: &str, max_length: usize) -> String {
    let ascii = prompt
        .nfkd()
        .filter(char::is_ascii)
        .collect::<String>()
        .to_ascii_lowercase();
    let mut slug = collapse_component(&ascii, |character| character.is_ascii_alphanumeric());
    if slug.len() > max_length {
        slug.truncate(max_length);
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    if slug.is_empty() {
        "video".into()
    } else {
        slug
    }
}

fn collapse_component<F>(value: &str, accepted: F) -> String
where
    F: Fn(char) -> bool,
{
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if accepted(character) {
            if separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    output
}

fn safe_job_component(value: &str) -> String {
    let mut component = collapse_component(value, |character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
    });
    if component.len() > 20 {
        component.truncate(20);
    }
    if component.is_empty() {
        "job".into()
    } else {
        component
    }
}

fn clean_suffix(value: &str) -> String {
    let valid = value.strip_prefix('.').is_some_and(|suffix| {
        (1..=8).contains(&suffix.len())
            && suffix
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
    });
    if valid {
        value.to_ascii_lowercase()
    } else {
        DEFAULT_VIDEO_SUFFIX.into()
    }
}

pub fn make_output_path(prompt: &str, job_id: &str, videos_dir: &Path) -> PathBuf {
    make_output_path_at(
        prompt,
        job_id,
        videos_dir,
        Local::now(),
        DEFAULT_VIDEO_SUFFIX,
    )
}

pub fn make_output_path_at(
    prompt: &str,
    job_id: &str,
    videos_dir: &Path,
    now: DateTime<Local>,
    suffix: &str,
) -> PathBuf {
    let timestamp = now.format("%Y%m%d-%H%M%S");
    let suffix = clean_suffix(suffix);
    let stem = format!(
        "{timestamp}-{}-{}",
        slugify_prompt(prompt, 48),
        safe_job_component(job_id)
    );
    let mut candidate = videos_dir.join(format!("{stem}{suffix}"));
    let mut index = 2u32;
    while candidate.exists() || partial_path(&candidate).exists() {
        candidate = videos_dir.join(format!("{stem}-{index}{suffix}"));
        index += 1;
    }
    candidate
}

pub fn partial_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("video.mp4");
    target.with_file_name(format!("{name}.part"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn injected_app_paths_are_exact_and_derive_storage_locations() {
        let root = PathBuf::from("injected-platform-paths");
        let data = root.join("data");
        let cache = root.join("cache");
        let config = root.join("config");
        let videos = root.join("finished-videos");
        let paths = AppPaths::new(&data, &cache, &config, &videos);

        assert_eq!(paths.data_dir, data);
        assert_eq!(paths.cache_dir, cache);
        assert_eq!(paths.config_dir, config);
        assert_eq!(paths.videos_dir, videos);
        assert_eq!(paths.history_db(), data.join("history.sqlite3"));
        assert_eq!(paths.gui_state_db(), data.join("gui-state.sqlite3"));
        assert_eq!(paths.catalog_cache(), cache.join("video-models.json"));
        assert_eq!(paths.model_settings(), config.join("model-settings.json"));
        assert_eq!(paths.app_settings(), config.join(APP_SETTINGS_FILE));

        assert_eq!(
            paths
                .provider_catalog_cache(&ProviderId::openrouter())
                .expect("OpenRouter catalog path"),
            cache.join("video-models.json")
        );
        assert_eq!(
            paths
                .provider_model_settings(&ProviderId::openrouter())
                .expect("OpenRouter settings path"),
            config.join("model-settings.json")
        );
        assert_eq!(
            paths
                .provider_catalog_cache(&ProviderId::fal())
                .expect("fal catalog path"),
            cache
                .join("providers")
                .join("fal")
                .join("video-models.json")
        );
        assert_eq!(
            paths
                .provider_model_settings(&ProviderId::fal())
                .expect("fal settings path"),
            config
                .join("providers")
                .join("fal")
                .join("model-settings.json")
        );
    }

    #[test]
    fn remembered_model_settings_reject_credential_fields_recursively() {
        let directory = tempdir().expect("settings directory");
        let path = directory.path().join("model-settings.json");
        save_model_settings(
            &path,
            "example/model",
            json!({"duration": 5, "generate_audio": null}),
        )
        .expect("credential-free controls");
        let original = fs::read(&path).expect("saved settings");

        for settings in [
            json!({"api_key": "must-not-persist"}),
            json!({"nested": {"Authorization": "Bearer must-not-persist"}}),
            json!({"items": [{"access-token": "must-not-persist"}]}),
        ] {
            assert!(matches!(
                save_model_settings(&path, "example/model", settings),
                Err(ConfigError::CredentialInSettings)
            ));
            assert_eq!(fs::read(&path).expect("unchanged settings"), original);
        }

        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "example/model": {"nested": {"secret_key": "must-not-load"}}
            }))
            .expect("malicious settings fixture"),
        )
        .expect("write malicious settings fixture");
        assert!(matches!(
            load_model_settings(&path),
            Err(ConfigError::CredentialInSettings)
        ));
    }
}
