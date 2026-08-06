//! Provider-neutral video requests, catalogs, jobs, and cost quotes.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use thiserror::Error;
use url::Url;

pub const PREFERRED_MODEL_ID: &str = "black-forest-labs/flux-3-video";
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
pub const FAL_PROVIDER_ID: &str = "fal";

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("{0}")]
    Validation(String),
    #[error("model catalog response does not contain a data list")]
    InvalidCatalog,
    #[error("could not read or write the model catalog: {0}")]
    Io(#[from] std::io::Error),
    #[error("model catalog contains invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Stable, persistence-safe identifier for a video provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let normalized = value.trim().to_ascii_lowercase();
        let valid = !normalized.is_empty()
            && normalized.len() <= 64
            && normalized
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid {
            return Err(DomainError::Validation(
                "provider id must use lowercase letters, digits, and hyphens".into(),
            ));
        }
        Ok(Self(normalized))
    }

    pub fn openrouter() -> Self {
        Self(OPENROUTER_PROVIDER_ID.into())
    }

    pub fn fal() -> Self {
        Self(FAL_PROVIDER_ID.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProviderId {
    fn default() -> Self {
        Self::openrouter()
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderId {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ModelRef {
    pub provider_id: ProviderId,
    pub model_id: String,
}

impl ModelRef {
    pub fn new(provider_id: ProviderId, model_id: impl Into<String>) -> Result<Self, DomainError> {
        let model_id = model_id.into().trim().to_owned();
        if model_id.is_empty() || model_id.len() > 256 || model_id.chars().any(char::is_control) {
            return Err(DomainError::Validation("model id is invalid".into()));
        }
        Ok(Self {
            provider_id,
            model_id,
        })
    }
}

impl<'de> Deserialize<'de> for ModelRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            provider_id: ProviderId,
            model_id: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.provider_id, wire.model_id).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ProviderJobKey {
    pub provider_id: ProviderId,
    pub remote_job_id: String,
}

impl ProviderJobKey {
    pub fn new(
        provider_id: ProviderId,
        remote_job_id: impl Into<String>,
    ) -> Result<Self, DomainError> {
        let remote_job_id = remote_job_id.into().trim().to_owned();
        if remote_job_id.is_empty()
            || remote_job_id.len() > 1024
            || remote_job_id.chars().any(char::is_control)
        {
            return Err(DomainError::Validation("remote job id is invalid".into()));
        }
        Ok(Self {
            provider_id,
            remote_job_id,
        })
    }
}

impl<'de> Deserialize<'de> for ProviderJobKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            provider_id: ProviderId,
            remote_job_id: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.provider_id, wire.remote_job_id).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum JobLocator {
    OpenRouter {
        polling_url: String,
    },
    Fal {
        endpoint_id: String,
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_url: Option<String>,
    },
}

impl<'de> Deserialize<'de> for JobLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "provider", rename_all = "snake_case")]
        enum Wire {
            OpenRouter {
                polling_url: String,
            },
            Fal {
                endpoint_id: String,
                request_id: String,
                #[serde(default)]
                status_url: Option<String>,
                #[serde(default)]
                response_url: Option<String>,
            },
        }
        let locator = match Wire::deserialize(deserializer)? {
            Wire::OpenRouter { polling_url } => Self::OpenRouter { polling_url },
            Wire::Fal {
                endpoint_id,
                request_id,
                status_url,
                response_url,
            } => Self::Fal {
                endpoint_id,
                request_id,
                status_url,
                response_url,
            },
        };
        locator.validate().map_err(serde::de::Error::custom)?;
        Ok(locator)
    }
}

impl JobLocator {
    pub fn provider_id(&self) -> ProviderId {
        match self {
            Self::OpenRouter { .. } => ProviderId::openrouter(),
            Self::Fal { .. } => ProviderId::fal(),
        }
    }

