//! fal.ai queue adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    LOCATION, RETRY_AFTER, USER_AGENT,
};
use reqwest::{Method, StatusCode};
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use url::Url;

use crate::api::{
    DEFAULT_APP_TITLE, DownloadProgress, HttpExecutor, HttpRequest, HttpResponse, ReqwestExecutor,
};
use crate::config::partial_path;
use crate::domain::{
    CostQuote, DraftMedia, JobLocator, JobStatus, MediaBinding, MediaCardinality, MediaKind,
    MediaSource, ProviderDescriptor, ProviderId, QuoteConfidence, StagedMedia, UploadReceipt,
    VideoArtifact, VideoCatalog, VideoJob, VideoModel, VideoRequest, validate_public_https_url,
};

use super::{
    MediaCapabilities, ProviderAccount, ProviderError, ProviderErrorKind, UploadProgress,
    VideoProvider, media_sha256,
};

pub const DEFAULT_PLATFORM_URL: &str = "https://api.fal.ai/v1";
pub const DEFAULT_QUEUE_URL: &str = "https://queue.fal.run";
pub const DEFAULT_STORAGE_URL: &str = "https://rest.fal.ai";
pub const INPUT_UPLOAD_RETENTION_SECONDS: u64 = 24 * 60 * 60;
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MULTIPART_THRESHOLD: u64 = 90 * 1024 * 1024;
const MULTIPART_CHUNK_SIZE: usize = 10 * 1024 * 1024;
const DISCOVERY_CATEGORIES: [&str; 4] = [
    "text-to-video",
    "image-to-video",
    "video-to-video",
    "audio-to-video",
];
const MAX_INPUT_MEDIA: usize = 12;
const MAX_IMAGE_INPUTS: usize = 9;
const MAX_VIDEO_INPUTS: usize = 3;
const MAX_AUDIO_INPUTS: usize = 3;
const SEEDANCE_MAX_IMAGE_BYTES: u64 = 30_000_000;
const SEEDANCE_MAX_VIDEO_BYTES: u64 = 50_000_000;
const SEEDANCE_MAX_AUDIO_BYTES: u64 = 15_000_000;

#[derive(Debug, Clone)]
pub struct FalOptions {
    pub platform_base_url: Url,
    pub queue_base_url: Url,
    pub storage_base_url: Url,
    pub timeout: Duration,
    pub max_retries: usize,
    pub backoff_base: Duration,
}

impl Default for FalOptions {
    fn default() -> Self {
        Self {
            platform_base_url: Url::parse(DEFAULT_PLATFORM_URL).expect("built-in fal URL is valid"),
            queue_base_url: Url::parse(DEFAULT_QUEUE_URL).expect("built-in fal URL is valid"),
            storage_base_url: Url::parse(DEFAULT_STORAGE_URL)
                .expect("built-in fal storage URL is valid"),
            timeout: Duration::from_secs(60),
            max_retries: 3,
            backoff_base: Duration::from_secs(1),
        }
    }
}

/// Binary side of fal's documented two-step CDN upload. Keeping it separate
/// from the JSON executor makes uploads testable without sending file bytes.
#[async_trait]
pub trait FalUploadExecutor: Send + Sync {
    async fn upload(
        &self,
        upload_url: &Url,
        path: &Path,
        content_type: &str,
        size_bytes: u64,
        multipart: bool,
        progress: Option<mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<(), ProviderError>;
}

struct ReqwestFalUploadExecutor {
    client: reqwest::Client,
}

impl ReqwestFalUploadExecutor {
    fn new(timeout: Duration) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| configuration("Could not configure fal CDN uploads"))?;
        Ok(Self { client })
    }

    async fn put_bytes(&self, url: &Url, bytes: Bytes) -> Result<Value, ProviderError> {
        let mut last_error = None;
        for attempt in 0..3 {
            let response = self
                .client
                .put(url.clone())
                .body(bytes.clone())
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    let mut stream = response.bytes_stream();
                    let mut body = Vec::new();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(|_| {
                            ProviderError::new(
                                ProviderId::fal(),
                                ProviderErrorKind::Network,
                                "fal CDN interrupted an upload response",
                            )
                        })?;
                        if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
                            return Err(response_error(
                                "fal CDN upload response exceeded the safe size limit",
                            ));
                        }
                        body.extend_from_slice(&chunk);
                    }
                    return if body.is_empty() {
                        Ok(Value::Null)
                    } else {
                        serde_json::from_slice(&body)
                            .map_err(|_| response_error("fal CDN returned invalid upload JSON"))
                    };
                }
                Ok(response) => {
                    last_error = Some(format!("HTTP {}", response.status().as_u16()));
                    if !response.status().is_server_error()
                        && response.status() != StatusCode::TOO_MANY_REQUESTS
                    {
                        break;
                    }
                }
                Err(_) => last_error = Some("network error".into()),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
            }
        }
        Err(ProviderError::new(
            ProviderId::fal(),
            ProviderErrorKind::Network,
            format!(
                "fal CDN upload failed{}",
                last_error
                    .as_deref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            ),
        ))
    }

    async fn upload_single(
        &self,
        upload_url: &Url,
        path: &Path,
        content_type: &str,
        size_bytes: u64,
        progress: Option<mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<(), ProviderError> {
        for attempt in 0..3 {
            if let Some(progress) = &progress {
                let _ = progress.send(UploadProgress {
                    sent: 0,
                    total: size_bytes,
                });
            }
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|error| validation(format!("Could not open local media: {error}")))?;
            let progress_for_stream = progress.clone();
            let stream = futures_util::stream::try_unfold(
                (file, 0u64, progress_for_stream),
                move |(mut file, mut sent, progress)| async move {
                    let mut buffer = vec![0u8; 1024 * 1024];
                    let read = file.read(&mut buffer).await?;
                    if read == 0 {
                        return Ok::<_, std::io::Error>(None);
                    }
                    buffer.truncate(read);
                    sent = sent.saturating_add(read as u64);
                    if let Some(progress) = &progress {
                        let _ = progress.send(UploadProgress {
                            sent,
                            total: size_bytes,
                        });
                    }
                    Ok(Some((Bytes::from(buffer), (file, sent, progress))))
                },
            );
            let response = self
                .client
                .put(upload_url.clone())
                .header(CONTENT_TYPE, content_type)
                .header(CONTENT_LENGTH, size_bytes)
                .body(reqwest::Body::wrap_stream(stream))
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response)
                    if !response.status().is_server_error()
                        && response.status() != StatusCode::TOO_MANY_REQUESTS =>
                {
                    return Err(response_error(format!(
                        "fal CDN rejected the upload with HTTP {}",
                        response.status().as_u16()
                    )));
                }
                _ if attempt < 2 => {
                    tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
                }
                _ => {
                    return Err(ProviderError::new(
                        ProviderId::fal(),
                        ProviderErrorKind::Network,
                        "fal CDN upload failed after safe retries",
                    ));
                }
            }
        }
        unreachable!("upload retry loop always returns")
    }

    async fn upload_multipart(
        &self,
        upload_url: &Url,
        path: &Path,
        size_bytes: u64,
        progress: Option<mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<(), ProviderError> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|error| validation(format!("Could not open local media: {error}")))?;
        let mut sent = 0u64;
        let mut part_number = 1u64;
        let mut parts = Vec::new();
        if let Some(progress) = &progress {
            let _ = progress.send(UploadProgress {
                sent,
                total: size_bytes,
            });
        }
        loop {
            let mut chunk = vec![0u8; MULTIPART_CHUNK_SIZE];
            let mut read_total = 0usize;
            while read_total < chunk.len() {
                let read = file
                    .read(&mut chunk[read_total..])
                    .await
                    .map_err(|error| validation(format!("Could not read local media: {error}")))?;
                if read == 0 {
                    break;
                }
                read_total += read;
            }
            if read_total == 0 {
                break;
            }
            chunk.truncate(read_total);
            let part_url = upload_child_url(upload_url, &part_number.to_string())?;
            let response = self.put_bytes(&part_url, Bytes::from(chunk)).await?;
            let response_part = response
                .get("partNumber")
                .or_else(|| response.get("part_number"))
                .and_then(Value::as_u64)
                .unwrap_or(part_number);
            let etag = response
                .get("etag")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| response_error("fal CDN upload part is missing its ETag"))?;
            parts.push(json!({"partNumber": response_part, "etag": etag}));
            sent = sent.saturating_add(read_total as u64);
            if let Some(progress) = &progress {
                let _ = progress.send(UploadProgress {
                    sent,
                    total: size_bytes,
                });
            }
            part_number += 1;
        }
        let complete_url = upload_child_url(upload_url, "complete")?;
        let completion = json!({"parts": parts});
        for attempt in 0..3 {
            let response = self
                .client
                .post(complete_url.clone())
                .header(CONTENT_TYPE, "application/json")
                .json(&completion)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response)
                    if !response.status().is_server_error()
                        && response.status() != StatusCode::TOO_MANY_REQUESTS =>
                {
                    return Err(response_error(format!(
                        "fal CDN could not complete the upload (HTTP {})",
                        response.status().as_u16()
                    )));
                }
                _ if attempt < 2 => {
                    tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
                }
                _ => {
                    return Err(ProviderError::new(
                        ProviderId::fal(),
                        ProviderErrorKind::Network,
                        "Could not complete fal CDN multipart upload after safe retries",
                    ));
                }
            }
        }
        unreachable!("multipart completion retry loop always returns")
    }
}

#[async_trait]
impl FalUploadExecutor for ReqwestFalUploadExecutor {
    async fn upload(
        &self,
        upload_url: &Url,
        path: &Path,
        content_type: &str,
        size_bytes: u64,
        multipart: bool,
        progress: Option<mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<(), ProviderError> {
        validate_upload_url(upload_url)?;
        if multipart {
            self.upload_multipart(upload_url, path, size_bytes, progress)
                .await
        } else {
            self.upload_single(upload_url, path, content_type, size_bytes, progress)
                .await
        }
    }
}

pub struct FalProvider {
    api_key: SecretString,
    options: FalOptions,
    executor: Arc<dyn HttpExecutor>,
    upload_executor: Arc<dyn FalUploadExecutor>,
    models: RwLock<BTreeMap<String, VideoModel>>,
}

impl fmt::Debug for FalProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FalProvider")
            .field("api_key", &"[REDACTED]")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl FalProvider {
    pub fn new(api_key: SecretString) -> Result<Self, ProviderError> {
        let options = FalOptions::default();
        let executor = Arc::new(ReqwestExecutor::new(options.timeout).map_err(|error| {
            ProviderError::new(
                ProviderId::fal(),
                ProviderErrorKind::Configuration,
                error.message,
            )
        })?);
        Self::with_executor(api_key, options, executor)
    }

    pub fn from_key(api_key: impl Into<String>) -> Result<Self, ProviderError> {
        Self::new(SecretString::from(api_key.into()))
    }

    pub fn with_executor(
        api_key: SecretString,
        options: FalOptions,
        executor: Arc<dyn HttpExecutor>,
    ) -> Result<Self, ProviderError> {
        let upload_executor = Arc::new(ReqwestFalUploadExecutor::new(options.timeout)?);
        Self::with_executors(api_key, options, executor, upload_executor)
    }

    pub fn with_executors(
        api_key: SecretString,
        mut options: FalOptions,
        executor: Arc<dyn HttpExecutor>,
        upload_executor: Arc<dyn FalUploadExecutor>,
    ) -> Result<Self, ProviderError> {
        let key = api_key.expose_secret().trim().to_owned();
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            return Err(ProviderError::new(
                ProviderId::fal(),
                ProviderErrorKind::Configuration,
                "A valid fal API key is required",
            ));
        }
        validate_base(&options.platform_base_url, "fal platform URL")?;
        validate_base(&options.queue_base_url, "fal queue URL")?;
        validate_base(&options.storage_base_url, "fal storage URL")?;
        if options.platform_base_url.host_str() != Some("api.fal.ai")
            || options.queue_base_url.host_str() != Some("queue.fal.run")
            || options.storage_base_url.host_str() != Some("rest.fal.ai")
        {
            return Err(configuration(
                "fal credentials may only be sent to api.fal.ai, queue.fal.run, and rest.fal.ai",
            ));
        }
        normalize_base(&mut options.platform_base_url);
        normalize_base(&mut options.queue_base_url);
        normalize_base(&mut options.storage_base_url);
        Ok(Self {
            api_key: SecretString::from(key),
            options,
            executor,
            upload_executor,
            models: RwLock::new(BTreeMap::new()),
        })
    }

    pub fn options(&self) -> &FalOptions {
        &self.options
    }

