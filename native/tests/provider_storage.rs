use std::fs;

use openrouter_video_studio::config::{
    APP_NAME, APP_SETTINGS_SCHEMA_VERSION, AppPaths, AppSettings, ConfigError, load_app_settings,
    save_app_settings, validated_provider_slug,
};
use openrouter_video_studio::credentials::{
    CredentialStore, DEFAULT_USERNAME, FAL_USERNAME, username_for_provider,
};
use openrouter_video_studio::domain::ProviderId;
use secrecy::ExposeSecret;
use tempfile::tempdir;

#[test]
fn provider_paths_retain_openrouter_compatibility_and_namespace_fal() {
    let directory = tempdir().expect("temporary directory");
    let paths = AppPaths {
        data_dir: directory.path().join("data"),
        cache_dir: directory.path().join("cache"),
        config_dir: directory.path().join("config"),
        videos_dir: directory.path().join("Videos"),
    };
    assert_eq!(
        paths
            .provider_catalog_cache(&ProviderId::openrouter())
            .expect("OpenRouter cache"),
        paths.catalog_cache()
    );
    assert_eq!(
        paths
            .provider_model_settings(&ProviderId::openrouter())
            .expect("OpenRouter settings"),
        paths.model_settings()
    );
    assert_eq!(
        paths
            .provider_catalog_cache(&ProviderId::fal())
            .expect("fal cache"),
        paths.cache_dir.join("providers/fal/video-models.json")
    );
    assert_eq!(
        paths
            .provider_model_settings(&ProviderId::fal())
            .expect("fal settings"),
        paths.config_dir.join("providers/fal/model-settings.json")
    );
    assert_eq!(
        paths.app_settings(),
        paths.config_dir.join("app-settings.json")
    );
    assert!(validated_provider_slug("fal-2").is_ok());
    for unsafe_value in ["../fal", "Fal", "fal/provider", "", "."] {
        assert!(validated_provider_slug(unsafe_value).is_err());
    }
}

#[test]
fn app_settings_default_and_atomic_round_trip_are_versioned() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("config/app-settings.json");
    let default = load_app_settings(&path).expect("missing settings use defaults");
    assert_eq!(default.schema_version, APP_SETTINGS_SCHEMA_VERSION);
    assert_eq!(default.default_provider, ProviderId::openrouter());

    let settings = AppSettings {
        schema_version: APP_SETTINGS_SCHEMA_VERSION,
        default_provider: ProviderId::fal(),
    };
    save_app_settings(&path, &settings).expect("atomically save settings");
    assert_eq!(load_app_settings(&path).expect("reload settings"), settings);
    let leftovers = fs::read_dir(path.parent().expect("settings parent"))
        .expect("list settings directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        .count();
    assert_eq!(leftovers, 0);

    fs::write(&path, r#"{"schema_version":99,"default_provider":"fal"}"#)
        .expect("write future settings");
    let future_contents = fs::read(&path).expect("read future settings");
    assert!(matches!(
        load_app_settings(&path),
        Err(ConfigError::UnsupportedAppSettingsVersion {
            found: 99,
            supported: APP_SETTINGS_SCHEMA_VERSION
        })
    ));
    assert!(matches!(
        save_app_settings(&path, &AppSettings::default()),
        Err(ConfigError::UnsupportedAppSettingsVersion {
            found: 99,
            supported: APP_SETTINGS_SCHEMA_VERSION
        })
    ));
    assert_eq!(
        fs::read(&path).expect("future settings remain readable"),
        future_contents
    );

    fs::write(&path, b"{not valid json").expect("write malformed settings");
    let malformed_contents = fs::read(&path).expect("read malformed settings");
    assert!(matches!(
        save_app_settings(&path, &AppSettings::default()),
        Err(ConfigError::SettingsJson(_))
    ));
    assert_eq!(
        fs::read(&path).expect("malformed settings remain readable"),
        malformed_contents
    );
}

#[test]
fn provider_credentials_use_exact_identifiers_and_isolated_sessions() {
    assert_eq!(APP_NAME, "openrouter-video-studio");
    assert_eq!(DEFAULT_USERNAME, "openrouter-api-key");
    assert_eq!(FAL_USERNAME, "provider:fal:api-key");
    assert_eq!(
        username_for_provider(&ProviderId::openrouter()),
        DEFAULT_USERNAME
    );
    assert_eq!(username_for_provider(&ProviderId::fal()), FAL_USERNAME);

    let mut openrouter = CredentialStore::memory_only_for_provider(&ProviderId::openrouter());
    let mut fal = CredentialStore::memory_only_for_provider(&ProviderId::fal());
    openrouter
        .set_str("sk-or-session")
        .expect("set OpenRouter session key");
    fal.set_str("fal-session-key").expect("set fal session key");
    assert_eq!(
        openrouter.get().expect("OpenRouter key").expose_secret(),
        "sk-or-session"
    );
    assert_eq!(
        fal.get().expect("fal key").expose_secret(),
        "fal-session-key"
    );
    fal.delete();
    assert!(fal.get().is_none());
    assert_eq!(
        openrouter
            .get()
            .expect("OpenRouter remains connected")
            .expose_secret(),
        "sk-or-session"
    );
    let debug = format!("{openrouter:?}");
    assert!(!debug.contains("sk-or-session"));
}