    pub fn remote_job_id(&self) -> &str {
        match self {
            Self::OpenRouter { polling_url } => polling_url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(polling_url),
            Self::Fal { request_id, .. } => request_id,
        }
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        match self {
            Self::OpenRouter { polling_url } => {
                if polling_url.trim().is_empty() || polling_url.chars().any(char::is_control) {
                    return Err(DomainError::Validation(
                        "OpenRouter polling locator is invalid".into(),
                    ));
                }
            }
            Self::Fal {
                endpoint_id,
                request_id,
                status_url,
                response_url,
            } => {
                ModelRef::new(ProviderId::fal(), endpoint_id)?;
                ProviderJobKey::new(ProviderId::fal(), request_id)?;
                let base_path = format!(
                    "/{}/requests/{}",
                    endpoint_id.trim_matches('/'),
                    request_id.trim_matches('/')
                );
                if let Some(value) = status_url {
                    validate_fal_queue_url(
                        value,
                        "fal status URL",
                        &[format!("{base_path}/status")],
                    )?;
                }
                if let Some(value) = response_url {
                    validate_fal_queue_url(
                        value,
                        "fal response URL",
                        &[base_path.clone(), format!("{base_path}/response")],
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn validate_fal_queue_url(
    value: &str,
    label: &str,
    allowed_paths: &[String],
) -> Result<(), DomainError> {
    validate_public_https_url(value, label)?;
    let url = Url::parse(value)
        .map_err(|_| DomainError::Validation(format!("{label} must be a public HTTPS URL")))?;
    if url.host_str() != Some("queue.fal.run")
        || url.query().is_some()
        || url.fragment().is_some()
        || !allowed_paths.iter().any(|path| path == url.path())
    {
        return Err(DomainError::Validation(format!(
            "{label} is not the expected fal queue request URL"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoArtifact {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub index: usize,
}

impl VideoArtifact {
    pub fn new(url: impl Into<String>, index: usize) -> Result<Self, DomainError> {
        let artifact = Self {
            url: url.into(),
            content_type: None,
            index,
        };
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        validate_public_https_url(&self.url, "Video artifact")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: String,
    pub website: String,
}

fn validate_public_https_url(value: &str, label: &str) -> Result<(), DomainError> {
    let url = Url::parse(value)
        .map_err(|_| DomainError::Validation(format!("{label} must be a public HTTPS URL")))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(DomainError::Validation(format!(
            "{label} must be a public HTTPS URL"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(DomainError::Validation(format!(
            "{label} must not contain embedded credentials"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameType {
    FirstFrame,
    LastFrame,
}

impl FrameType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstFrame => "first_frame",
            Self::LastFrame => "last_frame",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameImage {
    pub url: String,
    pub frame_type: FrameType,
}

impl FrameImage {
    pub fn new(url: impl Into<String>, frame_type: FrameType) -> Result<Self, DomainError> {
        let value = Self {
            url: url.into(),
            frame_type,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        validate_public_https_url(&self.url, "Frame image")
    }

    pub fn to_payload(&self) -> Value {
        json!({
            "type": "image_url",
            "image_url": {"url": self.url},
            "frame_type": self.frame_type.as_str(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputReference {
    pub url: String,
}

impl InputReference {
    pub fn new(url: impl Into<String>) -> Result<Self, DomainError> {
        let value = Self { url: url.into() };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        validate_public_https_url(&self.url, "Input reference")
    }

    pub fn to_payload(&self) -> Value {
        json!({"type": "image_url", "image_url": {"url": self.url}})
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoRequest {
    pub provider_id: ProviderId,
    pub model: String,
    pub prompt: String,
    pub duration: Option<u32>,
    pub resolution: Option<String>,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    pub generate_audio: Option<bool>,
    pub seed: Option<i64>,
    pub frame_images: Vec<FrameImage>,
    pub input_references: Vec<InputReference>,
    /// Provider-specific, schema-validated input fields.
    pub adapter_options: Option<Value>,
}

impl VideoRequest {
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Result<Self, DomainError> {
        Self::for_provider(ProviderId::openrouter(), model, prompt)
    }

    pub fn for_provider(
        provider_id: ProviderId,
        model: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<Self, DomainError> {
        let request = Self {
            provider_id,
            model: model.into(),
            prompt: prompt.into(),
            duration: None,
            resolution: None,
            aspect_ratio: None,
            size: None,
            generate_audio: None,
            seed: None,
            frame_images: Vec::new(),
            input_references: Vec::new(),
            adapter_options: None,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        if self.model.trim().is_empty() {
            return Err(DomainError::Validation("model is required".into()));
        }
        if self.prompt.trim().is_empty() {
            return Err(DomainError::Validation("prompt is required".into()));
        }
        if self.duration == Some(0) {
            return Err(DomainError::Validation(
                "duration must be at least 1 second".into(),
            ));
        }
        if let Some(size) = &self.size {
            let valid = size.split_once('x').is_some_and(|(width, height)| {
                !width.starts_with('0')
                    && !height.starts_with('0')
                    && width.parse::<u32>().is_ok_and(|value| value > 0)
                    && height.parse::<u32>().is_ok_and(|value| value > 0)
            });
            if !valid {
                return Err(DomainError::Validation(
                    "size must use WIDTHxHEIGHT, for example 1280x720".into(),
                ));
            }
            if self.resolution.is_some() || self.aspect_ratio.is_some() {
                return Err(DomainError::Validation(
                    "size cannot be combined with resolution or aspect_ratio".into(),
                ));
            }
        }
        if self
            .adapter_options
            .as_ref()
            .is_some_and(|value| !value.is_object())
        {
            return Err(DomainError::Validation(
                "adapter_options must be a JSON object".into(),
            ));
        }
        for frame in &self.frame_images {
            frame.validate()?;
        }
        for reference in &self.input_references {
            reference.validate()?;
        }
        Ok(())
    }

    pub fn model_ref(&self) -> ModelRef {
        ModelRef {
            provider_id: self.provider_id.clone(),
            model_id: self.model.clone(),
        }
    }

    pub fn to_payload(&self) -> Result<Value, DomainError> {
        self.validate()?;
        let mut payload = Map::new();
        payload.insert("model".into(), Value::String(self.model.trim().into()));
        payload.insert("prompt".into(), Value::String(self.prompt.trim().into()));
        if let Some(value) = self.duration {
            payload.insert("duration".into(), Value::from(value));
        }
        if let Some(value) = &self.resolution {
            payload.insert("resolution".into(), Value::String(value.clone()));
        }
        if let Some(value) = &self.aspect_ratio {
            payload.insert("aspect_ratio".into(), Value::String(value.clone()));
        }
        if let Some(value) = &self.size {
            payload.insert("size".into(), Value::String(value.clone()));
        }
        if let Some(value) = self.generate_audio {
            payload.insert("generate_audio".into(), Value::Bool(value));
        }
        if let Some(value) = self.seed {
            payload.insert("seed".into(), Value::from(value));
        }
        if !self.frame_images.is_empty() {
            payload.insert(
                "frame_images".into(),
                Value::Array(
                    self.frame_images
                        .iter()
                        .map(FrameImage::to_payload)
                        .collect(),
                ),
            );
        }
        if !self.input_references.is_empty() {
            payload.insert(
                "input_references".into(),
                Value::Array(
                    self.input_references
                        .iter()
                        .map(InputReference::to_payload)
                        .collect(),
                ),
            );
        }
        if let Some(adapter_options) = &self.adapter_options {
            // This method is the legacy OpenRouter wire serializer. Other
            // providers build their input behind their adapter boundary.
            payload.insert("provider".into(), adapter_options.clone());
        }
        Ok(Value::Object(payload))
    }

    pub fn from_payload(payload: &Value) -> Result<Self, DomainError> {
        let object = payload.as_object().ok_or_else(|| {
            DomainError::Validation("video request payload must be a JSON object".into())
        })?;
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let prompt = object
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let provider_id = object
            .get("provider_id")
            .and_then(Value::as_str)
            .map(ProviderId::new)
            .transpose()?
            .unwrap_or_else(ProviderId::openrouter);
        let mut request = Self::for_provider(provider_id, model, prompt)?;
        request.duration = match object.get("duration") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| {
                        DomainError::Validation(
                            "duration must be an integer number of seconds".into(),
                        )
                    })?,
            ),
        };
        request.resolution = string_option(object.get("resolution"));
        request.aspect_ratio = string_option(object.get("aspect_ratio"));
        request.size = string_option(object.get("size"));
        request.generate_audio = object.get("generate_audio").and_then(Value::as_bool);
        request.seed = match object.get("seed") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .ok_or_else(|| DomainError::Validation("seed must be an integer".into()))?,
            ),
        };
        request.adapter_options = object
            .get("adapter_options")
            .or_else(|| object.get("provider"))
            .filter(|value| !value.is_null())
            .cloned();

        if let Some(frames) = object.get("frame_images").and_then(Value::as_array) {
            for frame in frames {
                let Some(frame_object) = frame.as_object() else {
                    continue;
                };
                let url = frame_object
                    .get("image_url")
                    .and_then(Value::as_object)
                    .and_then(|image| image.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let frame_type = match frame_object.get("frame_type").and_then(Value::as_str) {
                    None | Some("first_frame") => FrameType::FirstFrame,
                    Some("last_frame") => FrameType::LastFrame,
                    Some(_) => {
                        return Err(DomainError::Validation(
                            "frame_type must be first_frame or last_frame".into(),
                        ));
                    }
                };
                request.frame_images.push(FrameImage::new(url, frame_type)?);
            }
        }
        if let Some(references) = object.get("input_references").and_then(Value::as_array) {
            for reference in references {
                let Some(reference_object) = reference.as_object() else {
                    continue;
                };
                let url = reference_object
                    .get("image_url")
                    .and_then(Value::as_object)
                    .and_then(|image| image.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                request.input_references.push(InputReference::new(url)?);
            }
        }
        request.validate()?;
        Ok(request)
    }
}

impl Serialize for VideoRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_payload()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for VideoRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_payload(&value).map_err(serde::de::Error::custom)
    }
}

fn string_option(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(ToOwned::to_owned)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| !item.is_null())
                .map(|item| {
                    item.as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| item.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn integers(value: Option<&Value>) -> Vec<u32> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_u64()
                        .or_else(|| item.as_str().and_then(|value| value.parse().ok()))
                        .and_then(|value| u32::try_from(value).ok())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn decimal(value: &Value) -> Option<Decimal> {
    if value.is_boolean() || value.is_null() {
        return None;
    }
    let text = value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string());
    Decimal::from_str(&text).ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoModel {
    pub provider_id: ProviderId,
    pub id: String,
    pub name: String,
    pub description: String,
    pub canonical_slug: Option<String>,
    pub created: Option<i64>,
    pub supported_resolutions: Vec<String>,
    pub supported_aspect_ratios: Vec<String>,
    pub supported_sizes: Vec<String>,
    pub supported_durations: Vec<u32>,
    pub supported_frame_images: Vec<String>,
    pub generate_audio: Option<bool>,
    pub seed: Option<bool>,
    pub allowed_passthrough_parameters: Vec<String>,
    pub pricing_skus: BTreeMap<String, Decimal>,
    /// Resolved JSON schema for provider input validation, when available.
    pub input_schema: Option<Value>,
    /// Canonical common-field name to provider input property name.
    pub field_map: BTreeMap<String, String>,
    pub raw: Value,
}

impl VideoModel {
    pub fn from_api(data: &Value) -> Result<Self, DomainError> {
        Self::from_provider_api(ProviderId::openrouter(), data)
    }

    pub fn from_provider_api(provider_id: ProviderId, data: &Value) -> Result<Self, DomainError> {
        let object = data
            .as_object()
            .ok_or_else(|| DomainError::Validation("video model must be a JSON object".into()))?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(if id.is_empty() { "Unknown model" } else { &id })
            .to_owned();
        let mut pricing_skus = BTreeMap::new();
        if let Some(prices) = object.get("pricing_skus").and_then(Value::as_object) {
            for (key, value) in prices {
                if let Some(value) = decimal(value) {
                    pricing_skus.insert(key.clone(), value);
                }
            }
        }
        Ok(Self {
            provider_id,
            id,
            name,
            description: object
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            canonical_slug: string_option(object.get("canonical_slug")),
            created: object.get("created").and_then(Value::as_i64),
            supported_resolutions: strings(object.get("supported_resolutions")),
            supported_aspect_ratios: strings(object.get("supported_aspect_ratios")),
            supported_sizes: strings(object.get("supported_sizes")),
            supported_durations: integers(object.get("supported_durations")),
            supported_frame_images: strings(object.get("supported_frame_images")),
            generate_audio: object.get("generate_audio").and_then(Value::as_bool),
            seed: object.get("seed").and_then(Value::as_bool),
            allowed_passthrough_parameters: strings(object.get("allowed_passthrough_parameters")),
            pricing_skus,
            input_schema: object.get("input_schema").cloned(),
            field_map: object
                .get("field_map")
                .and_then(Value::as_object)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            raw: data.clone(),
        })
    }

    pub fn supports_request(&self, request: &VideoRequest) -> Vec<String> {
        let mut problems = Vec::new();
        if request.model != self.id {
            problems.push(format!(
                "request model is {}, expected {}",
                request.model, self.id
            ));
        }
        if let Some(value) = &request.resolution
            && !self.supported_resolutions.is_empty()
            && !self.supported_resolutions.contains(value)
        {
            problems.push(format!("resolution {value} is not supported"));
        }
        if let Some(value) = &request.aspect_ratio
            && !self.supported_aspect_ratios.is_empty()
            && !self.supported_aspect_ratios.contains(value)
        {
            problems.push(format!("aspect ratio {value} is not supported"));
        }
        if let Some(value) = &request.size
            && !self.supported_sizes.is_empty()
            && !self.supported_sizes.contains(value)
        {
            problems.push(format!("size {value} is not supported"));
        }
        if let Some(value) = request.duration
            && !self.supported_durations.is_empty()
            && !self.supported_durations.contains(&value)
        {
            problems.push(format!("duration {value}s is not supported"));
        }
        if request.generate_audio == Some(true) && self.generate_audio == Some(false) {
            problems.push("audio generation is not supported".into());
        }
        if request.seed.is_some() && self.seed == Some(false) {
            problems.push("seeded generation is not supported".into());
        }
        for frame in &request.frame_images {
            let frame_type = frame.frame_type.as_str().to_owned();
            if !self.supported_frame_images.is_empty()
                && !self.supported_frame_images.contains(&frame_type)
            {
                problems.push(format!("{frame_type} is not supported"));
            }
        }
        problems
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoCatalog {
    pub provider_id: ProviderId,
    pub models: Vec<VideoModel>,
    pub fetched_at: DateTime<Utc>,
    pub stale: bool,
}

impl VideoCatalog {
    pub fn from_api(data: &Value) -> Result<Self, DomainError> {
        Self::from_api_at(data, Utc::now(), false)
    }

    pub fn from_api_at(
        data: &Value,
        fetched_at: DateTime<Utc>,
        stale: bool,
    ) -> Result<Self, DomainError> {
        let values = data
            .as_object()
            .and_then(|object| object.get("data"))
            .and_then(Value::as_array)
            .ok_or(DomainError::InvalidCatalog)?;
        let provider_id = data
            .get("provider_id")
            .and_then(Value::as_str)
            .map(ProviderId::new)
            .transpose()?
            .unwrap_or_else(ProviderId::openrouter);
        let models = values
            .iter()
            .filter(|value| value.is_object())
            .map(|value| VideoModel::from_provider_api(provider_id.clone(), value))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            provider_id,
            models,
            fetched_at,
            stale,
        })
    }

    pub fn new(provider_id: ProviderId, models: Vec<VideoModel>, stale: bool) -> Self {
        Self {
            provider_id,
            models,
            fetched_at: Utc::now(),
            stale,
        }
    }

    pub fn find(&self, model_id: &str) -> Option<&VideoModel> {
        self.models.iter().find(|model| model.id == model_id)
    }

    pub fn preferred(&self) -> Option<&VideoModel> {
        self.find(PREFERRED_MODEL_ID)
            .or_else(|| {
                self.models
                    .iter()
                    .find(|model| model.id.to_ascii_lowercase().contains("flux"))
            })
            .or_else(|| self.models.first())
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), DomainError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = json!({
            "provider_id": self.provider_id,
            "fetched_at": self.fetched_at.to_rfc3339(),
            "data": self.models.iter().map(|model| &model.raw).collect::<Vec<_>>(),
        });
        let temporary = path.with_file_name(format!(
            "{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("catalog")
        ));
        fs::write(&temporary, serde_json::to_vec(&payload)?)?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, DomainError> {
        let path = path.as_ref();
        let payload: Value = serde_json::from_slice(&fs::read(path)?)?;
        let fetched_at = payload
            .get("fetched_at")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .or_else(|| {
                fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .map(DateTime::<Utc>::from)
            })
            .unwrap_or_else(Utc::now);
        Self::from_api_at(&payload, fetched_at, true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    Expired,
    Unknown(String),
}

impl JobStatus {
    pub fn from_raw(value: impl Into<String>) -> Self {
        let value = value.into().to_ascii_lowercase();
        match value.as_str() {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "expired" => Self::Expired,
            _ => Self::Unknown(value),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Unknown(value) => value,
        }
    }

    pub const fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Expired
        )
    }
}

impl Serialize for JobStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for JobStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_raw(String::deserialize(deserializer)?))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoJob {
    pub provider_id: ProviderId,
    pub id: String,
    pub status: JobStatus,
    pub polling_url: String,
    pub generation_id: Option<String>,
    pub unsigned_urls: Vec<String>,
    pub usage: Map<String, Value>,
    pub error: Option<String>,
    pub locator: JobLocator,
    pub artifacts: Vec<VideoArtifact>,
    pub raw: Value,
}

impl VideoJob {
    pub fn from_api(data: &Value) -> Result<Self, DomainError> {
        Self::from_openrouter_api(data)
    }

    pub fn from_openrouter_api(data: &Value) -> Result<Self, DomainError> {
        let object = data.as_object().ok_or_else(|| {
            DomainError::Validation("video job response must be a JSON object".into())
        })?;
        let error = object.get("error").and_then(|value| match value {
            Value::Object(error) => error
                .get("message")
                .or_else(|| error.get("code"))
                .map(value_to_string)
                .or_else(|| Some("Unknown error".into())),
            Value::Null => None,
            value => Some(value_to_string(value)),
        });
        let id = object.get("id").map(value_to_string).unwrap_or_default();
        let polling_url = object
            .get("polling_url")
            .map(value_to_string)
            .unwrap_or_default();
        let unsigned_urls = strings(object.get("unsigned_urls"));
        let artifacts = unsigned_urls
            .iter()
            .enumerate()
            .filter_map(|(index, url)| VideoArtifact::new(url, index).ok())
            .collect();
        Ok(Self {
            provider_id: ProviderId::openrouter(),
            id,
            status: JobStatus::from_raw(
                object
                    .get("status")
                    .map(value_to_string)
                    .unwrap_or_else(|| "unknown".into()),
            ),
            polling_url: polling_url.clone(),
            generation_id: object.get("generation_id").and_then(nonempty_string),
            unsigned_urls,
            usage: object
                .get("usage")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            error,
            locator: JobLocator::OpenRouter { polling_url },
            artifacts,
            raw: data.clone(),
        })
    }

    pub fn key(&self) -> ProviderJobKey {
        ProviderJobKey {
            provider_id: self.provider_id.clone(),
            remote_job_id: self.id.clone(),
        }
    }

    pub fn terminal(&self) -> bool {
        self.status.terminal()
    }

    pub fn successful(&self) -> bool {
        self.status == JobStatus::Completed
    }

    pub fn cost(&self) -> Option<Decimal> {
        self.usage.get("cost").and_then(decimal)
    }
}

fn nonempty_string(value: &Value) -> Option<String> {
    if value.is_null() {
        None
    } else {
        let value = value_to_string(value);
        (!value.is_empty()).then_some(value)
    }
}

fn value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostEstimate {
    pub amount: Option<Decimal>,
    pub basis: String,
    pub exact: bool,
    pub pricing_sku: Option<String>,
    pub unit_price: Option<Decimal>,
    pub currency: String,
    pub raw_pricing: BTreeMap<String, Decimal>,
    pub confidence: QuoteConfidence,
}

pub type CostQuote = CostEstimate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteConfidence {
    Exact,
    Estimated,
    Unknown,
}

impl CostEstimate {
    pub fn available(&self) -> bool {
        self.amount.is_some()
    }
}

pub fn estimate_cost(model: &VideoModel, request: &VideoRequest) -> CostEstimate {
    let pricing = &model.pricing_skus;
    if pricing.is_empty() {
        return unavailable("No pricing advertised", pricing);
    }
    let resolution = request
        .resolution
        .as_ref()
        .map(|value| value.to_ascii_lowercase());
    let audio = request.generate_audio.or(model.generate_audio);

    let variants = |stem: &str| {
        let mut values = Vec::new();
        let audio_label = match audio {
            Some(true) => Some("with_audio"),
            Some(false) => Some("without_audio"),
            None => None,
        };
        if let (Some(resolution), Some(audio_label)) = (&resolution, audio_label) {
            values.push(format!("{stem}_{resolution}_{audio_label}"));
            values.push(format!("{stem}_{audio_label}_{resolution}"));
        }
        if let Some(audio_label) = audio_label {
            values.push(format!("{stem}_{audio_label}"));
        }
        if let Some(resolution) = &resolution {
            values.push(format!("{stem}_{resolution}"));
            values.push(format!("{stem}-{resolution}"));
        }
        values.push(stem.to_owned());
        values
    };

    if let Some(duration) = request.duration {
        for sku in variants("cents_per_second_output") {
            if let Some(cents) = pricing.get(&sku) {
                let unit = *cents / Decimal::from(100u32);
                return CostEstimate {
                    amount: Some(unit * Decimal::from(duration)),
                    basis: format!("{duration}s × {cents}¢/video-second"),
                    exact: true,
                    pricing_sku: Some(sku),
                    unit_price: Some(unit),
                    currency: "USD".into(),
                    raw_pricing: pricing.clone(),
                    confidence: QuoteConfidence::Exact,
                };
            }
        }

        let mut stems = Vec::new();
        if request.frame_images.is_empty() && request.input_references.is_empty() {
            stems.push("text_to_video_duration_seconds");
        }
        stems.extend([
            "duration_seconds",
            "per-video-second",
            "per_video_second",
            "per_second",
        ]);
        for stem in stems {
            for sku in variants(stem) {
                if let Some(unit) = pricing.get(&sku) {
                    return CostEstimate {
                        amount: Some(*unit * Decimal::from(duration)),
                        basis: format!("{duration}s × ${unit}/video-second"),
                        exact: true,
                        pricing_sku: Some(sku),
                        unit_price: Some(*unit),
                        currency: "USD".into(),
                        raw_pricing: pricing.clone(),
                        confidence: QuoteConfidence::Exact,
                    };
                }
            }
        }
    }

    let fixed = ["generate", "per-video", "per_generation"]
        .into_iter()
        .filter(|key| pricing.contains_key(*key))
        .collect::<Vec<_>>();
    if pricing.len() == 1 && fixed.len() == 1 {
        let sku = fixed[0];
        let amount = pricing[sku];
        return CostEstimate {
            amount: Some(amount),
            basis: format!("Advertised fixed generation price: ${amount}"),
            exact: true,
            pricing_sku: Some(sku.into()),
            unit_price: Some(amount),
            currency: "USD".into(),
            raw_pricing: pricing.clone(),
            confidence: QuoteConfidence::Exact,
        };
    }

    unavailable(
        "Pricing SKUs use provider-specific units; inspect raw pricing before submitting",
        pricing,
    )
}

fn unavailable(reason: &str, pricing: &BTreeMap<String, Decimal>) -> CostEstimate {
    CostEstimate {
        amount: None,
        basis: reason.into(),
        exact: false,
        pricing_sku: None,
        unit_price: None,
        currency: "USD".into(),
        raw_pricing: pricing.clone(),
        confidence: QuoteConfidence::Unknown,
    }
}