    /// Parse either a fal queue/status/result URL into a safe locator.
    pub fn parse_import_url(value: &str) -> Result<JobLocator, ProviderError> {
        let url = Url::parse(value).map_err(|_| validation("Invalid fal queue URL"))?;
        if url.scheme() != "https"
            || url.host_str() != Some("queue.fal.run")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ProviderError::new(
                ProviderId::fal(),
                ProviderErrorKind::UnsafeEndpoint,
                "fal imports must use an HTTPS queue.fal.run URL",
            ));
        }
        let parts = url
            .path_segments()
            .map(|parts| parts.filter(|part| !part.is_empty()).collect::<Vec<_>>())
            .unwrap_or_default();
        let request_index = parts
            .iter()
            .position(|part| *part == "requests")
            .ok_or_else(|| validation("fal queue URL is missing /requests/{request_id}"))?;
        if request_index == 0 || request_index + 1 >= parts.len() {
            return Err(validation(
                "fal queue URL is missing an endpoint or request id",
            ));
        }
        let endpoint_id = parts[..request_index].join("/");
        let request_id = parts[request_index + 1].to_owned();
        let suffix = &parts[request_index + 2..];
        if !matches!(suffix, [] | ["status"] | ["response"]) {
            return Err(validation("fal queue URL has an unknown request suffix"));
        }
        Ok(JobLocator::Fal {
            endpoint_id,
            request_id,
            status_url: (suffix == ["status"]).then(|| url.as_str().to_owned()),
            response_url: (suffix == ["response"]).then(|| url.as_str().to_owned()),
        })
    }

    pub fn locator(
        endpoint_id: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Result<JobLocator, ProviderError> {
        let endpoint_id = endpoint_id.into().trim_matches('/').to_owned();
        let request_id = request_id.into().trim_matches('/').to_owned();
        let locator = JobLocator::Fal {
            endpoint_id,
            request_id,
            status_url: None,
            response_url: None,
        };
        locator
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        Ok(locator)
    }

    async fn load_catalog_pages(&self) -> Result<Vec<Value>, ProviderError> {
        let mut discovered = BTreeMap::<String, Value>::new();
        for category in DISCOVERY_CATEGORIES {
            let mut cursor = None::<String>;
            loop {
                let mut url = self.platform_url(&["models"])?;
                {
                    let mut query = url.query_pairs_mut();
                    query
                        .append_pair("category", category)
                        .append_pair("status", "active")
                        .append_pair("limit", "50");
                    if let Some(cursor) = &cursor {
                        query.append_pair("cursor", cursor);
                    }
                }
                // Discovery metadata is public; do not attach a credential.
                let payload = self
                    .request_json(Method::GET, url, false, true, None)
                    .await?;
                let models = payload
                    .get("models")
                    .and_then(Value::as_array)
                    .ok_or_else(|| response_error("fal model response is missing models"))?;
                for model in models {
                    if let Some(id) = model.get("endpoint_id").and_then(Value::as_str) {
                        discovered.insert(id.to_owned(), model.clone());
                    }
                }
                cursor = payload
                    .get("next_cursor")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                if cursor.is_none()
                    || !payload
                        .get("has_more")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    break;
                }
            }
        }

        let ids = discovered.keys().cloned().collect::<Vec<_>>();
        for batch in ids.chunks(10) {
            let mut url = self.platform_url(&["models"])?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("expand", "openapi-3.0");
                for id in batch {
                    query.append_pair("endpoint_id", id);
                }
            }
            let payload = self
                .request_json(Method::GET, url, false, true, None)
                .await?;
            let models = payload
                .get("models")
                .and_then(Value::as_array)
                .ok_or_else(|| response_error("fal schema response is missing models"))?;
            for model in models {
                if let Some(id) = model.get("endpoint_id").and_then(Value::as_str) {
                    discovered.insert(id.to_owned(), model.clone());
                }
            }
        }
        Ok(discovered.into_values().collect())
    }

    async fn ensure_model(&self, model_id: &str) -> Result<VideoModel, ProviderError> {
        if let Some(model) = self
            .models
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(model_id)
            .cloned()
        {
            return Ok(model);
        }
        let mut url = self.platform_url(&["models"])?;
        url.query_pairs_mut()
            .append_pair("endpoint_id", model_id)
            .append_pair("expand", "openapi-3.0");
        // This targeted schema lookup is public and safe to repeat. A submit
        // cache miss must not reload the entire marketplace catalog.
        let payload = self
            .request_json(Method::GET, url, false, true, None)
            .await?;
        let raw = payload
            .get("models")
            .and_then(Value::as_array)
            .and_then(|models| {
                models.iter().find(|model| {
                    model.get("endpoint_id").and_then(Value::as_str) == Some(model_id)
                })
            })
            .ok_or_else(|| response_error("fal model lookup did not return the requested model"))?;
        let model = normalize_fal_model(raw)?.ok_or_else(|| {
            validation(format!(
                "fal model {model_id} is not compatible with video input/output"
            ))
        })?;
        self.models
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(model_id.to_owned(), model.clone());
        Ok(model)
    }

    fn platform_url(&self, segments: &[&str]) -> Result<Url, ProviderError> {
        append_segments(self.options.platform_base_url.clone(), segments)
    }

    fn queue_url(&self, endpoint_id: &str, suffix: &[&str]) -> Result<Url, ProviderError> {
        let mut segments = endpoint_id
            .split('/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        segments.extend_from_slice(suffix);
        append_segments(self.options.queue_base_url.clone(), &segments)
    }

    fn storage_url(&self, segments: &[&str]) -> Result<Url, ProviderError> {
        append_segments(self.options.storage_base_url.clone(), segments)
    }

    async fn request_json(
        &self,
        method: Method,
        url: Url,
        authenticated: bool,
        retry_safe: bool,
        body: Option<Value>,
    ) -> Result<Value, ProviderError> {
        self.request_json_with_headers(
            method,
            url,
            authenticated,
            retry_safe,
            body,
            HeaderMap::new(),
        )
        .await
    }

    async fn request_json_with_headers(
        &self,
        method: Method,
        url: Url,
        authenticated: bool,
        retry_safe: bool,
        body: Option<Value>,
        extra_headers: HeaderMap,
    ) -> Result<Value, ProviderError> {
        let max_attempts = if retry_safe {
            self.options.max_retries + 1
        } else {
            1
        };
        for attempt in 0..max_attempts {
            let request = self.http_request(
                method.clone(),
                url.clone(),
                authenticated,
                body.clone(),
                &extra_headers,
            )?;
            let response = match self.executor.execute(request).await {
                Ok(response) => response,
                Err(_) if !retry_safe => {
                    return Err(ProviderError::new(
                        ProviderId::fal(),
                        ProviderErrorKind::SubmissionUncertain,
                        "Connection failed during fal submission. The request may exist; do not submit again until history is checked.",
                    ));
                }
                Err(_) if attempt + 1 < max_attempts => {
                    self.backoff(attempt).await;
                    continue;
                }
                Err(_) => {
                    return Err(ProviderError::new(
                        ProviderId::fal(),
                        ProviderErrorKind::Network,
                        "Could not reach fal.ai",
                    ));
                }
            };
            if !response.status.is_success() {
                let status = response.status;
                let retry = retry_safe && retryable(response.status) && attempt + 1 < max_attempts;
                let error = self.error_from_http(response).await;
                if !retry_safe && submission_status_is_ambiguous(status) {
                    return Err(submission_uncertain_from_http(error));
                }
                if retry {
                    if let Some(delay) = error.retry_after {
                        tokio::time::sleep(delay).await;
                    } else {
                        self.backoff(attempt).await;
                    }
                    continue;
                }
                return Err(error);
            }
            let bytes = collect_body(response, MAX_JSON_BYTES).await.map_err(|error| {
                if retry_safe {
                    error
                } else {
                    submission_uncertain(
                        "fal accepted the submission connection but its response was interrupted",
                    )
                }
            })?;
            let payload: Value = serde_json::from_slice(&bytes).map_err(|_| {
                if retry_safe {
                    response_error("fal.ai returned invalid JSON")
                } else {
                    submission_uncertain(
                        "fal returned an unreadable submission response; the request may exist",
                    )
                }
            })?;
            if !payload.is_object() {
                return Err(if retry_safe {
                    response_error("fal.ai returned a non-object JSON response")
                } else {
                    submission_uncertain(
                        "fal returned an invalid submission response; the request may exist",
                    )
                });
            }
            return Ok(redact_value(&payload, self.api_key.expose_secret()));
        }
        Err(ProviderError::new(
            ProviderId::fal(),
            ProviderErrorKind::Network,
            "Could not reach fal.ai",
        ))
    }

    fn http_request(
        &self,
        method: Method,
        url: Url,
        authenticated: bool,
        body: Option<Value>,
        extra_headers: &HeaderMap,
    ) -> Result<HttpRequest, ProviderError> {
        let mut headers = extra_headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!(
                "video-harness/{} {DEFAULT_APP_TITLE}",
                env!("CARGO_PKG_VERSION")
            ))
            .map_err(|_| configuration("Invalid application title"))?,
        );
        if authenticated {
            let mut authorization =
                HeaderValue::from_str(&format!("Key {}", self.api_key.expose_secret()))
                    .map_err(|_| configuration("Invalid fal API key"))?;
            authorization.set_sensitive(true);
            headers.insert(AUTHORIZATION, authorization);
        }
        if body.is_some() {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        Ok(HttpRequest {
            method,
            url,
            headers,
            json_body: body,
        })
    }

    async fn backoff(&self, attempt: usize) {
        let factor = 1u32.checked_shl(attempt.min(16) as u32).unwrap_or(u32::MAX);
        tokio::time::sleep(self.options.backoff_base.saturating_mul(factor)).await;
    }

    async fn error_from_http(&self, response: HttpResponse) -> ProviderError {
        let status = response.status;
        let retry_after = parse_retry_after(response.headers.get(RETRY_AFTER));
        let body = collect_body(response, MAX_JSON_BYTES)
            .await
            .unwrap_or_default();
        let payload = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        let (message, code) = fal_error_message(&payload);
        let message = redact_text(&message, self.api_key.expose_secret());
        let code = code.map(|code| redact_text(&code, self.api_key.expose_secret()));
        let hint =
            format!("{} {message}", code.as_deref().unwrap_or_default()).to_ascii_lowercase();
        let kind = if ["insufficient credit", "insufficient_credit", "balance"]
            .iter()
            .any(|token| hint.contains(token))
        {
            ProviderErrorKind::InsufficientCredits
        } else if ["content policy", "content_policy", "moderation", "safety"]
            .iter()
            .any(|token| hint.contains(token))
        {
            ProviderErrorKind::ContentPolicy
        } else {
            match status.as_u16() {
                401 | 403 => ProviderErrorKind::Authentication,
                402 => ProviderErrorKind::InsufficientCredits,
                400 | 404 | 409 | 422 => ProviderErrorKind::Validation,
                429 => ProviderErrorKind::RateLimit,
                500..=599 => ProviderErrorKind::Unavailable,
                _ => ProviderErrorKind::Response,
            }
        };
        ProviderError {
            provider_id: ProviderId::fal(),
            kind,
            message: if message.is_empty() {
                format!("fal.ai request failed with HTTP {}", status.as_u16())
            } else {
                message
            },
            status_code: Some(status.as_u16()),
            code,
            details: Map::new(),
            retry_after,
        }
    }

    fn fal_input(
        &self,
        request: &VideoRequest,
        model: &VideoModel,
    ) -> Result<Value, ProviderError> {
        if request.provider_id != ProviderId::fal() || model.provider_id != ProviderId::fal() {
            return Err(validation("The request and model must belong to fal"));
        }
        request
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        let mut input = request
            .adapter_options
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut reserved = model.field_map.values().cloned().collect::<BTreeSet<_>>();
        reserved.extend(
            model
                .media_bindings
                .iter()
                .map(|binding| binding.property_name.clone()),
        );
        if let Some(name) = input.keys().find(|key| reserved.contains(*key)) {
            return Err(validation(format!(
                "Advanced JSON cannot override the common field {name}"
            )));
        }
        insert_common(
            &mut input,
            model,
            "prompt",
            Value::String(request.prompt.trim().into()),
        );
        insert_optional(
            &mut input,
            model,
            "duration",
            request
                .duration
                .map(|duration| fal_duration_value(model, duration)),
        )?;
        insert_optional(
            &mut input,
            model,
            "resolution",
            request.resolution.clone().map(Value::String),
        )?;
        insert_optional(
            &mut input,
            model,
            "aspect_ratio",
            request.aspect_ratio.clone().map(Value::String),
        )?;
        insert_optional(
            &mut input,
            model,
            "size",
            request.size.clone().map(Value::String),
        )?;
        insert_optional(
            &mut input,
            model,
            "generate_audio",
            request.generate_audio.map(Value::Bool),
        )?;
        insert_optional(&mut input, model, "seed", request.seed.map(Value::from))?;
        for frame in &request.frame_images {
            let canonical = match frame.frame_type {
                crate::domain::FrameType::FirstFrame => "first_frame",
                crate::domain::FrameType::LastFrame => "last_frame",
            };
            insert_optional(
                &mut input,
                model,
                canonical,
                Some(Value::String(frame.url.clone())),
            )?;
        }
        bind_media_references(&mut input, model, request)?;
        let value = Value::Object(input);
        if let Some(schema) = &model.input_schema {
            validate_schema(schema, &value, "$input")?;
        }
        Ok(value)
    }

    async fn download_anonymous(
        &self,
        artifact: &VideoArtifact,
        destination: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadProgress>>,
    ) -> Result<PathBuf, ProviderError> {
        artifact
            .validate()
            .map_err(|error| unsafe_endpoint(error.to_string()))?;
        let mut url =
            Url::parse(&artifact.url).map_err(|_| unsafe_endpoint("Invalid artifact URL"))?;
        validate_download_url(&url)?;
        if destination.exists() || partial_path(destination).exists() {
            return Err(download_error(format!(
                "Refusing to overwrite an existing download: {}",
                destination.display()
            )));
        }
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                download_error(format!("Could not create video directory: {error}"))
            })?;
        }
        let mut response = None;
        for redirect in 0..=MAX_REDIRECTS {
            let mut headers = HeaderMap::new();
            headers.insert(
                ACCEPT,
                HeaderValue::from_static("video/*,application/octet-stream"),
            );
            headers.insert(
                USER_AGENT,
                HeaderValue::from_static(concat!(
                    "video-harness/",
                    env!("CARGO_PKG_VERSION"),
                    " Video Harness"
                )),
            );
            let current = self
                .executor
                .execute(HttpRequest {
                    method: Method::GET,
                    url: url.clone(),
                    headers,
                    json_body: None,
                })
                .await
                .map_err(|_| download_error("Video download connection failed"))?;
            if current.status.is_redirection() {
                if redirect == MAX_REDIRECTS {
                    return Err(download_error("Video download redirected too many times"));
                }
                let location = current
                    .headers
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| download_error("Video redirect is missing Location"))?;
                url = url
                    .join(location)
                    .map_err(|_| unsafe_endpoint("Unsafe video redirect"))?;
                validate_download_url(&url)?;
                continue;
            }
            response = Some(current);
            break;
        }
        let response = response.ok_or_else(|| download_error("Video download failed"))?;
        if !response.status.is_success() {
            return Err(download_error(format!(
                "Video download failed with HTTP {}",
                response.status.as_u16()
            )));
        }
        let total = response
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let partial = partial_path(destination);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await
            .map_err(|error| download_error(format!("Could not create partial video: {error}")))?;
        let mut body = response.body;
        let mut written = 0u64;
        let transfer = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|_| download_error("Video download was interrupted"))?;
                file.write_all(&chunk)
                    .await
                    .map_err(|error| download_error(format!("Could not write video: {error}")))?;
                written = written.saturating_add(chunk.len() as u64);
                if let Some(progress) = &progress {
                    let _ = progress.send(DownloadProgress { written, total });
                }
            }
            file.flush()
                .await
                .map_err(|error| download_error(format!("Could not flush video: {error}")))?;
            file.sync_all()
                .await
                .map_err(|error| download_error(format!("Could not sync video: {error}")))?;
            Ok::<(), ProviderError>(())
        }
        .await;
        drop(file);
        if let Err(error) = transfer {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(error);
        }
        if written == 0 {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(download_error("fal returned an empty video file"));
        }
        if let Some(total) = total
            && written != total
        {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(download_error(format!(
                "Video download size mismatch: expected {total} bytes, received {written}"
            )));
        }
        match tokio::fs::hard_link(&partial, destination).await {
            Ok(()) => {
                let _ = tokio::fs::remove_file(&partial).await;
                Ok(destination.to_owned())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = tokio::fs::remove_file(&partial).await;
                Err(download_error(format!(
                    "Refusing to overwrite an existing video: {}",
                    destination.display()
                )))
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&partial).await;
                Err(download_error(format!("Could not finalize video: {error}")))
            }
        }
    }

    async fn submit_request(
        &self,
        request: &VideoRequest,
        submit_before: Option<DateTime<Utc>>,
    ) -> Result<VideoJob, ProviderError> {
        let model = self.ensure_model(&request.model).await?;
        let input = self.fal_input(request, &model)?;
        let url = self.queue_url(&request.model, &[])?;
        // Schema resolution above can involve retryable public GETs. Check
        // staged-input freshness only after that work and immediately before
        // the one potentially billable POST.
        if submit_before.is_some_and(|deadline| Utc::now() >= deadline) {
            return Err(validation(
                "Staged input media is too close to expiring; Review again before generating",
            ));
        }
        let payload = self
            .request_json(Method::POST, url, true, false, Some(input))
            .await?;
        fal_submitted_job(&request.model, &payload).map_err(|error| {
            submission_uncertain(format!(
                "fal returned an invalid accepted-job response: {}. The request may exist.",
                error.message
            ))
        })
    }
}

