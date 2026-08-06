//! Provider-neutral video requests, catalogs, jobs, and cost quotes.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use thiserror::Error;
use url::{Host, Url};

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

pub(crate) fn validate_public_https_url(value: &str, label: &str) -> Result<(), DomainError> {
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
    if url.host().is_some_and(host_is_non_public) {
        return Err(DomainError::Validation(format!(
            "{label} must use a public host"
        )));
    }
    Ok(())
}

fn host_is_non_public(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            matches!(domain.as_str(), "localhost" | "local")
                || domain.ends_with(".localhost")
                || domain.ends_with(".local")
        }
        Host::Ipv4(address) => ipv4_is_non_global(address),
        Host::Ipv6(address) => ipv6_is_non_global(address),
    }
}

fn ipv4_is_non_global(address: Ipv4Addr) -> bool {
    // Explicit IANA special-purpose ranges keep this check stable across Rust
    // releases and fail closed for literal-IP SSRF targets.
    [
        ([0, 0, 0, 0], 8),       // current network / unspecified
        ([10, 0, 0, 0], 8),      // private
        ([100, 64, 0, 0], 10),   // shared address space (CGNAT)
        ([127, 0, 0, 0], 8),     // loopback
        ([169, 254, 0, 0], 16),  // link-local
        ([172, 16, 0, 0], 12),   // private
        ([192, 0, 0, 0], 24),    // IETF protocol assignments
        ([192, 0, 2, 0], 24),    // TEST-NET-1
        ([192, 88, 99, 0], 24),  // deprecated 6to4 relay anycast
        ([192, 168, 0, 0], 16),  // private
        ([198, 18, 0, 0], 15),   // benchmarking
        ([198, 51, 100, 0], 24), // TEST-NET-2
        ([203, 0, 113, 0], 24),  // TEST-NET-3
        ([224, 0, 0, 0], 4),     // multicast
        ([240, 0, 0, 0], 4),     // reserved / limited broadcast
    ]
    .into_iter()
    .any(|(network, prefix)| ipv4_in_prefix(address, network, prefix))
}

fn ipv4_in_prefix(address: Ipv4Addr, network: [u8; 4], prefix: u32) -> bool {
    let mask = u32::MAX << (32 - prefix);
    u32::from_be_bytes(address.octets()) & mask == u32::from_be_bytes(network) & mask
}

