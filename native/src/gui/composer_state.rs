//! Pure selection and precedence rules for the generation composer.
//!
//! GTK widgets are projections of this state. Keeping catalog ordering and
//! persistence precedence here prevents rendering callbacks from becoming
//! accidental edits.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{ProviderId, VideoCatalog, VideoModel};

pub const COMPACT_MAX_WIDTH: u32 = 799;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellLayout {
    pub header_switcher: bool,
    pub bottom_switcher: bool,
    pub inspector_pinned: bool,
}

pub const fn shell_layout_for_width(width: u32) -> ShellLayout {
    let compact = width <= COMPACT_MAX_WIDTH;
    ShellLayout {
        header_switcher: !compact,
        bottom_switcher: compact,
        inspector_pinned: !compact,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioChoice {
    ProviderDefault,
    On,
    Off,
}

impl AudioChoice {
    pub const fn request_value(self) -> Option<bool> {
        match self {
            Self::ProviderDefault => None,
            Self::On => Some(true),
            Self::Off => Some(false),
        }
    }

    pub const fn from_request(value: Option<bool>) -> Self {
        match value {
            None => Self::ProviderDefault,
            Some(true) => Self::On,
            Some(false) => Self::Off,
        }
    }

    pub const fn selected(self) -> u32 {
        match self {
            Self::ProviderDefault => 0,
            Self::On => 1,
            Self::Off => 2,
        }
    }

    pub const fn from_selected(selected: u32) -> Self {
        match selected {
            1 => Self::On,
            2 => Self::Off,
            _ => Self::ProviderDefault,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelKey {
    pub provider_id: ProviderId,
    pub model_id: String,
}

impl ModelKey {
    pub fn new(provider_id: ProviderId, model_id: impl Into<String>) -> Self {
        Self {
            provider_id,
            model_id: model_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelection {
    Available(ModelKey),
    /// A restored draft names a model absent from the current catalog. It is
    /// intentionally not redirected to the preferred model.
    Unavailable(ModelKey),
    Empty(ProviderId),
}

pub fn resolve_selection(
    catalog: &VideoCatalog,
    exact_saved_model: Option<&str>,
    previous_model: Option<&str>,
    brand_new: bool,
) -> ModelSelection {
    let available = |model_id: &str| catalog.find(model_id).is_some();
    if let Some(model_id) = exact_saved_model {
        let key = ModelKey::new(catalog.provider_id.clone(), model_id);
        return if available(model_id) {
            ModelSelection::Available(key)
        } else {
            ModelSelection::Unavailable(key)
        };
    }
    if let Some(model_id) = previous_model.filter(|model_id| available(model_id)) {
        return ModelSelection::Available(ModelKey::new(catalog.provider_id.clone(), model_id));
    }
    if brand_new && let Some(model) = catalog.preferred() {
        return ModelSelection::Available(ModelKey::new(catalog.provider_id.clone(), &model.id));
    }
    ModelSelection::Empty(catalog.provider_id.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSource {
    ExactDraft,
    Remembered,
    ProviderDefault,
}

pub fn resolve_setting<T: Clone>(
    exact_draft: Option<&T>,
    remembered: Option<&T>,
    provider_default: &T,
) -> (T, SettingsSource) {
    if let Some(value) = exact_draft {
        (value.clone(), SettingsSource::ExactDraft)
    } else if let Some(value) = remembered {
        (value.clone(), SettingsSource::Remembered)
    } else {
        (provider_default.clone(), SettingsSource::ProviderDefault)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CapabilityFingerprint {
    generated_audio_supported: bool,
    generated_audio_default: Option<bool>,
    seed: Option<bool>,
    durations: Vec<u32>,
    resolutions: Vec<String>,
    aspect_ratios: Vec<String>,
    sizes: Vec<String>,
    frame_images: Vec<String>,
    input_schema: Option<Value>,
    pricing: BTreeMap<String, rust_decimal::Decimal>,
}

impl From<&VideoModel> for CapabilityFingerprint {
    fn from(model: &VideoModel) -> Self {
        Self {
            generated_audio_supported: model.generated_audio.supported,
            generated_audio_default: model.generated_audio.provider_default,
            seed: model.seed,
            durations: model.supported_durations.clone(),
            resolutions: model.supported_resolutions.clone(),
            aspect_ratios: model.supported_aspect_ratios.clone(),
            sizes: model.supported_sizes.clone(),
            frame_images: model.supported_frame_images.clone(),
            input_schema: model.input_schema.clone(),
            pricing: model.pricing_skus.clone(),
        }
    }
}

/// Deduplicates cached/live catalog emissions and reports only selected-model
/// changes that can invalidate Review.
#[derive(Debug, Default)]
pub struct CatalogReducer {
    catalogs: BTreeMap<ProviderId, BTreeMap<String, CapabilityFingerprint>>,
}

impl CatalogReducer {
    pub fn apply(&mut self, catalog: &VideoCatalog, selected: Option<&ModelKey>) -> bool {
        let next = catalog
            .models
            .iter()
            .map(|model| (model.id.clone(), CapabilityFingerprint::from(model)))
            .collect::<BTreeMap<_, _>>();
        let previous = self
            .catalogs
            .insert(catalog.provider_id.clone(), next.clone());
        let Some(selected) = selected.filter(|key| key.provider_id == catalog.provider_id) else {
            return false;
        };
        let before = previous
            .as_ref()
            .and_then(|models| models.get(&selected.model_id));
        let after = next.get(&selected.model_id);
        before.is_some() && before != after
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    fn catalog(models: &[Value]) -> VideoCatalog {
        VideoCatalog::from_api_at(&json!({"data": models}), Utc::now(), false).expect("catalog")
    }

    #[test]
    fn audio_choice_is_an_exact_three_way_mapping() {
        for (choice, request, selected) in [
            (AudioChoice::ProviderDefault, None, 0),
            (AudioChoice::On, Some(true), 1),
            (AudioChoice::Off, Some(false), 2),
        ] {
            assert_eq!(choice.request_value(), request);
            assert_eq!(AudioChoice::from_request(request), choice);
            assert_eq!(choice.selected(), selected);
            assert_eq!(AudioChoice::from_selected(selected), choice);
        }
    }

    #[test]
    fn missing_saved_model_never_falls_back_but_new_drafts_can() {
        let catalog = catalog(&[
            json!({"id": "example/first"}),
            json!({"id": "black-forest-labs/flux-3-video"}),
        ]);
        assert!(matches!(
            resolve_selection(&catalog, Some("removed/model"), None, false),
            ModelSelection::Unavailable(key) if key.model_id == "removed/model"
        ));
        assert!(matches!(
            resolve_selection(&catalog, None, None, true),
            ModelSelection::Available(key) if key.model_id == "black-forest-labs/flux-3-video"
        ));
    }

    #[test]
    fn exact_then_remembered_then_default_precedence_is_stable() {
        assert_eq!(
            resolve_setting(Some(&3), Some(&2), &1),
            (3, SettingsSource::ExactDraft)
        );
        assert_eq!(
            resolve_setting(None, Some(&2), &1),
            (2, SettingsSource::Remembered)
        );
        assert_eq!(
            resolve_setting(None, None, &1),
            (1, SettingsSource::ProviderDefault)
        );
    }

    #[test]
    fn catalog_order_is_idempotent_and_capability_changes_invalidate_once() {
        let first = catalog(&[json!({"id": "example/model", "generate_audio": true})]);
        let changed = catalog(&[json!({"id": "example/model", "generate_audio": false})]);
        let key = ModelKey::new(ProviderId::openrouter(), "example/model");
        let mut reducer = CatalogReducer::default();
        assert!(!reducer.apply(&first, Some(&key)));
        assert!(!reducer.apply(&first, Some(&key)));
        assert!(reducer.apply(&changed, Some(&key)));
        assert!(!reducer.apply(&changed, Some(&key)));
    }

    #[test]
    fn responsive_shell_has_exactly_one_switcher_at_supported_widths() {
        for width in [480, 720, 1_100] {
            let layout = shell_layout_for_width(width);
            assert_ne!(layout.header_switcher, layout.bottom_switcher);
            assert_eq!(layout.inspector_pinned, width > COMPACT_MAX_WIDTH);
        }
    }
}