#[async_trait]
impl VideoProvider for FalProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::fal(),
            display_name: "fal.ai".into(),
            website: "https://fal.ai".into(),
        }
    }

    fn media_capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            remote_urls: true,
            local_files: true,
            uploaded_files_public: true,
            upload_retention: Some(Duration::from_secs(INPUT_UPLOAD_RETENTION_SECONDS)),
        }
    }

    fn validate_draft_media_constraints(
        &self,
        draft: &crate::domain::GenerationDraft,
    ) -> Result<(), ProviderError> {
        if !is_seedance_2_endpoint(&draft.model) {
            return Ok(());
        }
        let mut local_video_bytes = 0u64;
        for media in &draft.media {
            let MediaSource::LocalFile { path } = &media.source else {
                continue;
            };
            let size = std::fs::metadata(path)
                .map_err(|error| {
                    validation(format!(
                        "Could not inspect local media {}: {error}",
                        path.display()
                    ))
                })?
                .len();
            match media.role.kind() {
                MediaKind::Image => {
                    let extension = path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(str::to_ascii_lowercase)
                        .unwrap_or_default();
                    if !matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp") {
                        return Err(validation(
                            "Seedance 2.0 image inputs must use JPEG, PNG, or WebP",
                        ));
                    }
                    if size > SEEDANCE_MAX_IMAGE_BYTES {
                        return Err(validation(format!(
                            "Seedance 2.0 image inputs must be at most {} MB each",
                            SEEDANCE_MAX_IMAGE_BYTES / 1_000_000
                        )));
                    }
                }
                MediaKind::Video => {
                    local_video_bytes = local_video_bytes.saturating_add(size);
                }
                MediaKind::Audio if size > SEEDANCE_MAX_AUDIO_BYTES => {
                    return Err(validation(format!(
                        "Seedance 2.0 audio inputs must be at most {} MB each",
                        SEEDANCE_MAX_AUDIO_BYTES / 1_000_000
                    )));
                }
                MediaKind::Audio => {}
            }
        }
        if local_video_bytes >= SEEDANCE_MAX_VIDEO_BYTES {
            return Err(validation(format!(
                "Seedance 2.0 local video inputs must total less than {} MB",
                SEEDANCE_MAX_VIDEO_BYTES / 1_000_000
            )));
        }
        Ok(())
    }

    fn validate_staged_media_constraints(
        &self,
        draft: &crate::domain::GenerationDraft,
        staged_media: &[StagedMedia],
    ) -> Result<(), ProviderError> {
        validate_seedance_staged_media_constraints(draft, staged_media)
    }

    async fn validate_request(&self, request: &VideoRequest) -> Result<(), ProviderError> {
        if request.provider_id != ProviderId::fal() {
            return Err(validation("The request belongs to a different provider"));
        }
        request
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        let model = self.ensure_model(&request.model).await?;
        self.fal_input(request, &model)?;
        Ok(())
    }

    async fn stage_media(
        &self,
        media: &DraftMedia,
        cached_receipt: Option<&UploadReceipt>,
        progress: Option<mpsc::UnboundedSender<UploadProgress>>,
    ) -> Result<StagedMedia, ProviderError> {
        let MediaSource::LocalFile { path } = &media.source else {
            media
                .validate()
                .map_err(|error| validation(error.to_string()))?;
            let MediaSource::RemoteUrl { url } = &media.source else {
                unreachable!()
            };
            return StagedMedia::remote(media.role, url.clone())
                .map_err(|error| validation(error.to_string()));
        };

        // Take the accepted file snapshot before signature validation. This
        // closes the replacement window between validating the bytes and
        // selecting the file identity whose hash will be uploaded.
        let source_snapshot = local_file_snapshot(path).await?;
        media
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        ensure_local_file_unchanged(path, &source_snapshot).await?;
        let (sha256, size_bytes) = media_sha256(path)
            .await
            .map_err(|error| validation(format!("Could not read local media: {error}")))?;
        if size_bytes == 0 {
            return Err(validation("Local media file is empty"));
        }
        ensure_local_file_unchanged(path, &source_snapshot).await?;
        let now = Utc::now();
        if let Some(receipt) = cached_receipt.filter(|receipt| {
            receipt.reusable_for(&ProviderId::fal(), &sha256, now)
                && receipt.size_bytes == size_bytes
        }) {
            return StagedMedia::uploaded(media.role, receipt.clone())
                .map_err(|error| validation(error.to_string()));
        }

        let content_type = media_content_type(path);
        let mut file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "reference.bin".into());
        file_name.retain(|character| !character.is_control());
        if file_name.is_empty() {
            file_name = "reference.bin".into();
        }
        let multipart = size_bytes > MULTIPART_THRESHOLD;
        let endpoint = if multipart {
            "initiate-multipart"
        } else {
            "initiate"
        };
        let mut url = self.storage_url(&["storage", "upload", endpoint])?;
        url.query_pairs_mut()
            .append_pair("storage_type", "fal-cdn-v3");
        let lifecycle = json!({
            "expiration_duration_seconds": INPUT_UPLOAD_RETENTION_SECONDS
        })
        .to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-fal-object-lifecycle"),
            HeaderValue::from_str(&lifecycle)
                .map_err(|_| configuration("Invalid fal upload lifecycle"))?,
        );
        let upload_started_at = Utc::now();
        let initiated = self
            .request_json_with_headers(
                Method::POST,
                url,
                true,
                true,
                Some(json!({
                    "file_name": file_name,
                    "content_type": content_type.clone(),
                })),
                headers,
            )
            .await?;
        let public_url = initiated
            .get("file_url")
            .and_then(Value::as_str)
            .ok_or_else(|| response_error("fal upload initiation is missing file_url"))?;
        StagedMedia::remote(media.role, public_url.to_owned())
            .map_err(|error| unsafe_endpoint(error.to_string()))?;
        let upload_url = initiated
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or_else(|| response_error("fal upload initiation is missing upload_url"))?;
        let upload_url = Url::parse(upload_url)
            .map_err(|_| unsafe_endpoint("fal returned an invalid CDN upload URL"))?;
        validate_upload_url(&upload_url)?;
        // No fal API credential is attached to this provider-signed URL.
        self.upload_executor
            .upload(
                &upload_url,
                path,
                &content_type,
                size_bytes,
                multipart,
                progress,
            )
            .await?;
        ensure_local_file_unchanged(path, &source_snapshot).await?;

        // The provider may start the retention clock when the upload is
        // initiated, so never overstate the cache lifetime by upload time.
        let uploaded_at = upload_started_at;
        let expires_at =
            uploaded_at + chrono::Duration::seconds(INPUT_UPLOAD_RETENTION_SECONDS as i64);
        let receipt = UploadReceipt::new(
            ProviderId::fal(),
            sha256,
            public_url,
            uploaded_at,
            expires_at,
            Some(content_type),
            size_bytes,
        )
        .map_err(|error| response_error(error.to_string()))?;
        StagedMedia::uploaded(media.role, receipt)
            .map_err(|error| response_error(error.to_string()))
    }

    async fn validate_credentials(&self) -> Result<ProviderAccount, ProviderError> {
        let mut url = self.platform_url(&["models"])?;
        url.query_pairs_mut().append_pair("limit", "1");
        let raw = self
            .request_json(Method::GET, url, true, true, None)
            .await?;
        Ok(ProviderAccount {
            label: "fal.ai API key".into(),
            balance: None,
            raw,
        })
    }

    async fn load_catalog(&self) -> Result<VideoCatalog, ProviderError> {
        let mut normalized = Vec::new();
        for raw in self.load_catalog_pages().await? {
            if let Ok(Some(model)) = normalize_fal_model(&raw) {
                normalized.push(model);
            }
        }
        normalized.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        let mut cache = self
            .models
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cache = normalized
            .iter()
            .map(|model| (model.id.clone(), model.clone()))
            .collect();
        drop(cache);
        Ok(VideoCatalog::new(ProviderId::fal(), normalized, false))
    }

    async fn quote(&self, request: &VideoRequest) -> Result<CostQuote, ProviderError> {
        self.validate_request(request).await?;
        let mut url = self.platform_url(&["models", "pricing"])?;
        url.query_pairs_mut()
            .append_pair("endpoint_id", &request.model);
        let payload = self
            .request_json(Method::GET, url, true, true, None)
            .await?;
        let price = payload
            .get("prices")
            .and_then(Value::as_array)
            .and_then(|prices| {
                prices.iter().find(|price| {
                    price.get("endpoint_id").and_then(Value::as_str) == Some(&request.model)
                })
            })
            .ok_or_else(|| response_error("fal pricing response does not include this model"))?;
        let unit = price
            .get("unit")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let unit_price = price.get("unit_price").and_then(decimal_value);
        let mut raw_pricing = BTreeMap::new();
        if let Some(value) = unit_price {
            raw_pricing.insert(unit.to_owned(), value);
        }
        let normalized = unit.to_ascii_lowercase().replace([' ', '-'], "_");
        let (amount, mut basis, mut confidence) =
            if matches!(normalized.as_str(), "video" | "request" | "generation") {
                (
                    unit_price,
                    format!("Advertised fal price per {unit}"),
                    if unit_price.is_some() {
                        QuoteConfidence::Exact
                    } else {
                        QuoteConfidence::Unknown
                    },
                )
            } else if matches!(
                normalized.as_str(),
                "video_second" | "second" | "output_second"
            ) {
                match (unit_price, request.duration) {
                    (Some(price), Some(duration)) => (
                        Some(price * Decimal::from(duration)),
                        format!("Estimated {duration}s × ${price}/{unit}"),
                        QuoteConfidence::Estimated,
                    ),
                    _ => (
                        None,
                        format!("fal bills per {unit}; duration is required"),
                        QuoteConfidence::Unknown,
                    ),
                }
            } else {
                (
                    None,
                    format!("fal uses unsupported billing unit {unit}"),
                    QuoteConfidence::Unknown,
                )
            };
        apply_request_quote_uncertainty(request, &mut basis, &mut confidence);
        let currency = price
            .get("currency")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("USD")
            .to_owned();
        Ok(CostQuote {
            amount,
            basis,
            exact: confidence == QuoteConfidence::Exact,
            pricing_sku: Some(unit.to_owned()),
            unit_price,
            currency,
            raw_pricing,
            confidence,
        })
    }

    async fn submit(&self, request: &VideoRequest) -> Result<VideoJob, ProviderError> {
        self.submit_request(request, None).await
    }

    async fn submit_prepared(
        &self,
        request: &VideoRequest,
        submit_before: Option<DateTime<Utc>>,
    ) -> Result<VideoJob, ProviderError> {
        self.submit_request(request, submit_before).await
    }

    async fn poll(&self, locator: &JobLocator) -> Result<VideoJob, ProviderError> {
        let JobLocator::Fal {
            endpoint_id,
            request_id,
            response_url,
            ..
        } = locator
        else {
            return Err(validation(
                "fal cannot poll a locator owned by another provider",
            ));
        };
        locator
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        let status_url = self.queue_url(endpoint_id, &["requests", request_id, "status"])?;
        let status = self
            .request_json(Method::GET, status_url, true, true, None)
            .await?;
        let state = status
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN");
        if !state.eq_ignore_ascii_case("COMPLETED") || fal_payload_error(&status).is_some() {
            return fal_status_job(endpoint_id, request_id, &status);
        }
        let result = if let Some(response_url) = response_url {
            let response_url = Url::parse(response_url)
                .map_err(|_| unsafe_endpoint("Invalid fal response URL"))?;
            self.request_json(Method::GET, response_url, true, true, None)
                .await?
        } else {
            let with_response =
                self.queue_url(endpoint_id, &["requests", request_id, "response"])?;
            match self
                .request_json(Method::GET, with_response, true, true, None)
                .await
            {
                Ok(result) => result,
                Err(error) if error.status_code == Some(404) => {
                    let base = self.queue_url(endpoint_id, &["requests", request_id])?;
                    self.request_json(Method::GET, base, true, true, None)
                        .await?
                }
                Err(error) => return Err(error),
            }
        };
        fal_result_job(endpoint_id, request_id, &status, &result)
    }

    async fn download(
        &self,
        artifact: &VideoArtifact,
        destination: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadProgress>>,
    ) -> Result<PathBuf, ProviderError> {
        self.download_anonymous(artifact, destination, progress)
            .await
    }
}