fn ipv6_is_non_global(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    let mapped_ipv4 = octets[..10] == [0; 10] && octets[10..12] == [0xff, 0xff];
    if mapped_ipv4 {
        return ipv4_is_non_global(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    // Deprecated IPv4-compatible literals are never treated as public IPv6,
    // even when their final four bytes spell a globally routed IPv4 address.
    if octets[..12] == [0; 12] {
        return true;
    }
    // IANA currently allocates ordinary IPv6 global unicast from 2000::/3.
    // Treat all other literal space as reserved unless it was the explicitly
    // handled IPv4-mapped form above.
    if !ipv6_in_prefix(
        address,
        [0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        3,
    ) {
        return true;
    }
    [
        ([0x20, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 23),
        (
            [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            32,
        ),
        ([0x20, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 16),
        ([0x3f, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 20),
    ]
    .into_iter()
    .any(|(network, prefix)| ipv6_in_prefix(address, network, prefix))
}

fn ipv6_in_prefix(address: Ipv6Addr, network: [u8; 16], prefix: u32) -> bool {
    let mask = u128::MAX << (128 - prefix);
    u128::from_be_bytes(address.octets()) & mask == u128::from_be_bytes(network) & mask
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
    pub kind: InputReferenceKind,
}

impl InputReference {
    /// Construct the legacy image reference form. Keeping this constructor
    /// image-specific preserves the v0.4 request wire format and persisted
    /// request fingerprints.
    pub fn new(url: impl Into<String>) -> Result<Self, DomainError> {
        Self::with_kind(url, InputReferenceKind::Image)
    }

    pub fn with_kind(
        url: impl Into<String>,
        kind: impl Into<InputReferenceKind>,
    ) -> Result<Self, DomainError> {
        let value = Self {
            url: url.into(),
            kind: kind.into(),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        validate_public_https_url(&self.url, "Input reference")
    }

    pub fn to_payload(&self) -> Value {
        let wire_name = self.kind.wire_name();
        let mut payload = Map::new();
        payload.insert("type".into(), Value::String(wire_name.into()));
        payload.insert(wire_name.into(), json!({"url": self.url}));
        Value::Object(payload)
    }

    pub fn from_payload(payload: &Value) -> Result<Self, DomainError> {
        let object = payload.as_object().ok_or_else(|| {
            DomainError::Validation("input reference must be a JSON object".into())
        })?;
        let media_fields = [
            ("image_url", InputReferenceKind::Image),
            ("video_url", InputReferenceKind::Video),
            ("audio_url", InputReferenceKind::Audio),
        ]
        .into_iter()
        .filter(|(name, _)| object.contains_key(*name))
        .collect::<Vec<_>>();
        let [(_, field_kind)] = media_fields.as_slice() else {
            return Err(DomainError::Validation(
                "input reference must contain exactly one image_url, video_url, or audio_url"
                    .into(),
            ));
        };
        let kind = match object.get("type") {
            Some(Value::String(value)) => match value.as_str() {
                "image_url" => InputReferenceKind::Image,
                "video_url" => InputReferenceKind::Video,
                "audio_url" => InputReferenceKind::Audio,
                _ => {
                    return Err(DomainError::Validation(
                        "input reference type must be image_url, video_url, or audio_url".into(),
                    ));
                }
            },
            // Older persisted requests omitted the redundant discriminator.
            None => *field_kind,
            Some(_) => {
                return Err(DomainError::Validation(
                    "input reference type must be a string".into(),
                ));
            }
        };
        if kind != *field_kind {
            return Err(DomainError::Validation(
                "input reference type must match its media URL field".into(),
            ));
        }
        let url = object
            .get(kind.wire_name())
            .and_then(Value::as_object)
            .and_then(|reference| reference.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        Self::with_kind(url, kind)
    }
}

impl Serialize for InputReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.to_payload().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InputReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_payload(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    #[default]
    Image,
    Video,
    Audio,
}

impl MediaKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum InputReferenceKind {
    #[default]
    Image,
    Video,
    Audio,
}

impl InputReferenceKind {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Image => "image_url",
            Self::Video => "video_url",
            Self::Audio => "audio_url",
        }
    }
}

impl From<MediaKind> for InputReferenceKind {
    fn from(kind: MediaKind) -> Self {
        match kind {
            MediaKind::Image => Self::Image,
            MediaKind::Video => Self::Video,
            MediaKind::Audio => Self::Audio,
        }
    }
}

impl From<InputReferenceKind> for MediaKind {
    fn from(kind: InputReferenceKind) -> Self {
        match kind {
            InputReferenceKind::Image => Self::Image,
            InputReferenceKind::Video => Self::Video,
            InputReferenceKind::Audio => Self::Audio,
        }
    }
}

/// A reference asset as selected in the GUI before any provider-specific
/// upload has taken place. Local file contents are deliberately never
/// serialized; only their path is persisted with a draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaSource {
    LocalFile { path: PathBuf },
    RemoteUrl { url: String },
}

impl MediaSource {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::LocalFile { path: path.into() }
    }

    pub fn remote(url: impl Into<String>) -> Result<Self, DomainError> {
        let source = Self::RemoteUrl { url: url.into() };
        source.validate()?;
        Ok(source)
    }

    /// Validate a source as a legacy image reference.
    pub fn validate(&self) -> Result<(), DomainError> {
        self.validate_for_kind(MediaKind::Image)
    }

    pub fn validate_for_kind(&self, kind: MediaKind) -> Result<(), DomainError> {
        match self {
            Self::LocalFile { path } => {
                if path.as_os_str().is_empty() {
                    return Err(DomainError::Validation(
                        "Local media path is required".into(),
                    ));
                }
                if !path.is_absolute() {
                    return Err(DomainError::Validation(
                        "Local media path must be absolute".into(),
                    ));
                }
                let metadata = fs::metadata(path).map_err(|error| {
                    DomainError::Validation(format!(
                        "Local media file {} is unavailable: {error}",
                        path.display()
                    ))
                })?;
                if !metadata.is_file() {
                    return Err(DomainError::Validation(format!(
                        "Local media path {} is not a regular file",
                        path.display()
                    )));
                }
                if metadata.len() == 0 {
                    return Err(DomainError::Validation(format!(
                        "Local media file {} is empty",
                        path.display()
                    )));
                }
                validate_local_reference(path, kind)?;
            }
            Self::RemoteUrl { url } => validate_public_https_url(url, "Reference media")?,
        }
        Ok(())
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            Self::LocalFile { path } => Some(path),
            Self::RemoteUrl { .. } => None,
        }
    }

    pub fn remote_url(&self) -> Option<&str> {
        match self {
            Self::LocalFile { .. } => None,
            Self::RemoteUrl { url } => Some(url),
        }
    }
}

fn validate_local_reference(path: &Path, kind: MediaKind) -> Result<(), DomainError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            DomainError::Validation(format!(
                "Local {} references must use a supported extension",
                kind.as_str()
            ))
        })?;
    let extension_supported = match kind {
        MediaKind::Image => matches!(
            extension.as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "avif" | "bmp" | "tif" | "tiff"
        ),
        MediaKind::Video => matches!(extension.as_str(), "mp4" | "mov"),
        MediaKind::Audio => matches!(extension.as_str(), "mp3" | "wav"),
    };
    if !extension_supported {
        return Err(DomainError::Validation(format!(
            "Local reference {} is not a supported {}",
            path.display(),
            kind.as_str()
        )));
    }
    let mut file = fs::File::open(path).map_err(|error| {
        DomainError::Validation(format!(
            "Local media file {} is unavailable: {error}",
            path.display()
        ))
    })?;
    let mut header = [0u8; 16];
    let read = file.read(&mut header).map_err(|error| {
        DomainError::Validation(format!(
            "Could not inspect local media file {}: {error}",
            path.display()
        ))
    })?;
    let bytes = &header[..read];
    let signature_matches = match kind {
        MediaKind::Image => match extension.as_str() {
            "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "jpg" | "jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
            "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            "webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
            "bmp" => bytes.starts_with(b"BM"),
            "tif" | "tiff" => bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"),
            "avif" => {
                bytes.len() >= 12
                    && &bytes[4..8] == b"ftyp"
                    && matches!(&bytes[8..12], b"avif" | b"avis" | b"mif1" | b"miaf")
            }
            _ => false,
        },
        MediaKind::Video => {
            let brand = (bytes.len() >= 12 && &bytes[4..8] == b"ftyp").then(|| &bytes[8..12]);
            match extension.as_str() {
                "mp4" => brand.is_some_and(is_mp4_brand),
                "mov" => brand == Some(b"qt  ".as_slice()),
                _ => false,
            }
        }
        MediaKind::Audio => match extension.as_str() {
            "mp3" => {
                bytes.starts_with(b"ID3")
                    || (bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
            }
            "wav" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE",
            _ => false,
        },
    };
    if !signature_matches {
        return Err(DomainError::Validation(format!(
            "Local reference {} does not match its {} format",
            path.display(),
            kind.as_str()
        )));
    }
    Ok(())
}

fn is_mp4_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"avc1" | b"dash" | b"M4V " | b"M4VH" | b"M4VP" | b"MSNV"
    ) || brand.starts_with(b"iso")
        || brand.starts_with(b"mp4")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaRole {
    StartFrame,
    EndFrame,
    Reference,
    VideoInput,
    AudioInput,
}

impl MediaRole {
    pub const fn frame_type(self) -> Option<FrameType> {
        match self {
            Self::StartFrame => Some(FrameType::FirstFrame),
            Self::EndFrame => Some(FrameType::LastFrame),
            Self::Reference | Self::VideoInput | Self::AudioInput => None,
        }
    }

    pub const fn kind(self) -> MediaKind {
        match self {
            Self::StartFrame | Self::EndFrame | Self::Reference => MediaKind::Image,
            Self::VideoInput => MediaKind::Video,
            Self::AudioInput => MediaKind::Audio,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftMedia {
    pub source: MediaSource,
    pub role: MediaRole,
}

impl DraftMedia {
    pub fn local(path: impl Into<PathBuf>, role: MediaRole) -> Self {
        Self {
            source: MediaSource::local(path),
            role,
        }
    }

    pub fn remote(url: impl Into<String>, role: MediaRole) -> Result<Self, DomainError> {
        Ok(Self {
            source: MediaSource::remote(url)?,
            role,
        })
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        self.source.validate_for_kind(self.role.kind())
    }
}

/// A provider upload that can be reused while both its content digest and
/// expiration remain valid. The source path is intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadReceipt {
    pub provider_id: ProviderId,
    pub sha256: String,
    pub public_url: String,
    pub uploaded_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub size_bytes: u64,
}

impl UploadReceipt {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider_id: ProviderId,
        sha256: impl Into<String>,
        public_url: impl Into<String>,
        uploaded_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        content_type: Option<String>,
        size_bytes: u64,
    ) -> Result<Self, DomainError> {
        let receipt = Self {
            provider_id,
            sha256: sha256.into(),
            public_url: public_url.into(),
            uploaded_at,
            expires_at,
            content_type,
            size_bytes,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(DomainError::Validation(
                "Upload receipt SHA-256 digest is invalid".into(),
            ));
        }
        validate_public_https_url(&self.public_url, "Uploaded media")?;
        if self.expires_at <= self.uploaded_at {
            return Err(DomainError::Validation(
                "Upload receipt expiration must follow upload time".into(),
            ));
        }
        if self.size_bytes == 0 {
            return Err(DomainError::Validation(
                "Upload receipt cannot describe an empty file".into(),
            ));
        }
        if self
            .content_type
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.chars().any(char::is_control))
        {
            return Err(DomainError::Validation(
                "Upload receipt content type is invalid".into(),
            ));
        }
        Ok(())
    }

    pub fn reusable_for(&self, provider_id: &ProviderId, sha256: &str, now: DateTime<Utc>) -> bool {
        self.validate().is_ok()
            && &self.provider_id == provider_id
            && self.sha256 == sha256
            && self.expires_at > now
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedMedia {
    pub role: MediaRole,
    pub public_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<UploadReceipt>,
}

impl StagedMedia {
    pub fn remote(role: MediaRole, url: impl Into<String>) -> Result<Self, DomainError> {
        let media = Self {
            role,
            public_url: url.into(),
            receipt: None,
        };
        media.validate()?;
        Ok(media)
    }

    pub fn uploaded(role: MediaRole, receipt: UploadReceipt) -> Result<Self, DomainError> {
        receipt.validate()?;
        let media = Self {
            role,
            public_url: receipt.public_url.clone(),
            receipt: Some(receipt),
        };
        media.validate()?;
        Ok(media)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        validate_public_https_url(&self.public_url, "Staged media")?;
        if let Some(receipt) = &self.receipt {
            receipt.validate()?;
            if receipt.public_url != self.public_url {
                return Err(DomainError::Validation(
                    "Staged media URL does not match its upload receipt".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Editable, autosave-friendly generation input. It deliberately separates
/// local reference paths from the URL-only `VideoRequest` sent to providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationDraft {
    pub provider_id: ProviderId,
    pub model: String,
    pub prompt: String,
    pub duration: Option<u32>,
    pub resolution: Option<String>,
    pub aspect_ratio: Option<String>,
    pub size: Option<String>,
    pub generate_audio: Option<bool>,
    pub seed: Option<i64>,
    #[serde(default)]
    pub media: Vec<DraftMedia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_options: Option<Value>,
}

impl GenerationDraft {
    pub fn new(
        provider_id: ProviderId,
        model: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<Self, DomainError> {
        let draft = Self {
            provider_id,
            model: model.into(),
            prompt: prompt.into(),
            duration: None,
            resolution: None,
            aspect_ratio: None,
            size: None,
            generate_audio: None,
            seed: None,
            media: Vec::new(),
            adapter_options: None,
        };
        // A newly opened composer may have an empty prompt/model. Full
        // validation happens at Review, matching the GUI workflow.
        Ok(draft)
    }

    pub fn validate(&self) -> Result<(), DomainError> {
        let request = self.request_without_media()?;
        request.validate()?;
        let mut start_frames = 0usize;
        let mut end_frames = 0usize;
        for media in &self.media {
            media.validate()?;
            match media.role {
                MediaRole::StartFrame => start_frames += 1,
                MediaRole::EndFrame => end_frames += 1,
                MediaRole::Reference | MediaRole::VideoInput | MediaRole::AudioInput => {}
            }
        }
        if start_frames > 1 || end_frames > 1 {
            return Err(DomainError::Validation(
                "A draft can contain at most one start frame and one end frame".into(),
            ));
        }
        Ok(())
    }

    pub fn to_video_request(&self, staged: &[StagedMedia]) -> Result<VideoRequest, DomainError> {
        self.validate()?;
        if staged.len() != self.media.len() {
            return Err(DomainError::Validation(
                "Every draft media item must be staged before Review".into(),
            ));
        }
        let mut request = self.request_without_media()?;
        for (draft_media, staged_media) in self.media.iter().zip(staged) {
            staged_media.validate()?;
            if draft_media.role != staged_media.role {
                return Err(DomainError::Validation(
                    "Staged media order or role does not match the draft".into(),
                ));
            }
            if let Some(frame_type) = staged_media.role.frame_type() {
                request.frame_images.push(FrameImage::new(
                    staged_media.public_url.clone(),
                    frame_type,
                )?);
            } else {
                request.input_references.push(InputReference::with_kind(
                    staged_media.public_url.clone(),
                    staged_media.role.kind(),
                )?);
            }
        }
        request.validate()?;
        Ok(request)
    }

    fn request_without_media(&self) -> Result<VideoRequest, DomainError> {
        let mut request = VideoRequest::for_provider(
            self.provider_id.clone(),
            self.model.clone(),
            self.prompt.clone(),
        )?;
        request.duration = self.duration;
        request.resolution = self.resolution.clone();
        request.aspect_ratio = self.aspect_ratio.clone();
        request.size = self.size.clone();
        request.generate_audio = self.generate_audio;
        request.seed = self.seed;
        request.adapter_options = self.adapter_options.clone();
        Ok(request)
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
        let mut first_frames = 0usize;
        let mut last_frames = 0usize;
        for frame in &self.frame_images {
            frame.validate()?;
            match frame.frame_type {
                FrameType::FirstFrame => first_frames += 1,
                FrameType::LastFrame => last_frames += 1,
            }
        }
        if first_frames > 1 || last_frames > 1 {
            return Err(DomainError::Validation(
                "A video request can contain at most one first_frame and one last_frame".into(),
            ));
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
        let provider_id = match object.get("provider_id") {
            None | Some(Value::Null) => ProviderId::openrouter(),
            Some(Value::String(value)) => ProviderId::new(value)?,
            Some(_) => {
                return Err(DomainError::Validation(
                    "provider_id must be a string or null".into(),
                ));
            }
        };
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
        request.resolution = optional_string_field(object, "resolution")?;
        request.aspect_ratio = optional_string_field(object, "aspect_ratio")?;
        request.size = optional_string_field(object, "size")?;
        request.generate_audio = optional_bool_field(object, "generate_audio")?;
        request.seed = match object.get("seed") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .ok_or_else(|| DomainError::Validation("seed must be an integer".into()))?,
            ),
        };
        let adapter_options = object
            .get("adapter_options")
            .filter(|value| !value.is_null());
        let provider_options = object.get("provider").filter(|value| !value.is_null());
        request.adapter_options = match (adapter_options, provider_options) {
            (Some(adapter), Some(provider)) if adapter != provider => {
                return Err(DomainError::Validation(
                    "adapter_options and provider cannot specify different values".into(),
                ));
            }
            (Some(value), Some(_)) | (Some(value), None) | (None, Some(value)) => {
                Some(value.clone())
            }
            (None, None) => None,
        };

        match object.get("frame_images") {
            None | Some(Value::Null) => {}
            Some(Value::Array(frames)) => {
                for (index, frame) in frames.iter().enumerate() {
                    let Some(frame_object) = frame.as_object() else {
                        return Err(DomainError::Validation(format!(
                            "frame_images[{index}] must be a JSON object"
                        )));
                    };
                    let frame_media_fields = ["image_url", "video_url", "audio_url"]
                        .into_iter()
                        .filter(|name| frame_object.contains_key(*name))
                        .collect::<Vec<_>>();
                    if frame_media_fields != ["image_url"] {
                        return Err(DomainError::Validation(format!(
                            "frame_images[{index}] must contain exactly one image_url field"
                        )));
                    }
                    match frame_object.get("type") {
                        None => {}
                        Some(Value::String(value)) if value == "image_url" => {}
                        Some(Value::String(_)) => {
                            return Err(DomainError::Validation(
                                "frame image type must be image_url".into(),
                            ));
                        }
                        Some(_) => {
                            return Err(DomainError::Validation(
                                "frame image type must be a string".into(),
                            ));
                        }
                    }
                    let url = frame_object
                        .get("image_url")
                        .and_then(Value::as_object)
                        .and_then(|image| image.get("url"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let frame_type = match frame_object.get("frame_type") {
                        None => FrameType::FirstFrame,
                        Some(Value::String(value)) if value == "first_frame" => {
                            FrameType::FirstFrame
                        }
                        Some(Value::String(value)) if value == "last_frame" => FrameType::LastFrame,
                        Some(Value::String(_)) => {
                            return Err(DomainError::Validation(
                                "frame_type must be first_frame or last_frame".into(),
                            ));
                        }
                        Some(_) => {
                            return Err(DomainError::Validation(
                                "frame_type must be a string".into(),
                            ));
                        }
                    };
                    request.frame_images.push(FrameImage::new(url, frame_type)?);
                }
            }
            Some(_) => {
                return Err(DomainError::Validation(
                    "frame_images must be an array".into(),
                ));
            }
        }
        match object.get("input_references") {
            None | Some(Value::Null) => {}
            Some(Value::Array(references)) => {
                for (index, reference) in references.iter().enumerate() {
                    if !reference.is_object() {
                        return Err(DomainError::Validation(format!(
                            "input_references[{index}] must be a JSON object"
                        )));
                    }
                    request
                        .input_references
                        .push(InputReference::from_payload(reference)?);
                }
            }
            Some(_) => {
                return Err(DomainError::Validation(
                    "input_references must be an array".into(),
                ));
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

fn optional_string_field(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, DomainError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(DomainError::Validation(format!(
            "{key} must be a string or null"
        ))),
    }
}

fn optional_bool_field(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<bool>, DomainError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(DomainError::Validation(format!(
            "{key} must be a boolean or null"
        ))),
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaCardinality {
    #[default]
    Scalar,
    List,
}

/// Describes how a provider-specific model schema binds one media kind to an
/// input property. OpenRouter uses its typed input-reference union directly,
/// while schema-driven adapters such as fal use these bindings to construct
/// the provider input without guessing property names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaBinding {
    pub kind: MediaKind,
    pub property_name: String,
    #[serde(default)]
    pub cardinality: MediaCardinality,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_items: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl MediaBinding {
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.property_name.trim().is_empty()
            || self.property_name.len() > 256
            || self.property_name.chars().any(char::is_control)
        {
            return Err(DomainError::Validation(
                "media binding property name is invalid".into(),
            ));
        }
        if self
            .min_items
            .zip(self.max_items)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err(DomainError::Validation(
                "media binding min_items cannot exceed max_items".into(),
            ));
        }
        for (label, value) in [
            ("title", self.title.as_deref()),
            ("description", self.description.as_deref()),
        ] {
            if value.is_some_and(|value| value.chars().any(char::is_control)) {
                return Err(DomainError::Validation(format!(
                    "media binding {label} is invalid"
                )));
            }
        }
        Ok(())
    }
}

fn media_kind(value: &Value) -> Option<MediaKind> {
    match value.as_str()? {
        "image" => Some(MediaKind::Image),
        "video" => Some(MediaKind::Video),
        "audio" => Some(MediaKind::Audio),
        _ => None,
    }
}

fn input_modalities(object: &Map<String, Value>) -> Option<Vec<MediaKind>> {
    let value = object.get("input_modalities").or_else(|| {
        object
            .get("architecture")
            .and_then(Value::as_object)
            .and_then(|architecture| architecture.get("input_modalities"))
    })?;
    if value.is_null() {
        return None;
    }
    value
        .as_array()
        .map(|values| values.iter().filter_map(media_kind).collect())
}

fn media_bindings(object: &Map<String, Value>) -> Result<Vec<MediaBinding>, DomainError> {
    let Some(value) = object.get("media_bindings") else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let bindings: Vec<MediaBinding> = serde_json::from_value(value.clone()).map_err(|error| {
        DomainError::Validation(format!("model media_bindings are invalid: {error}"))
    })?;
    for binding in &bindings {
        binding.validate()?;
    }
    Ok(bindings)
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
    /// Explicit media accepted by a model. `None` means the catalog did not
    /// advertise this capability; legacy image references remain compatible,
    /// while video and audio fail closed.
    pub input_modalities: Option<Vec<MediaKind>>,
    /// Provider-schema property bindings. An empty list means the provider
    /// uses its native typed reference union or did not advertise bindings.
    pub media_bindings: Vec<MediaBinding>,
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
            input_modalities: input_modalities(object),
            media_bindings: media_bindings(object)?,
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
            if !self.supported_frame_images.contains(&frame_type) {
                problems.push(format!("{frame_type} is not supported"));
            }
        }

        let mut reference_counts = BTreeMap::<MediaKind, usize>::new();
        for reference in &request.input_references {
            *reference_counts
                .entry(MediaKind::from(reference.kind))
                .or_default() += 1;
        }
        for (&kind, &count) in &reference_counts {
            match &self.input_modalities {
                Some(modalities) if !modalities.contains(&kind) => problems.push(format!(
                    "{} input references are not supported",
                    kind.as_str()
                )),
                None if kind != MediaKind::Image => problems.push(format!(
                    "{} input support is not advertised by this model",
                    kind.as_str()
                )),
                Some(_) | None => {}
            }
            if !self.has_reference_transport(kind) {
                problems.push(format!(
                    "{} input has no provider media binding",
                    kind.as_str()
                ));
            }
            if let Some(binding) = self.media_binding(kind) {
                match binding.cardinality {
                    MediaCardinality::Scalar if count > 1 => problems.push(format!(
                        "{} accepts at most one {} input",
                        binding.property_name,
                        kind.as_str()
                    )),
                    MediaCardinality::List => {
                        if binding.min_items.is_some_and(|minimum| count < minimum) {
                            problems.push(format!(
                                "{} requires at least {} {} input item(s)",
                                binding.property_name,
                                binding.min_items.unwrap_or_default(),
                                kind.as_str()
                            ));
                        }
                        if binding.max_items.is_some_and(|maximum| count > maximum) {
                            problems.push(format!(
                                "{} accepts at most {} {} input item(s)",
                                binding.property_name,
                                binding.max_items.unwrap_or_default(),
                                kind.as_str()
                            ));
                        }
                    }
                    MediaCardinality::Scalar => {}
                }
            }
        }
        problems
    }

    pub fn media_binding(&self, kind: MediaKind) -> Option<&MediaBinding> {
        self.media_bindings
            .iter()
            .find(|binding| binding.kind == kind)
    }

    pub fn supports_media_kind(&self, kind: MediaKind) -> bool {
        let modality_supported = match &self.input_modalities {
            Some(modalities) => modalities.contains(&kind),
            None => kind == MediaKind::Image,
        };
        modality_supported && self.has_reference_transport(kind)
    }

    fn has_reference_transport(&self, kind: MediaKind) -> bool {
        if self.provider_id == ProviderId::openrouter() {
            return true;
        }
        if self.media_binding(kind).is_some() {
            return true;
        }
        self.provider_id == ProviderId::fal()
            && kind == MediaKind::Image
            && self.field_map.contains_key("references")
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn typed_input_references_keep_the_legacy_image_wire_shape() {
        let url = "https://media.example/reference.png";
        let image = InputReference::new(url).expect("image reference");
        assert_eq!(image.kind, InputReferenceKind::Image);
        assert_eq!(
            image.to_payload(),
            json!({"type": "image_url", "image_url": {"url": url}})
        );
        assert_eq!(
            serde_json::to_value(&image).expect("serialize image reference"),
            json!({"type": "image_url", "image_url": {"url": url}})
        );

        for (kind, wire_name, extension) in [
            (InputReferenceKind::Video, "video_url", "mp4"),
            (InputReferenceKind::Audio, "audio_url", "wav"),
        ] {
            let url = format!("https://media.example/reference.{extension}");
            let reference = InputReference::with_kind(&url, kind).expect("typed reference");
            assert_eq!(
                reference.to_payload(),
                json!({"type": wire_name, wire_name: {"url": url}})
            );
            assert_eq!(
                InputReference::from_payload(&reference.to_payload())
                    .expect("round-trip typed reference"),
                reference
            );
        }
    }

    #[test]
    fn request_payload_rejects_malformed_media_arrays_and_discriminators() {
        let base = json!({"model": "example/video", "prompt": "test"});
        for (field, invalid) in [
            ("frame_images", json!({})),
            ("frame_images", json!("not-an-array")),
            ("input_references", json!({})),
            ("input_references", json!(false)),
        ] {
            let mut payload = base.clone();
            payload[field] = invalid;
            assert!(
                VideoRequest::from_payload(&payload).is_err(),
                "accepted malformed {field}: {payload}"
            );
        }

        for (field, invalid_item) in [
            ("frame_images", json!(null)),
            ("frame_images", json!(7)),
            ("input_references", json!(null)),
            ("input_references", json!("reference")),
        ] {
            let mut payload = base.clone();
            payload[field] = json!([invalid_item]);
            assert!(
                VideoRequest::from_payload(&payload).is_err(),
                "silently dropped malformed {field} element: {payload}"
            );
        }

        let mut frame_type_confusion = base.clone();
        frame_type_confusion["frame_images"] = json!([{
            "type": "image_url",
            "image_url": {"url": "https://media.example/start.png"},
            "frame_type": false
        }]);
        assert!(VideoRequest::from_payload(&frame_type_confusion).is_err());

        let mut reference_type_confusion = base.clone();
        reference_type_confusion["input_references"] = json!([{
            "type": 7,
            "image_url": {"url": "https://media.example/reference.png"}
        }]);
        assert!(VideoRequest::from_payload(&reference_type_confusion).is_err());

        for reference in [
            json!({
                "type": "image_url",
                "video_url": {"url": "https://media.example/reference.mp4"}
            }),
            json!({
                "type": "video_url",
                "image_url": {"url": "https://media.example/reference.png"},
                "video_url": {"url": "https://media.example/reference.mp4"}
            }),
            json!({
                "image_url": {"url": "https://media.example/reference.png"},
                "audio_url": {"url": "https://media.example/reference.wav"}
            }),
        ] {
            let mut payload = base.clone();
            payload["input_references"] = json!([reference]);
            assert!(VideoRequest::from_payload(&payload).is_err());
        }

        for frame in [
            json!({
                "type": "video_url",
                "image_url": {"url": "https://media.example/start.png"},
                "frame_type": "first_frame"
            }),
            json!({
                "type": "image_url",
                "image_url": {"url": "https://media.example/start.png"},
                "video_url": {"url": "https://media.example/start.mp4"},
                "frame_type": "first_frame"
            }),
        ] {
            let mut payload = base.clone();
            payload["frame_images"] = json!([frame]);
            assert!(VideoRequest::from_payload(&payload).is_err());
        }

        let mut partial_references = base.clone();
        partial_references["input_references"] = json!([
            {
                "type": "image_url",
                "image_url": {"url": "https://media.example/reference.png"}
            },
            7
        ]);
        assert!(
            VideoRequest::from_payload(&partial_references).is_err(),
            "a valid prefix must not hide a malformed later media item"
        );

        let mut nullable = base;
        nullable["provider_id"] = Value::Null;
        nullable["resolution"] = Value::Null;
        nullable["aspect_ratio"] = Value::Null;
        nullable["size"] = Value::Null;
        nullable["generate_audio"] = Value::Null;
        nullable["frame_images"] = Value::Null;
        nullable["input_references"] = Value::Null;
        let parsed = VideoRequest::from_payload(&nullable).expect("nullable legacy request");
        assert_eq!(parsed.provider_id, ProviderId::openrouter());
        assert!(parsed.frame_images.is_empty());
        assert!(parsed.input_references.is_empty());

        let mut missing_frame_type = nullable;
        missing_frame_type["frame_images"] = json!([{
            "type": "image_url",
            "image_url": {"url": "https://media.example/start.png"}
        }]);
        assert_eq!(
            VideoRequest::from_payload(&missing_frame_type)
                .expect("legacy frame without discriminator")
                .frame_images[0]
                .frame_type,
            FrameType::FirstFrame
        );
    }

    #[test]
    fn request_payload_rejects_wrong_typed_optional_controls() {
        let base = json!({"model": "example/video", "prompt": "test"});
        for (field, invalid) in [
            ("provider_id", json!(false)),
            ("resolution", json!(720)),
            ("aspect_ratio", json!(["16:9"])),
            ("size", json!({"width": 1280, "height": 720})),
            ("generate_audio", json!("true")),
        ] {
            let mut payload = base.clone();
            payload[field] = invalid;
            assert!(
                VideoRequest::from_payload(&payload).is_err(),
                "silently erased wrong-typed {field}: {payload}"
            );
        }

        let mut provider_fallback = base.clone();
        provider_fallback["adapter_options"] = Value::Null;
        provider_fallback["provider"] = json!({"options": {"route": "fixture"}});
        assert_eq!(
            VideoRequest::from_payload(&provider_fallback)
                .expect("wire provider options")
                .adapter_options,
            Some(json!({"options": {"route": "fixture"}}))
        );

        let mut conflicting = base;
        conflicting["adapter_options"] = json!({"route": "one"});
        conflicting["provider"] = json!({"route": "two"});
        assert!(VideoRequest::from_payload(&conflicting).is_err());
    }

    #[test]
    fn video_request_rejects_duplicate_frame_destinations() {
        let first = || {
            FrameImage::new("https://media.example/first.png", FrameType::FirstFrame)
                .expect("first frame")
        };
        let mut request = VideoRequest::new("example/video", "test").expect("request");
        request.frame_images = vec![first(), first()];
        assert!(request.validate().is_err());
        assert!(request.to_payload().is_err());

        let duplicate_payload = json!({
            "model": "example/video",
            "prompt": "test",
            "frame_images": [first().to_payload(), first().to_payload()]
        });
        assert!(VideoRequest::from_payload(&duplicate_payload).is_err());

        request.frame_images = vec![
            first(),
            FrameImage::new("https://media.example/last.png", FrameType::LastFrame)
                .expect("last frame"),
        ];
        request.validate().expect("one frame per destination");
    }

    #[test]
    fn local_media_validation_uses_the_selected_role_kind() {
        let directory = tempdir().expect("media directory");
        let image = directory.path().join("reference.png");
        let video = directory.path().join("reference.mp4");
        let movie = directory.path().join("reference.mov");
        let audio = directory.path().join("reference.mp3");
        let wave = directory.path().join("reference.wav");
        fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").expect("image fixture");
        fs::write(&video, b"\0\0\0\x18ftypmp42fixture").expect("video fixture");
        fs::write(&movie, b"\0\0\0\x14ftypqt  fixture").expect("mov fixture");
        fs::write(&audio, b"ID3fixture").expect("audio fixture");
        fs::write(&wave, b"RIFF\0\0\0\0WAVEfixture").expect("wave fixture");

        DraftMedia::local(&image, MediaRole::Reference)
            .validate()
            .expect("legacy image");
        DraftMedia::local(&video, MediaRole::VideoInput)
            .validate()
            .expect("MP4 video");
        DraftMedia::local(&movie, MediaRole::VideoInput)
            .validate()
            .expect("MOV video");
        DraftMedia::local(&audio, MediaRole::AudioInput)
            .validate()
            .expect("MP3 audio");
        DraftMedia::local(&wave, MediaRole::AudioInput)
            .validate()
            .expect("WAV audio");
        assert!(
            DraftMedia::local(&video, MediaRole::Reference)
                .validate()
                .is_err(),
            "an MP4 must not be serialized as a legacy image_url"
        );
        assert!(
            DraftMedia::local(&image, MediaRole::VideoInput)
                .validate()
                .is_err(),
            "an image must not be serialized as a video_url"
        );
    }

    #[test]
    fn model_media_capabilities_fail_closed_for_new_reference_kinds() {
        let unknown = VideoModel::from_api(&json!({"id": "example/unknown"}))
            .expect("model without modalities");
        assert!(unknown.supports_media_kind(MediaKind::Image));
        assert!(!unknown.supports_media_kind(MediaKind::Video));

        let mut request = VideoRequest::new("example/unknown", "test").expect("request");
        request.input_references.push(
            InputReference::with_kind(
                "https://media.example/reference.mp4",
                InputReferenceKind::Video,
            )
            .expect("video reference"),
        );
        assert!(
            unknown
                .supports_request(&request)
                .iter()
                .any(|problem| problem.contains("video input support"))
        );

        let capable = VideoModel::from_api(&json!({
            "id": "example/capable",
            "input_modalities": ["text", "video", "audio"],
            "media_bindings": [{
                "kind": "video",
                "property_name": "video_url"
            }]
        }))
        .expect("typed model");
        assert_eq!(
            capable.input_modalities,
            Some(vec![MediaKind::Video, MediaKind::Audio])
        );
        assert!(capable.supports_media_kind(MediaKind::Video));
        assert!(
            capable.supports_media_kind(MediaKind::Audio),
            "OpenRouter's native union does not require per-property bindings"
        );
        assert_eq!(
            capable.media_bindings[0].cardinality,
            MediaCardinality::Scalar
        );

        request.model = capable.id.clone();
        assert!(capable.supports_request(&request).is_empty());

        let fal_frame_only = VideoModel::from_provider_api(
            ProviderId::fal(),
            &json!({
                "id": "fal/frame-only",
                "input_modalities": ["image", "video"],
                "supported_frame_images": ["first_frame"],
                "field_map": {"first_frame": "image_url"}
            }),
        )
        .expect("fal frame-only model");
        assert!(!fal_frame_only.supports_media_kind(MediaKind::Image));
        assert!(!fal_frame_only.supports_media_kind(MediaKind::Video));

        let fal_legacy_references = VideoModel::from_provider_api(
            ProviderId::fal(),
            &json!({
                "id": "fal/legacy-references",
                "field_map": {"references": "reference_images"}
            }),
        )
        .expect("legacy fal model");
        assert!(fal_legacy_references.supports_media_kind(MediaKind::Image));
    }

    #[test]
    fn public_https_validation_rejects_local_and_non_public_literal_hosts() {
        for url in [
            "https://localhost/reference.png",
            "https://LOCALHOST./reference.png",
            "https://media.localhost/reference.png",
            "https://renderbox.local/reference.png",
            "https://RENDERBOX.LOCAL./reference.png",
            "https://127.0.0.1/reference.png",
            "https://10.0.0.1/reference.png",
            "https://172.16.0.1/reference.png",
            "https://192.168.0.1/reference.png",
            "https://169.254.1.1/reference.png",
            "https://0.0.0.0/reference.png",
            "https://0.1.2.3/reference.png",
            "https://100.64.0.1/reference.png",
            "https://100.127.255.254/reference.png",
            "https://192.0.0.1/reference.png",
            "https://192.0.2.1/reference.png",
            "https://192.88.99.1/reference.png",
            "https://198.18.0.1/reference.png",
            "https://198.19.255.254/reference.png",
            "https://198.51.100.1/reference.png",
            "https://203.0.113.1/reference.png",
            "https://224.0.0.1/reference.png",
            "https://240.0.0.1/reference.png",
            "https://255.255.255.255/reference.png",
            "https://[::1]/reference.png",
            "https://[::]/reference.png",
            "https://[::808:808]/reference.png",
            "https://[fc00::1]/reference.png",
            "https://[fe80::1]/reference.png",
            "https://[fec0::1]/reference.png",
            "https://[ff02::1]/reference.png",
            "https://[::ffff:c0a8:1]/reference.png",
            "https://[64:ff9b::808:808]/reference.png",
            "https://[64:ff9b:1::1]/reference.png",
            "https://[100::1]/reference.png",
            "https://[2001::1]/reference.png",
            "https://[2001:db8::1]/reference.png",
            "https://[2002::1]/reference.png",
            "https://[3fff::1]/reference.png",
            "https://[4000::1]/reference.png",
            "https://[5f00::1]/reference.png",
        ] {
            assert!(
                validate_public_https_url(url, "fixture").is_err(),
                "accepted non-public URL {url}"
            );
        }

        for url in [
            "https://media.example/reference.png?X-Signature=abc123&expires=123",
            "https://8.8.8.8/reference.png",
            "https://100.128.0.1/reference.png",
            "https://[2606:4700:4700::1111]/reference.png",
            "https://[2001:4860:4860::8888]/reference.png",
            "https://[::ffff:808:808]/reference.png",
        ] {
            validate_public_https_url(url, "fixture")
                .unwrap_or_else(|error| panic!("rejected public URL {url}: {error}"));
        }
    }
}