fn normalize_fal_model(raw: &Value) -> Result<Option<VideoModel>, ProviderError> {
    let endpoint_id = raw
        .get("endpoint_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let metadata = raw.get("metadata").and_then(Value::as_object);
    let category = metadata
        .and_then(|value| value.get("category"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if endpoint_id.is_empty()
        || !DISCOVERY_CATEGORIES.contains(&category)
        || metadata
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .is_some_and(|status| status != "active")
    {
        return Ok(None);
    }
    let Some(openapi) = raw.get("openapi") else {
        return Ok(None);
    };
    let Some((input_schema, output_schema)) = openapi_inference_schemas(openapi, endpoint_id)?
    else {
        return Ok(None);
    };
    let resolved_input = resolve_refs(&input_schema, openapi, 0)?;
    let resolved_output = resolve_refs(&output_schema, openapi, 0)?;
    if !schema_has_unambiguous_video_output(&resolved_output) {
        return Ok(None);
    }
    let field_map = common_field_map(&resolved_input);
    let Some(prompt_name) = field_map.get("prompt") else {
        return Ok(None);
    };
    let properties = schema_properties(&resolved_input);
    if !properties
        .get(prompt_name)
        .is_some_and(schema_accepts_nullable_string)
    {
        return Ok(None);
    }
    let Some(media_bindings) = normalize_media_bindings(&resolved_input, &field_map) else {
        return Ok(None);
    };
    let enum_strings = |canonical: &str| {
        field_map
            .get(canonical)
            .and_then(|name| properties.get(name))
            .and_then(|schema| schema.get("enum"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let durations = field_map
        .get("duration")
        .and_then(|name| properties.get(name))
        .and_then(|schema| schema.get("enum"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                        .and_then(|value| u32::try_from(value).ok())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let supported_frame_images = ["first_frame", "last_frame"]
        .into_iter()
        .filter(|field| field_map.contains_key(*field))
        .collect::<Vec<_>>();
    let mut input_modalities = media_bindings
        .iter()
        .map(|binding| binding.kind)
        .collect::<BTreeSet<_>>();
    if !supported_frame_images.is_empty() {
        input_modalities.insert(MediaKind::Image);
    }
    let generated_audio_supported = field_map.contains_key("generate_audio");
    let generated_audio_default = field_map
        .get("generate_audio")
        .and_then(|name| properties.get(name))
        .and_then(|schema| schema.get("default"))
        .and_then(Value::as_bool);
    let normalized = json!({
        "id": endpoint_id,
        "name": metadata.and_then(|value| value.get("display_name")).and_then(Value::as_str).unwrap_or(endpoint_id),
        "description": metadata.and_then(|value| value.get("description")).and_then(Value::as_str).unwrap_or_default(),
        "supported_resolutions": enum_strings("resolution"),
        "supported_aspect_ratios": enum_strings("aspect_ratio"),
        "supported_sizes": enum_strings("size"),
        "supported_durations": durations,
        "supported_frame_images": supported_frame_images,
        "input_modalities": input_modalities,
        "media_bindings": media_bindings,
        "generated_audio_capability": {
            "supported": generated_audio_supported,
            "provider_default": generated_audio_default,
        },
        // Keep this compatibility flag in cached raw catalogs written by
        // pre-v0.6 releases. New readers prefer the structured capability.
        "generate_audio": generated_audio_supported,
        "seed": field_map.contains_key("seed"),
        "allowed_passthrough_parameters": properties.keys().cloned().collect::<Vec<_>>(),
        "input_schema": resolved_input,
        "field_map": field_map,
        "fal_metadata": raw,
    });
    VideoModel::from_provider_api(ProviderId::fal(), &normalized)
        .map(Some)
        .map_err(|error| response_error(error.to_string()))
}

fn openapi_inference_schemas(
    openapi: &Value,
    endpoint_id: &str,
) -> Result<Option<(Value, Value)>, ProviderError> {
    let Some(paths) = openapi.get("paths").and_then(Value::as_object) else {
        return Ok(None);
    };
    let endpoint_path = format!("/{}", endpoint_id.trim_matches('/'));
    let inference_path = if paths
        .get(&endpoint_path)
        .and_then(|path| path.get("post"))
        .is_some()
    {
        endpoint_path.clone()
    } else if paths.get("/").and_then(|path| path.get("post")).is_some() {
        "/".to_owned()
    } else {
        let posts = paths
            .iter()
            .filter(|(_, path)| path.get("post").is_some())
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if posts.len() != 1 {
            return Ok(None);
        }
        posts[0].clone()
    };
    let Some(post) = paths.get(&inference_path).and_then(|path| path.get("post")) else {
        return Ok(None);
    };
    let Some(input_schema) = post
        .get("requestBody")
        .and_then(|body| body.get("content"))
        .and_then(|content| content.get("application/json"))
        .and_then(|content| content.get("schema"))
        .cloned()
    else {
        return Ok(None);
    };
    // A documented successful JSON response proves this is an invokable
    // inference operation rather than an unrelated component schema.
    let Some(post_response_schema) = success_json_schema(post) else {
        return Ok(None);
    };

    let inference_base = inference_path.trim_end_matches('/');
    let result_path = format!("{inference_base}/requests/{{request_id}}");
    let output_schema = paths
        .get(&result_path)
        .and_then(|path| path.get("get"))
        .and_then(success_json_schema)
        .unwrap_or(post_response_schema);
    // Resolve here only to ensure that a queue acknowledgement is never
    // mistaken for the generated result when the paired result path is absent.
    let resolved_output = resolve_refs(&output_schema, openapi, 0)?;
    if !schema_has_unambiguous_video_output(&resolved_output) {
        return Ok(None);
    }
    Ok(Some((input_schema, output_schema)))
}

fn success_json_schema(operation: &Value) -> Option<Value> {
    let responses = operation.get("responses")?.as_object()?;
    let mut codes = responses
        .keys()
        .filter(|code| {
            code.len() == 3
                && code.starts_with('2')
                && code.as_bytes().iter().all(u8::is_ascii_digit)
        })
        .collect::<Vec<_>>();
    codes.sort_unstable();
    codes.into_iter().find_map(|code| {
        responses
            .get(code)?
            .get("content")?
            .get("application/json")?
            .get("schema")
            .cloned()
    })
}

fn resolve_refs(value: &Value, root: &Value, depth: usize) -> Result<Value, ProviderError> {
    if depth > 64 {
        return Err(response_error(
            "fal OpenAPI schema contains recursive references",
        ));
    }
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                let pointer = reference.strip_prefix('#').ok_or_else(|| {
                    response_error("fal OpenAPI schema uses an external reference")
                })?;
                let target = root.pointer(pointer).ok_or_else(|| {
                    response_error("fal OpenAPI schema contains a missing reference")
                })?;
                return resolve_refs(target, root, depth + 1);
            }
            object
                .iter()
                .map(|(key, value)| {
                    resolve_refs(value, root, depth + 1).map(|value| (key.clone(), value))
                })
                .collect::<Result<Map<_, _>, _>>()
                .map(Value::Object)
        }
        Value::Array(values) => values
            .iter()
            .map(|value| resolve_refs(value, root, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Ok(value.clone()),
    }
}

fn common_field_map(schema: &Value) -> BTreeMap<String, String> {
    let properties = schema_properties(schema);
    let candidates: [(&str, &[&str]); 10] = [
        ("prompt", &["prompt", "text"]),
        (
            "duration",
            &[
                "duration",
                "duration_seconds",
                "video_length",
                "num_seconds",
            ],
        ),
        ("resolution", &["resolution", "video_resolution"]),
        ("aspect_ratio", &["aspect_ratio"]),
        ("size", &["size", "video_size"]),
        ("generate_audio", &["generate_audio", "enable_audio"]),
        ("seed", &["seed"]),
        (
            "first_frame",
            &["image_url", "start_image_url", "first_frame_url"],
        ),
        ("last_frame", &["end_image_url", "last_frame_url"]),
        (
            "references",
            &["reference_image_urls", "image_urls", "reference_images"],
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(canonical, names)| {
            names
                .iter()
                .find(|name| properties.contains_key(**name))
                .map(|name| (canonical.to_owned(), (*name).to_owned()))
        })
        .collect()
}

fn schema_properties(schema: &Value) -> Map<String, Value> {
    let mut properties = Map::new();
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for branch in all_of {
            properties.extend(schema_properties(branch));
        }
    }
    if let Some(direct) = schema.get("properties").and_then(Value::as_object) {
        properties.extend(direct.clone());
    }
    properties
}

fn schema_required(schema: &Value) -> BTreeSet<String> {
    let mut required = BTreeSet::new();
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for branch in all_of {
            required.extend(schema_required(branch));
        }
    }
    required.extend(
        schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned),
    );
    required
}

fn normalize_media_bindings(
    schema: &Value,
    field_map: &BTreeMap<String, String>,
) -> Option<Vec<MediaBinding>> {
    let properties = schema_properties(schema);
    let required = schema_required(schema);

    // A required media-shaped object or nested media structure cannot be
    // represented by the first-release scalar/list URL editor.
    for property_name in &required {
        let Some(property_schema) = properties.get(property_name) else {
            continue;
        };
        if media_kind_hint(property_name).is_some() {
            let frame_field = ["first_frame", "last_frame"]
                .into_iter()
                .filter_map(|canonical| field_map.get(canonical))
                .any(|name| name == property_name);
            media_cardinality(property_schema)?;
            if general_media_kind(property_name).is_none() && !frame_field {
                return None;
            }
        } else if schema_contains_media_property(property_schema) {
            return None;
        }
    }

    let mut candidates = BTreeMap::<MediaKind, Vec<MediaBinding>>::new();
    for property_name in ordered_property_names(schema, &properties) {
        let Some(kind) = general_media_kind(&property_name) else {
            continue;
        };
        let Some(property_schema) = properties.get(&property_name) else {
            continue;
        };
        let Some(cardinality) = media_cardinality(property_schema) else {
            if required.contains(&property_name) {
                return None;
            }
            continue;
        };
        candidates.entry(kind).or_default().push(MediaBinding {
            kind,
            property_name: property_name.clone(),
            cardinality,
            required: required.contains(&property_name),
            min_items: (cardinality == MediaCardinality::List)
                .then(|| schema_array_keyword(property_schema, "minItems"))
                .flatten(),
            max_items: (cardinality == MediaCardinality::List)
                .then(|| schema_array_keyword(property_schema, "maxItems"))
                .flatten(),
            title: normalized_schema_text(property_schema, "title"),
            description: normalized_schema_text(property_schema, "description"),
        });
    }

    // `image_url` and its start-frame aliases remain first-frame controls.
    // When no separate image-reference field exists, they are also the one
    // unambiguous scalar target for a general image reference.
    if candidates.get(&MediaKind::Image).is_none_or(Vec::is_empty)
        && let Some(property_name) = field_map.get("first_frame")
        && let Some(property_schema) = properties.get(property_name)
        && media_cardinality(property_schema) == Some(MediaCardinality::Scalar)
    {
        candidates
            .entry(MediaKind::Image)
            .or_default()
            .push(MediaBinding {
                kind: MediaKind::Image,
                property_name: property_name.clone(),
                cardinality: MediaCardinality::Scalar,
                required: required.contains(property_name),
                min_items: None,
                max_items: None,
                title: normalized_schema_text(property_schema, "title"),
                description: normalized_schema_text(property_schema, "description"),
            });
    }

    let mut bindings = Vec::new();
    for kind in [MediaKind::Image, MediaKind::Video, MediaKind::Audio] {
        let Some(mut same_kind) = candidates.remove(&kind) else {
            continue;
        };
        if same_kind.len() == 1 {
            bindings.push(same_kind.remove(0));
        } else if same_kind.iter().any(|binding| binding.required) {
            // Required same-kind fields cannot be populated without guessing
            // which selected asset has which provider-specific purpose.
            return None;
        }
        // Multiple optional same-kind targets are deliberately not advertised.
    }
    Some(bindings)
}

fn ordered_property_names(schema: &Value, properties: &Map<String, Value>) -> Vec<String> {
    let mut names = schema
        .get("x-fal-order-properties")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| properties.contains_key(*name))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let seen = names.iter().cloned().collect::<BTreeSet<_>>();
    names.extend(
        properties
            .keys()
            .filter(|name| !seen.contains(*name))
            .cloned(),
    );
    names
}

fn general_media_kind(name: &str) -> Option<MediaKind> {
    match name {
        "image_urls" | "reference_image_url" | "reference_image_urls" | "reference_images" => {
            Some(MediaKind::Image)
        }
        "video_url"
        | "video_urls"
        | "reference_video_url"
        | "reference_video_urls"
        | "control_video_url"
        | "input_video_url"
        | "source_video_url" => Some(MediaKind::Video),
        "audio_url"
        | "audio_urls"
        | "reference_audio_url"
        | "reference_audio_urls"
        | "input_audio_url"
        | "source_audio_url" => Some(MediaKind::Audio),
        _ => None,
    }
}

fn media_kind_hint(name: &str) -> Option<MediaKind> {
    general_media_kind(name).or(match name {
        "image" | "image_url" | "start_image_url" | "first_frame_url" | "end_image_url"
        | "last_frame_url" => Some(MediaKind::Image),
        "video" => Some(MediaKind::Video),
        "audio" => Some(MediaKind::Audio),
        _ => None,
    })
}

fn schema_contains_media_property(schema: &Value) -> bool {
    let properties = schema_properties(schema);
    if properties.iter().any(|(name, child)| {
        media_kind_hint(name).is_some() || schema_contains_media_property(child)
    }) {
        return true;
    }
    if let Some(items) = schema.get("items")
        && schema_contains_media_property(items)
    {
        return true;
    }
    ["allOf", "anyOf", "oneOf"].into_iter().any(|keyword| {
        schema
            .get(keyword)
            .and_then(Value::as_array)
            .is_some_and(|branches| branches.iter().any(schema_contains_media_property))
    })
}

fn normalized_schema_text(schema: &Value, key: &str) -> Option<String> {
    let value = schema.get(key)?.as_str()?;
    if value
        .chars()
        .any(|character| character.is_control() && !character.is_whitespace())
    {
        return None;
    }
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn media_cardinality(schema: &Value) -> Option<MediaCardinality> {
    if schema_accepts_nullable_string(schema) {
        Some(MediaCardinality::Scalar)
    } else if schema_accepts_nullable_array(schema)
        && schema_array_items(schema).is_some_and(schema_accepts_nullable_string)
    {
        Some(MediaCardinality::List)
    } else {
        None
    }
}

fn schema_explicit_types(schema: &Value) -> Option<BTreeSet<String>> {
    if let Some(kind) = schema.get("type") {
        let kinds = match kind {
            Value::String(kind) => [kind.clone()].into_iter().collect(),
            Value::Array(kinds) => kinds
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect(),
            _ => return None,
        };
        return Some(kinds);
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            let mut kinds = BTreeSet::new();
            for branch in branches {
                kinds.extend(schema_explicit_types(branch)?);
            }
            return Some(kinds);
        }
    }
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        let mut typed = branches.iter().filter_map(schema_explicit_types);
        let mut kinds = typed.next()?;
        for branch_kinds in typed {
            kinds = kinds.intersection(&branch_kinds).cloned().collect();
        }
        return Some(kinds);
    }
    None
}

fn schema_accepts_nullable_string(schema: &Value) -> bool {
    schema_explicit_types(schema).is_some_and(|types| {
        types.contains("string")
            && types
                .iter()
                .all(|kind| matches!(kind.as_str(), "string" | "null"))
    })
}

fn schema_accepts_nullable_array(schema: &Value) -> bool {
    schema_explicit_types(schema).is_some_and(|types| {
        types.contains("array")
            && types
                .iter()
                .all(|kind| matches!(kind.as_str(), "array" | "null"))
    })
}

fn schema_array_branch(schema: &Value) -> Option<&Value> {
    if schema.get("type").is_some_and(|kind| match kind {
        Value::String(kind) => kind == "array",
        Value::Array(kinds) => kinds.iter().any(|kind| kind.as_str() == Some("array")),
        _ => false,
    }) {
        return Some(schema);
    }
    ["anyOf", "oneOf", "allOf"].into_iter().find_map(|keyword| {
        schema
            .get(keyword)
            .and_then(Value::as_array)?
            .iter()
            .find_map(schema_array_branch)
    })
}

fn schema_array_items(schema: &Value) -> Option<&Value> {
    schema_array_branch(schema)?.get("items")
}

fn schema_array_keyword(schema: &Value, keyword: &str) -> Option<usize> {
    schema_array_branch(schema)?
        .get(keyword)?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
}

fn schema_has_unambiguous_video_output(schema: &Value) -> bool {
    if schema_may_be_null(schema) || !schema_has_only_type(schema, "object") {
        return false;
    }
    let required = schema_required(schema);
    let properties = schema_properties(schema);
    let mut video_fields = properties
        .iter()
        .filter(|(name, _)| output_name_is_video(name));
    let Some((name, property_schema)) = video_fields.next() else {
        return false;
    };
    video_fields.next().is_none()
        && required.contains(name)
        && schema_is_media_output(property_schema)
}

fn output_name_is_video(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "video" | "videos" | "video_url" | "video_urls"
    ) || name.ends_with("_video")
        || name.ends_with("_videos")
        || name.ends_with("_video_url")
        || name.ends_with("_video_urls")
}

fn schema_is_media_output(schema: &Value) -> bool {
    if schema_may_be_null(schema) {
        return false;
    }
    if schema_has_only_type(schema, "string") {
        return true;
    }
    if !schema_has_only_type(schema, "object") {
        return false;
    }
    let properties = schema_properties(schema);
    let required = schema_required(schema);
    let mut url_fields = properties.iter().filter(|(name, child)| {
        matches!(name.as_str(), "url" | "file_url" | "video_url")
            && !schema_may_be_null(child)
            && schema_has_only_type(child, "string")
    });
    if let Some((name, _)) = url_fields.next() {
        return required.contains(name) && url_fields.next().is_none();
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            let media_branches = branches
                .iter()
                .filter(|branch| !schema_is_null_only(branch))
                .collect::<Vec<_>>();
            return !media_branches.is_empty()
                && media_branches
                    .iter()
                    .all(|branch| schema_is_media_output(branch));
        }
    }
    schema
        .get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|branches| branches.iter().any(schema_is_media_output))
}

fn schema_has_only_type(schema: &Value, expected: &str) -> bool {
    schema_explicit_types(schema).is_some_and(|types| types.len() == 1 && types.contains(expected))
}

fn schema_may_be_null(schema: &Value) -> bool {
    schema.get("nullable").and_then(Value::as_bool) == Some(true)
        || schema.get("type").is_some_and(|kind| match kind {
            Value::String(kind) => kind == "null",
            Value::Array(kinds) => kinds.iter().any(|kind| kind.as_str() == Some("null")),
            _ => false,
        })
        || ["anyOf", "oneOf", "allOf"].into_iter().any(|keyword| {
            schema
                .get(keyword)
                .and_then(Value::as_array)
                .is_some_and(|branches| branches.iter().any(schema_may_be_null))
        })
}

fn schema_is_null_only(schema: &Value) -> bool {
    schema_explicit_types(schema).is_some_and(|types| types.len() == 1 && types.contains("null"))
}

fn validate_schema(schema: &Value, value: &Value, path: &str) -> Result<(), ProviderError> {
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for branch in all_of {
            validate_schema(branch, value, path)?;
        }
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array)
        && !options
            .iter()
            .any(|schema| validate_schema(schema, value, path).is_ok())
    {
        return Err(validation(format!(
            "{path} does not match any allowed schema"
        )));
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = options
            .iter()
            .filter(|schema| validate_schema(schema, value, path).is_ok())
            .count();
        if matching != 1 {
            return Err(validation(format!(
                "{path} must match exactly one allowed schema"
            )));
        }
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        return Err(validation(format!("{path} is not an allowed value")));
    }
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return Err(validation(format!("{path} must equal the schema constant")));
    }
    let kinds = schema
        .get("type")
        .map(|kind| match kind {
            Value::Array(values) => values.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
            Value::String(value) => vec![value.as_str()],
            _ => Vec::new(),
        })
        .unwrap_or_default();
    let type_ok = kinds.is_empty()
        || kinds.iter().any(|kind| match *kind {
            "null" => value.is_null(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            _ => true,
        });
    if !type_ok {
        return Err(validation(format!("{path} has the wrong JSON type")));
    }
    if let Some(number) = value.as_f64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
            && number < minimum
        {
            return Err(validation(format!("{path} must be at least {minimum}")));
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
            && number > maximum
        {
            return Err(validation(format!("{path} must be at most {maximum}")));
        }
        if let Some(minimum) = schema.get("exclusiveMinimum").and_then(Value::as_f64)
            && number <= minimum
        {
            return Err(validation(format!("{path} must be greater than {minimum}")));
        }
        if let Some(maximum) = schema.get("exclusiveMaximum").and_then(Value::as_f64)
            && number >= maximum
        {
            return Err(validation(format!("{path} must be less than {maximum}")));
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
            && length < minimum
        {
            return Err(validation(format!(
                "{path} must contain at least {minimum} characters"
            )));
        }
        if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64)
            && length > maximum
        {
            return Err(validation(format!(
                "{path} must contain at most {maximum} characters"
            )));
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(required) {
                return Err(validation(format!("{path}.{required} is required")));
            }
        }
        for (key, value) in object {
            if let Some(child_schema) = properties.and_then(|properties| properties.get(key)) {
                validate_schema(child_schema, value, &format!("{path}.{key}"))?;
                continue;
            }
            match schema.get("additionalProperties") {
                Some(Value::Bool(false)) => {
                    return Err(validation(format!(
                        "{path}.{key} is not accepted by this model"
                    )));
                }
                Some(child_schema @ Value::Object(_)) => {
                    validate_schema(child_schema, value, &format!("{path}.{key}"))?;
                }
                _ => {}
            }
        }
    }
    if let Some(values) = value.as_array()
        && let Some(items) = schema.get("items")
    {
        for (index, value) in values.iter().enumerate() {
            validate_schema(items, value, &format!("{path}[{index}]"))?;
        }
    }
    if let Some(values) = value.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
            && values.len() < minimum as usize
        {
            return Err(validation(format!(
                "{path} must contain at least {minimum} items"
            )));
        }
        if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
            && values.len() > maximum as usize
        {
            return Err(validation(format!(
                "{path} must contain at most {maximum} items"
            )));
        }
    }
    Ok(())
}

fn fal_submitted_job(endpoint_id: &str, payload: &Value) -> Result<VideoJob, ProviderError> {
    let request_id = payload
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| response_error("fal submission is missing request_id"))?;
    let status_url = payload
        .get("status_url")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let response_url = payload
        .get("response_url")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let locator = JobLocator::Fal {
        endpoint_id: endpoint_id.to_owned(),
        request_id: request_id.to_owned(),
        status_url,
        response_url,
    };
    locator
        .validate()
        .map_err(|error| response_error(error.to_string()))?;
    Ok(VideoJob {
        provider_id: ProviderId::fal(),
        id: request_id.to_owned(),
        status: JobStatus::Pending,
        polling_url: serde_json::to_string(&locator).unwrap_or_default(),
        generation_id: None,
        unsigned_urls: Vec::new(),
        usage: Map::new(),
        error: None,
        locator,
        artifacts: Vec::new(),
        raw: payload.clone(),
    })
}

fn fal_status_job(
    endpoint_id: &str,
    request_id: &str,
    payload: &Value,
) -> Result<VideoJob, ProviderError> {
    let raw_status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");
    let error = fal_payload_error(payload);
    let status = if error.is_some() {
        JobStatus::Failed
    } else {
        match raw_status.to_ascii_uppercase().as_str() {
            "IN_QUEUE" => JobStatus::Pending,
            "IN_PROGRESS" => JobStatus::InProgress,
            "COMPLETED" => JobStatus::Completed,
            "CANCELLED" | "CANCELED" => JobStatus::Cancelled,
            "FAILED" => JobStatus::Failed,
            _ => JobStatus::Unknown(raw_status.to_ascii_lowercase()),
        }
    };
    let locator = JobLocator::Fal {
        endpoint_id: endpoint_id.to_owned(),
        request_id: request_id.to_owned(),
        status_url: payload
            .get("status_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        response_url: payload
            .get("response_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    };
    Ok(VideoJob {
        provider_id: ProviderId::fal(),
        id: request_id.to_owned(),
        status,
        polling_url: serde_json::to_string(&locator).unwrap_or_default(),
        generation_id: None,
        unsigned_urls: Vec::new(),
        usage: payload
            .get("metrics")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        error,
        locator,
        artifacts: Vec::new(),
        raw: payload.clone(),
    })
}

fn fal_result_job(
    endpoint_id: &str,
    request_id: &str,
    status_payload: &Value,
    result_payload: &Value,
) -> Result<VideoJob, ProviderError> {
    let has_outer_video_field = result_payload
        .as_object()
        .is_some_and(|object| object.keys().any(|name| output_name_is_video(name)));
    let data = if has_outer_video_field {
        result_payload
    } else {
        result_payload.get("data").unwrap_or(result_payload)
    };
    let artifacts = extract_video_artifacts(data);
    if artifacts.len() != 1 {
        return Err(response_error(
            "fal completed without exactly one unambiguous video URL",
        ));
    }
    let locator = JobLocator::Fal {
        endpoint_id: endpoint_id.to_owned(),
        request_id: request_id.to_owned(),
        status_url: status_payload
            .get("status_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        response_url: status_payload
            .get("response_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    };
    let unsigned_urls = artifacts
        .iter()
        .map(|artifact| artifact.url.clone())
        .collect();
    Ok(VideoJob {
        provider_id: ProviderId::fal(),
        id: request_id.to_owned(),
        status: JobStatus::Completed,
        polling_url: serde_json::to_string(&locator).unwrap_or_default(),
        generation_id: None,
        unsigned_urls,
        usage: status_payload
            .get("metrics")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        error: None,
        locator,
        artifacts,
        raw: json!({"status": status_payload, "result": result_payload}),
    })
}

fn extract_video_artifacts(value: &Value) -> Vec<VideoArtifact> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let mut video_fields = object.iter().filter(|(name, _)| output_name_is_video(name));
    let Some((_, output)) = video_fields.next() else {
        return Vec::new();
    };
    if video_fields.next().is_some() {
        return Vec::new();
    }

    let (url, content_type) = match output {
        Value::String(url) => (url.clone(), None),
        Value::Object(file) => {
            let mut urls = ["url", "file_url", "video_url"]
                .into_iter()
                .filter_map(|name| file.get(name).and_then(Value::as_str));
            let Some(url) = urls.next() else {
                return Vec::new();
            };
            if urls.next().is_some() {
                return Vec::new();
            }
            let content_type = file
                .get("content_type")
                .or_else(|| file.get("mime_type"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            (url.to_owned(), content_type)
        }
        _ => return Vec::new(),
    };
    let Ok(mut artifact) = VideoArtifact::new(url, 0) else {
        return Vec::new();
    };
    artifact.content_type = content_type;
    vec![artifact]
}

fn bind_media_references(
    input: &mut Map<String, Value>,
    model: &VideoModel,
    request: &VideoRequest,
) -> Result<(), ProviderError> {
    let seedance = is_seedance_2_endpoint(&model.id);
    let total = request
        .frame_images
        .len()
        .saturating_add(request.input_references.len());
    let has_explicit_higher_maximum = !seedance
        && model.media_bindings.iter().any(|binding| {
            request
                .input_references
                .iter()
                .any(|reference| MediaKind::from(reference.kind) == binding.kind)
                && binding.cardinality == MediaCardinality::List
                && binding
                    .max_items
                    .is_some_and(|maximum| maximum > media_input_limit(binding.kind))
        });
    if total > MAX_INPUT_MEDIA && !has_explicit_higher_maximum {
        return Err(validation(format!(
            "fal requests accept at most {MAX_INPUT_MEDIA} input media items"
        )));
    }
    if seedance {
        let has_audio = request
            .input_references
            .iter()
            .any(|reference| MediaKind::from(reference.kind) == MediaKind::Audio);
        let has_visual = !request.frame_images.is_empty()
            || request.input_references.iter().any(|reference| {
                matches!(
                    MediaKind::from(reference.kind),
                    MediaKind::Image | MediaKind::Video
                )
            });
        if has_audio && !has_visual {
            return Err(validation(
                "Seedance 2.0 audio input requires at least one image or video input",
            ));
        }
    }

    for kind in [MediaKind::Image, MediaKind::Video, MediaKind::Audio] {
        let urls = request
            .input_references
            .iter()
            .filter(|reference| MediaKind::from(reference.kind) == kind)
            .map(|reference| reference.url.clone())
            .collect::<Vec<_>>();
        let count = urls.len()
            + if kind == MediaKind::Image {
                request.frame_images.len()
            } else {
                0
            };
        let application_maximum = media_input_limit(kind);
        if seedance && count > application_maximum {
            return Err(validation(format!(
                "fal requests accept at most {application_maximum} {kind} input item(s)"
            )));
        }

        let mut bindings = model
            .media_bindings
            .iter()
            .filter(|binding| binding.kind == kind);
        let binding = bindings.next();
        let ambiguous = bindings.next().is_some();
        if urls.is_empty() {
            if !ambiguous
                && let Some(binding) = binding
                && binding.required
                && binding.cardinality == MediaCardinality::List
                && binding.min_items == Some(0)
                && !input.contains_key(&binding.property_name)
            {
                input.insert(binding.property_name.clone(), Value::Array(Vec::new()));
            }
            continue;
        }

        let Some(binding) = binding else {
            return Err(validation(format!(
                "This fal model does not expose an unambiguous top-level {kind} input"
            )));
        };
        if ambiguous {
            return Err(validation(format!(
                "This fal model has ambiguous top-level {kind} inputs"
            )));
        }
        if input.contains_key(&binding.property_name) {
            return Err(validation(format!(
                "{} cannot be used by both frame media and {kind} references",
                binding.property_name
            )));
        }

        let value = match binding.cardinality {
            MediaCardinality::Scalar => {
                if urls.len() != 1 {
                    return Err(validation(format!(
                        "{} accepts exactly one {kind} input",
                        binding.property_name
                    )));
                }
                Value::String(urls.into_iter().next().unwrap_or_default())
            }
            MediaCardinality::List => {
                if binding
                    .min_items
                    .is_some_and(|minimum| urls.len() < minimum)
                {
                    return Err(validation(format!(
                        "{} requires at least {} {kind} input item(s)",
                        binding.property_name,
                        binding.min_items.unwrap_or_default()
                    )));
                }
                if let Some(maximum) = binding.max_items
                    && urls.len() > maximum
                {
                    return Err(validation(format!(
                        "{} accepts at most {maximum} {kind} input item(s)",
                        binding.property_name
                    )));
                }
                if binding.max_items.is_none() && count > application_maximum {
                    return Err(validation(format!(
                        "fal requests accept at most {application_maximum} {kind} input item(s)"
                    )));
                }
                Value::Array(urls.into_iter().map(Value::String).collect())
            }
        };
        input.insert(binding.property_name.clone(), value);
    }

    for binding in model
        .media_bindings
        .iter()
        .filter(|binding| binding.required)
    {
        if !input.contains_key(&binding.property_name) {
            return Err(validation(format!(
                "{} requires a {} input",
                binding.property_name, binding.kind
            )));
        }
    }
    Ok(())
}

fn apply_request_quote_uncertainty(
    request: &VideoRequest,
    basis: &mut String,
    confidence: &mut QuoteConfidence,
) {
    if !request.frame_images.is_empty() || !request.input_references.is_empty() {
        if *confidence == QuoteConfidence::Exact {
            *confidence = QuoteConfidence::Estimated;
        }
        basis.push_str("; input media may affect final provider usage");
    }
    if request
        .adapter_options
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|options| !options.is_empty())
    {
        if *confidence == QuoteConfidence::Exact {
            *confidence = QuoteConfidence::Estimated;
        }
        basis.push_str("; advanced provider-specific options may affect final usage");
    }
}

const fn media_input_limit(kind: MediaKind) -> usize {
    match kind {
        MediaKind::Image => MAX_IMAGE_INPUTS,
        MediaKind::Video => MAX_VIDEO_INPUTS,
        MediaKind::Audio => MAX_AUDIO_INPUTS,
    }
}

fn is_seedance_2_endpoint(endpoint_id: &str) -> bool {
    let endpoint_id = endpoint_id.to_ascii_lowercase();
    endpoint_id.starts_with("bytedance/seedance-2.0") || endpoint_id.contains("/seedance-2.0/")
}

fn validate_seedance_staged_media_constraints(
    draft: &crate::domain::GenerationDraft,
    staged_media: &[StagedMedia],
) -> Result<(), ProviderError> {
    if draft.media.len() != staged_media.len() {
        return Err(validation(
            "Every fal draft media item must have a matching staged result",
        ));
    }

    let seedance = is_seedance_2_endpoint(&draft.model);
    let mut local_video_bytes = 0u64;
    for (draft_media, staged) in draft.media.iter().zip(staged_media) {
        staged
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        if draft_media.role != staged.role {
            return Err(validation(
                "fal staged media order or role does not match the draft",
            ));
        }
        let MediaSource::LocalFile { .. } = &draft_media.source else {
            // Remote URL sizes cannot be verified without fetching them.
            continue;
        };
        let receipt = staged
            .receipt
            .as_ref()
            .ok_or_else(|| validation("Local fal media is missing its matching upload receipt"))?;
        if receipt.provider_id != ProviderId::fal() {
            return Err(validation(
                "Local fal media has a receipt from a different provider",
            ));
        }
        if !seedance {
            continue;
        }
        match draft_media.role.kind() {
            MediaKind::Image if receipt.size_bytes > SEEDANCE_MAX_IMAGE_BYTES => {
                return Err(validation(format!(
                    "Seedance 2.0 image inputs must be at most {} MB each",
                    SEEDANCE_MAX_IMAGE_BYTES / 1_000_000
                )));
            }
            MediaKind::Video => {
                local_video_bytes = local_video_bytes.saturating_add(receipt.size_bytes);
            }
            MediaKind::Audio if receipt.size_bytes > SEEDANCE_MAX_AUDIO_BYTES => {
                return Err(validation(format!(
                    "Seedance 2.0 audio inputs must be at most {} MB each",
                    SEEDANCE_MAX_AUDIO_BYTES / 1_000_000
                )));
            }
            MediaKind::Image | MediaKind::Audio => {}
        }
    }
    if seedance && local_video_bytes >= SEEDANCE_MAX_VIDEO_BYTES {
        return Err(validation(format!(
            "Seedance 2.0 local video inputs must total less than {} MB",
            SEEDANCE_MAX_VIDEO_BYTES / 1_000_000
        )));
    }
    Ok(())
}

fn insert_common(
    input: &mut Map<String, Value>,
    model: &VideoModel,
    canonical: &str,
    value: Value,
) {
    if let Some(name) = model.field_map.get(canonical) {
        input.insert(name.clone(), value);
    }
}

fn insert_optional(
    input: &mut Map<String, Value>,
    model: &VideoModel,
    canonical: &str,
    value: Option<Value>,
) -> Result<(), ProviderError> {
    if let Some(value) = value {
        let Some(name) = model.field_map.get(canonical) else {
            return Err(validation(format!(
                "This fal model does not support the requested {canonical} control"
            )));
        };
        input.insert(name.clone(), value);
    }
    Ok(())
}

fn fal_duration_value(model: &VideoModel, duration: u32) -> Value {
    let string_value = duration.to_string();
    let uses_string = model
        .input_schema
        .as_ref()
        .zip(model.field_map.get("duration"))
        .and_then(|(schema, name)| schema_properties(schema).get(name).cloned())
        .is_some_and(|schema| schema_accepts_string(&schema, &string_value));
    if uses_string {
        Value::String(string_value)
    } else {
        Value::from(duration)
    }
}

fn schema_accepts_string(schema: &Value, value: &str) -> bool {
    schema
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| values.iter().any(|item| item.as_str() == Some(value)))
        || schema.get("type").is_some_and(|kind| match kind {
            Value::String(kind) => kind == "string",
            Value::Array(kinds) => kinds.iter().any(|kind| kind.as_str() == Some("string")),
            _ => false,
        })
        || schema
            .get("allOf")
            .and_then(Value::as_array)
            .is_some_and(|schemas| {
                schemas
                    .iter()
                    .any(|schema| schema_accepts_string(schema, value))
            })
}

fn append_segments(mut url: Url, segments: &[&str]) -> Result<Url, ProviderError> {
    let mut path = url
        .path_segments_mut()
        .map_err(|_| configuration("Invalid fal base URL"))?;
    path.pop_if_empty();
    for segment in segments {
        path.push(segment);
    }
    drop(path);
    Ok(url)
}

fn validate_base(url: &Url, label: &str) -> Result<(), ProviderError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(configuration(format!("{label} must be a plain HTTPS URL")));
    }
    Ok(())
}

fn normalize_base(url: &mut Url) {
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
}

fn validate_download_url(url: &Url) -> Result<(), ProviderError> {
    validate_public_https_url(url.as_str(), "Video URL")
        .map_err(|_| unsafe_endpoint("Video URL must use a public HTTPS host without credentials"))
}

fn validate_upload_url(url: &Url) -> Result<(), ProviderError> {
    if url.fragment().is_some() {
        return Err(unsafe_endpoint(
            "fal CDN upload URL must not contain a fragment",
        ));
    }
    validate_public_https_url(url.as_str(), "fal CDN upload URL").map_err(|_| {
        unsafe_endpoint("fal CDN upload URL must use a public HTTPS host without credentials")
    })
}

fn upload_child_url(base: &Url, suffix: &str) -> Result<Url, ProviderError> {
    validate_upload_url(base)?;
    let mut url = base.clone();
    let path = format!("{}/{}", base.path().trim_end_matches('/'), suffix);
    url.set_path(&path);
    Ok(url)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalFileSnapshot {
    size_bytes: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

async fn local_file_snapshot(path: &Path) -> Result<LocalFileSnapshot, ProviderError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| validation(format!("Could not inspect local media: {error}")))?;
    if !metadata.is_file() {
        return Err(validation("Local media path is not a regular file"));
    }
    let modified = metadata
        .modified()
        .map_err(|error| validation(format!("Could not inspect local media mtime: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(LocalFileSnapshot {
            size_bytes: metadata.len(),
            modified,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(LocalFileSnapshot {
            size_bytes: metadata.len(),
            modified,
        })
    }
}

async fn ensure_local_file_unchanged(
    path: &Path,
    expected: &LocalFileSnapshot,
) -> Result<(), ProviderError> {
    let current = local_file_snapshot(path).await?;
    if &current != expected {
        return Err(validation(
            "Local media changed while fal was preparing its upload; Review again",
        ));
    }
    Ok(())
}

fn media_content_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("avif") => "image/avif",
        Some("bmp") => "image/bmp",
        Some("tif" | "tiff") => "image/tiff",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
    .into()
}

async fn collect_body(response: HttpResponse, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let mut body = response.body;
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| {
            ProviderError::new(
                ProviderId::fal(),
                ProviderErrorKind::Network,
                "Network connection interrupted while reading fal.ai's response",
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(response_error(
                "fal.ai response exceeded the safe size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn fal_error_message(payload: &Value) -> (String, Option<String>) {
    let error = payload.get("error").unwrap_or(payload);
    let code = error
        .get("type")
        .or_else(|| error.get("code"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let message = error
        .get("message")
        .or_else(|| payload.get("detail"))
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_default();
    (message, code)
}

fn fal_payload_error(payload: &Value) -> Option<String> {
    if let Some(error) = payload.get("error").filter(|value| !value.is_null()) {
        let message = error
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| error.to_string());
        if !message.is_empty() && message != "{}" {
            return Some(message);
        }
    }
    if let Some(error_type) = payload
        .get("error_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return Some(error_type.to_owned());
    }
    let (message, _) = fal_error_message(payload);
    (!message.is_empty()).then_some(message)
}

fn redact_value(value: &Value, secret: &str) -> Value {
    match value {
        Value::String(value) => Value::String(redact_text(value, secret)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_value(value, secret))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), redact_value(value, secret)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn redact_text(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_owned()
    } else {
        value.replace(secret, "[REDACTED]")
    }
}

fn decimal_value(value: &Value) -> Option<Decimal> {
    let text = value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string());
    Decimal::from_str(&text).ok()
}

fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn parse_retry_after(value: Option<&HeaderValue>) -> Option<Duration> {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn validation(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderId::fal(), ProviderErrorKind::Validation, message)
}

fn response_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderId::fal(), ProviderErrorKind::Response, message)
}

fn configuration(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderId::fal(), ProviderErrorKind::Configuration, message)
}

fn unsafe_endpoint(message: impl Into<String>) -> ProviderError {
    ProviderError::new(
        ProviderId::fal(),
        ProviderErrorKind::UnsafeEndpoint,
        message,
    )
}

fn download_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderId::fal(), ProviderErrorKind::Download, message)
}

fn submission_uncertain(message: impl Into<String>) -> ProviderError {
    ProviderError::new(
        ProviderId::fal(),
        ProviderErrorKind::SubmissionUncertain,
        message,
    )
}

fn submission_status_is_ambiguous(status: StatusCode) -> bool {
    !status.is_client_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status.canonical_reason().is_none()
}

fn submission_uncertain_from_http(mut error: ProviderError) -> ProviderError {
    let status = error.status_code.unwrap_or_default();
    let provider_message = error.message.trim();
    error.kind = ProviderErrorKind::SubmissionUncertain;
    error.message = format!(
        "fal.ai returned HTTP {status} after receiving the submission: {provider_message}. The request may exist; do not submit again until history is checked."
    );
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{GenerationDraft, InputReference, InputReferenceKind, MediaRole};
    use tempfile::tempdir;

    fn schema_model(endpoint_id: &str, category: &str, input: Value, output: Value) -> Value {
        let endpoint_path = format!("/{endpoint_id}");
        let result_path = format!("/{endpoint_id}/requests/{{request_id}}");
        let mut paths = Map::new();
        paths.insert(
            endpoint_path,
            json!({
                "post": {
                    "requestBody": {
                        "content": {"application/json": {"schema": input}}
                    },
                    "responses": {
                        "200": {
                            "content": {"application/json": {"schema": {
                                "type": "object",
                                "properties": {"request_id": {"type": "string"}}
                            }}}
                        }
                    }
                }
            }),
        );
        paths.insert(
            result_path,
            json!({
                "get": {
                    "responses": {
                        "201": {"content": {"application/json": {"schema": output}}}
                    }
                }
            }),
        );
        paths.insert(
            "/unrelated/utility".into(),
            json!({
                "post": {
                    "requestBody": {"content": {"application/json": {"schema": {
                        "type": "object",
                        "properties": {"text": {"type": "object"}}
                    }}}},
                    "responses": {"200": {"content": {"application/json": {"schema": {
                        "type": "object",
                        "properties": {"not_video": {"type": "string"}}
                    }}}}}
                }
            }),
        );
        json!({
            "endpoint_id": endpoint_id,
            "metadata": {
                "display_name": "Schema fixture",
                "description": "Offline fixture",
                "category": category,
                "status": "active"
            },
            "openapi": {"openapi": "3.0.4", "paths": paths}
        })
    }

    fn video_output_schema() -> Value {
        json!({
            "type": "object",
            "required": ["video"],
            "properties": {
                "video": {
                    "type": "object",
                    "required": ["url"],
                    "properties": {"url": {"type": "string"}}
                }
            }
        })
    }

    fn typed_binding_model(id: &str) -> VideoModel {
        VideoModel::from_provider_api(
            ProviderId::fal(),
            &json!({
                "id": id,
                "name": "Typed binding fixture",
                "input_modalities": ["image", "video", "audio"],
                "media_bindings": [
                    {
                        "kind": "image",
                        "property_name": "image_urls",
                        "cardinality": "list",
                        "max_items": 9
                    },
                    {
                        "kind": "video",
                        "property_name": "video_urls",
                        "cardinality": "list",
                        "max_items": 3
                    },
                    {
                        "kind": "audio",
                        "property_name": "audio_urls",
                        "cardinality": "list",
                        "max_items": 3
                    }
                ],
                "field_map": {"prompt": "prompt"}
            }),
        )
        .expect("typed binding fixture")
    }

    fn single_list_binding_model(
        id: &str,
        kind: MediaKind,
        property_name: &str,
        required: bool,
        min_items: Option<usize>,
        max_items: Option<usize>,
    ) -> VideoModel {
        let mut binding = json!({
            "kind": kind.as_str(),
            "property_name": property_name,
            "cardinality": "list",
            "required": required
        });
        if let Some(minimum) = min_items {
            binding["min_items"] = json!(minimum);
        }
        if let Some(maximum) = max_items {
            binding["max_items"] = json!(maximum);
        }
        VideoModel::from_provider_api(
            ProviderId::fal(),
            &json!({
                "id": id,
                "name": "List binding fixture",
                "input_modalities": [kind.as_str()],
                "media_bindings": [binding],
                "field_map": {"prompt": "prompt"}
            }),
        )
        .expect("list binding fixture")
    }

    #[test]
    fn discovery_includes_all_prompt_driven_video_categories() {
        assert_eq!(
            DISCOVERY_CATEGORIES,
            [
                "text-to-video",
                "image-to-video",
                "video-to-video",
                "audio-to-video"
            ]
        );
    }

    #[test]
    fn actual_inference_schema_normalizes_typed_top_level_media() {
        let raw = schema_model(
            "fal-ai/fixture/video-to-video",
            "video-to-video",
            json!({
                "type": "object",
                "required": ["video_url"],
                "x-fal-order-properties": ["prompt", "video_url", "image_urls", "audio_urls"],
                "properties": {
                    // The app always supplies a creative prompt; fal schemas
                    // may describe it as optional or conditionally required.
                    "prompt": {"type": "string"},
                    "video_url": {
                        "type": "string",
                        "title": "Source video",
                        "description": "Video to transform"
                    },
                    "image_urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": 9
                    },
                    "audio_urls": {
                        "anyOf": [
                            {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 3},
                            {"type": "null"}
                        ]
                    }
                }
            }),
            video_output_schema(),
        );
        let model = normalize_fal_model(&raw)
            .expect("valid schema")
            .expect("generative video model");
        assert_eq!(
            model.input_modalities,
            Some(vec![MediaKind::Image, MediaKind::Video, MediaKind::Audio])
        );
        assert_eq!(model.media_bindings.len(), 3);
        assert_eq!(model.media_bindings[0].property_name, "image_urls");
        assert_eq!(model.media_bindings[1].property_name, "video_url");
        assert_eq!(
            model.media_bindings[1].cardinality,
            MediaCardinality::Scalar
        );
        assert!(model.media_bindings[1].required);
        assert_eq!(
            model.media_bindings[1].title.as_deref(),
            Some("Source video")
        );
        assert_eq!(model.media_bindings[2].property_name, "audio_urls");
        assert_eq!(model.media_bindings[2].min_items, Some(1));
        assert_eq!(model.media_bindings[2].max_items, Some(3));
    }

    #[test]
    fn generated_audio_default_comes_from_the_boolean_schema() {
        for (schema_default, expected) in [
            (Some(true), Some(true)),
            (Some(false), Some(false)),
            (None, None),
        ] {
            let mut audio_schema = json!({"type": "boolean"});
            if let Some(value) = schema_default {
                audio_schema["default"] = json!(value);
            }
            let raw = schema_model(
                "fal-ai/fixture/audio-output",
                "text-to-video",
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string"},
                        "enable_audio": audio_schema
                    }
                }),
                video_output_schema(),
            );
            let model = normalize_fal_model(&raw)
                .expect("valid schema")
                .expect("video model");
            assert!(model.generated_audio.supported);
            assert_eq!(model.generated_audio.provider_default, expected);
        }
    }

    #[test]
    fn endpoint_alias_uses_the_selected_post_path_for_its_result_schema() {
        let endpoint_id = "bytedance/seedance-2.0/reference-to-video";
        let mut raw = schema_model(
            endpoint_id,
            "image-to-video",
            json!({
                "type": "object",
                "required": ["prompt"],
                "properties": {
                    "prompt": {"type": "string"},
                    "image_urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": 9
                    },
                    "video_urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": 3
                    },
                    "audio_urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": 3
                    }
                }
            }),
            video_output_schema(),
        );
        let paths = raw["openapi"]["paths"]
            .as_object_mut()
            .expect("fixture paths");
        let advertised_post = format!("/{endpoint_id}");
        let advertised_result = format!("/{endpoint_id}/requests/{{request_id}}");
        let post = paths.remove(&advertised_post).expect("advertised POST");
        let result = paths
            .remove(&advertised_result)
            .expect("advertised result GET");
        paths.remove("/unrelated/utility");
        paths.insert("/fal-ai/seedance-2/reference-to-video".into(), post);
        paths.insert(
            "/fal-ai/seedance-2/reference-to-video/requests/{request_id}".into(),
            result,
        );

        let model = normalize_fal_model(&raw)
            .expect("valid alias schema")
            .expect("alias model included");
        assert_eq!(model.id, endpoint_id);
        assert_eq!(
            model
                .media_bindings
                .iter()
                .map(|binding| binding.property_name.as_str())
                .collect::<Vec<_>>(),
            vec!["image_urls", "video_urls", "audio_urls"]
        );
    }

    #[test]
    fn only_required_scalar_video_outputs_enter_the_catalog() {
        let input = json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {"prompt": {"type": "string"}}
        });
        let required_scalar = schema_model(
            "fal-ai/fixture/required-scalar",
            "text-to-video",
            input.clone(),
            video_output_schema(),
        );
        assert!(
            normalize_fal_model(&required_scalar)
                .expect("schema")
                .is_some()
        );

        let optional_scalar = schema_model(
            "fal-ai/fixture/optional-scalar",
            "text-to-video",
            input.clone(),
            json!({
                "type": "object",
                "properties": {
                    "video": {
                        "type": "object",
                        "required": ["url"],
                        "properties": {"url": {"type": "string"}}
                    }
                }
            }),
        );
        assert!(
            normalize_fal_model(&optional_scalar)
                .expect("schema")
                .is_none()
        );

        let optional_nested_url = schema_model(
            "fal-ai/fixture/optional-nested-url",
            "text-to-video",
            input.clone(),
            json!({
                "type": "object",
                "required": ["video"],
                "properties": {
                    "video": {
                        "type": "object",
                        "properties": {"url": {"type": "string"}}
                    }
                }
            }),
        );
        assert!(
            normalize_fal_model(&optional_nested_url)
                .expect("schema")
                .is_none()
        );

        let optional_second_file_url = schema_model(
            "fal-ai/fixture/optional-second-file-url",
            "text-to-video",
            input.clone(),
            json!({
                "type": "object",
                "required": ["video"],
                "properties": {
                    "video": {
                        "type": "object",
                        "required": ["url"],
                        "properties": {
                            "url": {"type": "string"},
                            "file_url": {"type": "string"}
                        }
                    }
                }
            }),
        );
        assert!(
            normalize_fal_model(&optional_second_file_url)
                .expect("schema")
                .is_none()
        );

        let all_of_required_nested_url = schema_model(
            "fal-ai/fixture/all-of-required-nested-url",
            "text-to-video",
            input.clone(),
            json!({
                "type": "object",
                "required": ["video"],
                "properties": {
                    "video": {
                        "allOf": [
                            {
                                "type": "object",
                                "properties": {"url": {"type": "string"}}
                            },
                            {"required": ["url"]}
                        ]
                    }
                }
            }),
        );
        assert!(
            normalize_fal_model(&all_of_required_nested_url)
                .expect("schema")
                .is_some()
        );

        let generic_mime_output = schema_model(
            "fal-ai/fixture/generic-mime-output",
            "text-to-video",
            input.clone(),
            json!({
                "type": "object",
                "required": ["output"],
                "properties": {
                    "output": {
                        "type": "string",
                        "contentMediaType": "video/mp4"
                    }
                }
            }),
        );
        assert!(
            normalize_fal_model(&generic_mime_output)
                .expect("schema")
                .is_none()
        );

        let optional_second_video = schema_model(
            "fal-ai/fixture/optional-second-video",
            "text-to-video",
            input.clone(),
            json!({
                "type": "object",
                "required": ["video"],
                "properties": {
                    "video": {"type": "string"},
                    "preview_videos": {
                        "type": "array",
                        "items": {"type": "string"}
                    }
                }
            }),
        );
        assert!(
            normalize_fal_model(&optional_second_video)
                .expect("schema")
                .is_none()
        );

        let array_output = schema_model(
            "fal-ai/fixture/video-array",
            "text-to-video",
            input,
            json!({
                "type": "object",
                "required": ["videos"],
                "properties": {
                    "videos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["url"],
                            "properties": {"url": {"type": "string"}}
                        }
                    }
                }
            }),
        );
        assert!(
            normalize_fal_model(&array_output)
                .expect("schema")
                .is_none()
        );
    }

    #[test]
    fn nullable_or_untyped_result_shapes_are_excluded() {
        let input = json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {"prompt": {"type": "string"}}
        });
        let fixtures = [
            (
                "top-nullable",
                json!({
                    "type": "object",
                    "nullable": true,
                    "required": ["video"],
                    "properties": {"video": {"type": "string"}}
                }),
            ),
            (
                "top-object-null-union",
                json!({
                    "type": ["object", "null"],
                    "required": ["video"],
                    "properties": {"video": {"type": "string"}}
                }),
            ),
            (
                "top-any-of-null",
                json!({
                    "anyOf": [
                        {
                            "type": "object",
                            "required": ["video"],
                            "properties": {"video": {"type": "string"}}
                        },
                        {"type": "null"}
                    ]
                }),
            ),
            (
                "top-untyped",
                json!({
                    "required": ["video"],
                    "properties": {"video": {"type": "string"}}
                }),
            ),
            (
                "scalar-nullable",
                json!({
                    "type": "object",
                    "required": ["video"],
                    "properties": {"video": {"type": "string", "nullable": true}}
                }),
            ),
            (
                "file-nullable",
                json!({
                    "type": "object",
                    "required": ["video"],
                    "properties": {
                        "video": {
                            "type": "object",
                            "nullable": true,
                            "required": ["url"],
                            "properties": {"url": {"type": "string"}}
                        }
                    }
                }),
            ),
            (
                "file-url-nullable",
                json!({
                    "type": "object",
                    "required": ["video"],
                    "properties": {
                        "video": {
                            "type": "object",
                            "required": ["url"],
                            "properties": {
                                "url": {"type": "string", "nullable": true}
                            }
                        }
                    }
                }),
            ),
            (
                "file-untyped",
                json!({
                    "type": "object",
                    "required": ["video"],
                    "properties": {
                        "video": {
                            "required": ["url"],
                            "properties": {"url": {"type": "string"}}
                        }
                    }
                }),
            ),
        ];

        for (name, output) in fixtures {
            let raw = schema_model(
                &format!("fal-ai/fixture/{name}"),
                "text-to-video",
                input.clone(),
                output,
            );
            assert!(
                normalize_fal_model(&raw).expect("schema").is_none(),
                "admitted nullable or untyped output fixture {name}"
            );
        }
    }

    #[test]
    fn result_extraction_uses_only_the_one_top_level_video_output() {
        let status = json!({"status": "COMPLETED"});
        let generic = fal_result_job(
            "fal-ai/fixture/generic-output",
            "request-generic",
            &status,
            &json!({"output": "https://cdn.fal.media/signed?id=generic"}),
        )
        .expect_err("generic scalar output must not be inferred as video");
        assert_eq!(generic.kind, ProviderErrorKind::Response);

        let main_url = "https://cdn.fal.media/signed?id=main";
        let job = fal_result_job(
            "fal-ai/fixture/video-output",
            "request-video",
            &status,
            &json!({
                "video": {
                    "url": main_url,
                    "preview_url": "https://cdn.fal.media/signed?id=preview"
                },
                "metadata": {
                    "input_video_url": "https://cdn.fal.media/signed?id=input"
                },
                "data": {
                    "metadata": {
                        "input_video_url": "https://cdn.fal.media/signed?id=wrapped-input"
                    }
                }
            }),
        )
        .expect("one authoritative video output");
        assert_eq!(job.artifacts.len(), 1);
        assert_eq!(job.artifacts[0].url, main_url);
    }

    #[test]
    fn promptless_and_required_nested_media_utilities_are_excluded() {
        let promptless = schema_model(
            "fal-ai/fixture/lipsync",
            "audio-to-video",
            json!({
                "type": "object",
                "required": ["video_url", "audio_url"],
                "properties": {
                    "video_url": {"type": "string"},
                    "audio_url": {"type": "string"}
                }
            }),
            video_output_schema(),
        );
        assert!(normalize_fal_model(&promptless).expect("schema").is_none());

        let nested = schema_model(
            "fal-ai/fixture/nested",
            "video-to-video",
            json!({
                "type": "object",
                "required": ["prompt", "elements"],
                "properties": {
                    "prompt": {"type": "string"},
                    "elements": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["video_url"],
                            "properties": {"video_url": {"type": "string"}}
                        }
                    }
                }
            }),
            video_output_schema(),
        );
        assert!(normalize_fal_model(&nested).expect("schema").is_none());
    }

    #[test]
    fn optional_nested_and_ambiguous_optional_media_are_not_advertised() {
        let raw = schema_model(
            "fal-ai/fixture/optional-nested",
            "text-to-video",
            json!({
                "type": "object",
                "required": ["prompt"],
                "properties": {
                    "prompt": {"type": "string"},
                    "elements": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {"video_url": {"type": "string"}}
                        }
                    },
                    "video_url": {"type": "string"},
                    "control_video_url": {"type": "string"}
                }
            }),
            video_output_schema(),
        );
        let model = normalize_fal_model(&raw)
            .expect("schema")
            .expect("optional settings do not exclude generator");
        assert!(model.media_bindings.is_empty());
        assert_eq!(model.input_modalities, Some(Vec::new()));
    }

    #[test]
    fn required_ambiguous_same_kind_media_excludes_model() {
        let raw = schema_model(
            "fal-ai/fixture/ambiguous",
            "video-to-video",
            json!({
                "type": "object",
                "required": ["prompt", "video_url"],
                "properties": {
                    "prompt": {"type": "string"},
                    "video_url": {"type": "string"},
                    "control_video_url": {"type": "string"}
                }
            }),
            video_output_schema(),
        );
        assert!(normalize_fal_model(&raw).expect("schema").is_none());
    }

    #[test]
    fn typed_reference_lists_preserve_order_within_each_kind() {
        let model = typed_binding_model("fal-ai/fixture/mixed");
        let mut request =
            VideoRequest::for_provider(ProviderId::fal(), &model.id, "A mixed-media fixture")
                .expect("request");
        for (url, kind) in [
            (
                "https://media.example/video-2.mp4",
                InputReferenceKind::Video,
            ),
            (
                "https://media.example/audio-1.mp3",
                InputReferenceKind::Audio,
            ),
            (
                "https://media.example/video-1.mp4",
                InputReferenceKind::Video,
            ),
            (
                "https://media.example/image-1.png",
                InputReferenceKind::Image,
            ),
        ] {
            request
                .input_references
                .push(InputReference::with_kind(url, kind).expect("reference"));
        }
        let mut input = Map::new();
        bind_media_references(&mut input, &model, &request).expect("typed bindings");
        assert_eq!(
            input["video_urls"],
            json!([
                "https://media.example/video-2.mp4",
                "https://media.example/video-1.mp4"
            ])
        );
        assert_eq!(
            input["audio_urls"],
            json!(["https://media.example/audio-1.mp3"])
        );
        assert_eq!(
            input["image_urls"],
            json!(["https://media.example/image-1.png"])
        );
    }

    #[test]
    fn explicit_schema_maximum_overrides_the_non_seedance_kind_fallback() {
        let explicit = single_list_binding_model(
            "fal-ai/fixture/four-videos",
            MediaKind::Video,
            "video_urls",
            false,
            None,
            Some(4),
        );
        let absent = single_list_binding_model(
            "fal-ai/fixture/default-videos",
            MediaKind::Video,
            "video_urls",
            false,
            None,
            None,
        );
        let mut request = VideoRequest::for_provider(
            ProviderId::fal(),
            &explicit.id,
            "A four-video schema fixture",
        )
        .expect("request");
        for index in 0..4 {
            request.input_references.push(
                InputReference::with_kind(
                    format!("https://media.example/video-{index}.mp4"),
                    InputReferenceKind::Video,
                )
                .expect("video reference"),
            );
        }

        let mut input = Map::new();
        bind_media_references(&mut input, &explicit, &request)
            .expect("explicit maxItems must be authoritative");
        assert_eq!(input["video_urls"].as_array().map(Vec::len), Some(4));

        let error = bind_media_references(&mut Map::new(), &absent, &request)
            .expect_err("missing maxItems must use the three-video fallback");
        assert!(error.message.contains("at most 3 video"));
    }

    #[test]
    fn explicit_higher_schema_maximum_is_not_overridden_by_total_fallback() {
        let model = single_list_binding_model(
            "fal-ai/fixture/thirteen-images",
            MediaKind::Image,
            "image_urls",
            false,
            None,
            Some(20),
        );
        let mut request = VideoRequest::for_provider(
            ProviderId::fal(),
            &model.id,
            "A thirteen-image schema fixture",
        )
        .expect("request");
        for index in 0..13 {
            request.input_references.push(
                InputReference::with_kind(
                    format!("https://media.example/image-{index}.png"),
                    InputReferenceKind::Image,
                )
                .expect("image reference"),
            );
        }

        let mut input = Map::new();
        bind_media_references(&mut input, &model, &request)
            .expect("explicit maxItems must supersede the total fallback");
        assert_eq!(input["image_urls"].as_array().map(Vec::len), Some(13));
    }

    #[test]
    fn required_zero_minimum_list_is_bound_empty_without_weakening_nonempty_lists() {
        let empty_allowed = single_list_binding_model(
            "fal-ai/fixture/empty-audio-list",
            MediaKind::Audio,
            "audio_urls",
            true,
            Some(0),
            Some(3),
        );
        let request = VideoRequest::for_provider(
            ProviderId::fal(),
            &empty_allowed.id,
            "An empty-list schema fixture",
        )
        .expect("request");
        let mut input = Map::new();
        bind_media_references(&mut input, &empty_allowed, &request)
            .expect("required minItems zero list");
        assert_eq!(input["audio_urls"], json!([]));

        let nonempty_required = single_list_binding_model(
            "fal-ai/fixture/nonempty-audio-list",
            MediaKind::Audio,
            "audio_urls",
            true,
            Some(1),
            Some(3),
        );
        let error = bind_media_references(&mut Map::new(), &nonempty_required, &request)
            .expect_err("required minItems one list must remain required");
        assert!(error.message.contains("requires a audio input"));
    }

    #[test]
    fn nonempty_adapter_options_downgrade_an_exact_quote() {
        let mut request = VideoRequest::for_provider(
            ProviderId::fal(),
            "fal-ai/fixture/options",
            "An advanced-options quote fixture",
        )
        .expect("request");
        request.adapter_options = Some(json!({"provider_knob": 0.75}));
        let mut basis = "Advertised fal price per generation".to_owned();
        let mut confidence = QuoteConfidence::Exact;

        apply_request_quote_uncertainty(&request, &mut basis, &mut confidence);

        assert_eq!(confidence, QuoteConfidence::Estimated);
        assert!(basis.contains("advanced provider-specific options"));
    }

    #[test]
    fn seedance_audio_requires_visual_companion() {
        let model = typed_binding_model("bytedance/seedance-2.0/reference-to-video");
        let mut request =
            VideoRequest::for_provider(ProviderId::fal(), &model.id, "A Seedance fixture")
                .expect("request");
        request.input_references.push(
            InputReference::with_kind("https://media.example/audio.mp3", InputReferenceKind::Audio)
                .expect("audio"),
        );
        assert!(bind_media_references(&mut Map::new(), &model, &request).is_err());
        request.input_references.push(
            InputReference::with_kind("https://media.example/video.mp4", InputReferenceKind::Video)
                .expect("video"),
        );
        bind_media_references(&mut Map::new(), &model, &request)
            .expect("audio with visual companion");
    }

    #[test]
    fn staged_seedance_sizes_recheck_files_grown_after_early_validation() {
        let directory = tempdir().expect("temporary media");
        let path = directory.path().join("reference.mp3");
        std::fs::write(&path, b"ID3small fixture").expect("small MP3 fixture");
        let mut draft = GenerationDraft::new(
            ProviderId::fal(),
            "bytedance/seedance-2.0/reference-to-video",
            "A Seedance size fixture",
        )
        .expect("draft");
        draft
            .media
            .push(DraftMedia::local(&path, MediaRole::AudioInput));
        draft.validate().expect("initial small draft");
        let provider = FalProvider::from_key("fal-test-placeholder").expect("provider");
        provider
            .validate_draft_media_constraints(&draft)
            .expect("initial size check");

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("grow fixture");
        file.set_len(SEEDANCE_MAX_AUDIO_BYTES + 1)
            .expect("grow sparse fixture");
        drop(file);
        let size = std::fs::metadata(&path).expect("grown metadata").len();
        let uploaded_at = Utc::now();
        let receipt = UploadReceipt::new(
            ProviderId::fal(),
            "a".repeat(64),
            "https://v3.fal.media/files/fixture/reference.mp3",
            uploaded_at,
            uploaded_at + chrono::Duration::hours(1),
            Some("audio/mpeg".into()),
            size,
        )
        .expect("actual staged receipt");
        let staged = StagedMedia::uploaded(MediaRole::AudioInput, receipt).expect("staged media");

        let error = provider
            .validate_staged_media_constraints(&draft, &[staged])
            .expect_err("grown media must fail after staging");
        assert_eq!(error.kind, ProviderErrorKind::Validation);
        assert!(error.message.contains("at most 15 MB"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepted_snapshot_detects_same_size_atomic_file_replacement() {
        let directory = tempdir().expect("temporary media");
        let path = directory.path().join("reference.png");
        let replacement = directory.path().join("replacement.png");
        let bytes = b"\x89PNG\r\n\x1a\nfixture image bytes";
        std::fs::write(&path, bytes).expect("initial PNG fixture");
        std::fs::write(&replacement, bytes).expect("replacement PNG fixture");
        let snapshot = local_file_snapshot(&path).await.expect("accepted snapshot");

        std::fs::rename(&replacement, &path).expect("atomic replacement");

        let error = ensure_local_file_unchanged(&path, &snapshot)
            .await
            .expect_err("replacement inode must invalidate the accepted snapshot");
        assert!(error.message.contains("changed while fal was preparing"));
    }

    #[test]
    fn local_media_mime_set_is_deliberately_conservative() {
        assert_eq!(media_content_type(Path::new("fixture.mp4")), "video/mp4");
        assert_eq!(
            media_content_type(Path::new("fixture.mov")),
            "video/quicktime"
        );
        assert_eq!(media_content_type(Path::new("fixture.mp3")), "audio/mpeg");
        assert_eq!(media_content_type(Path::new("fixture.wav")), "audio/wav");
        for unsupported in ["m4v", "webm", "mkv", "m4a", "flac", "ogg"] {
            assert_eq!(
                media_content_type(Path::new(&format!("fixture.{unsupported}"))),
                "application/octet-stream"
            );
        }
    }

    #[test]
    fn upload_urls_reject_non_global_literal_hosts() {
        for value in [
            "https://0.1.2.3/upload",
            "https://100.64.0.1/upload",
            "https://198.18.0.1/upload",
            "https://192.0.2.1/upload",
            "https://224.0.0.1/upload",
            "https://240.0.0.1/upload",
            "https://[100::1]/upload",
            "https://[2001:2::1]/upload",
            "https://[2001:db8::1]/upload",
            "https://[3fff::1]/upload",
            "https://[fec0::1]/upload",
            "https://[::ffff:c0a8:101]/upload",
            "https://[4000::1]/upload",
            "https://[5f00::1]/upload",
            "https://[2001:10::1]/upload",
            "https://[64:ff9b::c0a8:101]/upload",
        ] {
            let url = Url::parse(value).expect("upload URL fixture");
            assert!(
                validate_upload_url(&url).is_err(),
                "accepted non-global upload URL {value}"
            );
            assert!(
                validate_download_url(&url).is_err(),
                "accepted non-global download URL {value}"
            );
        }

        for value in [
            "https://uploads.example/signed/object",
            "https://8.8.8.8/upload",
            "https://[2606:4700:4700::1111]/upload",
        ] {
            let url = Url::parse(value).expect("public upload URL fixture");
            validate_upload_url(&url)
                .unwrap_or_else(|error| panic!("rejected public upload URL {value}: {error}"));
            validate_download_url(&url)
                .unwrap_or_else(|error| panic!("rejected public download URL {value}: {error}"));
        }
    }

    #[test]
    fn generic_audio_property_is_not_the_output_audio_switch() {
        let field_map = common_field_map(&json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"},
                "audio": {"type": "string"}
            }
        }));
        assert!(!field_map.contains_key("generate_audio"));
    }

    #[test]
    fn schema_combinators_do_not_skip_sibling_constraints() {
        let any_of_with_minimum = json!({
            "type": "number",
            "minimum": 1,
            "anyOf": [{"type": "number"}, {"type": "string"}]
        });
        assert!(validate_schema(&any_of_with_minimum, &json!(0.5), "$input").is_err());

        let overlapping_one_of = json!({
            "oneOf": [{"type": "number"}, {"minimum": 0}]
        });
        assert!(validate_schema(&overlapping_one_of, &json!(1), "$input").is_err());
    }

    #[test]
    fn schema_additional_properties_is_enforced_without_declared_properties() {
        let closed = json!({"type": "object", "additionalProperties": false});
        assert!(validate_schema(&closed, &json!({"unexpected": true}), "$input").is_err());

        let typed = json!({
            "type": "object",
            "additionalProperties": {"type": "string"}
        });
        assert!(validate_schema(&typed, &json!({"valid": "yes"}), "$input").is_ok());
        assert!(validate_schema(&typed, &json!({"invalid": 1}), "$input").is_err());
    }
}
