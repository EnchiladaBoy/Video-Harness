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
    CostQuote, DraftMedia, JobLocator, JobStatus, MediaSource, ProviderDescriptor, ProviderId,
    QuoteConfidence, StagedMedia, UploadReceipt, VideoArtifact, VideoCatalog, VideoJob, VideoModel,
    VideoRequest,
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
        for category in ["text-to-video", "image-to-video"] {
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
            HeaderValue::from_str(&format!("video-harness/0.3 {DEFAULT_APP_TITLE}"))
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
        let reserved = model.field_map.values().cloned().collect::<BTreeSet<_>>();
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
        if !request.input_references.is_empty() {
            insert_optional(
                &mut input,
                model,
                "references",
                Some(Value::Array(
                    request
                        .input_references
                        .iter()
                        .map(|reference| Value::String(reference.url.clone()))
                        .collect(),
                )),
            )?;
        }
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
        let mut url =
            Url::parse(&artifact.url).map_err(|_| unsafe_endpoint("Invalid artifact URL"))?;
        let mut response = None;
        for redirect in 0..=MAX_REDIRECTS {
            let mut headers = HeaderMap::new();
            headers.insert(
                ACCEPT,
                HeaderValue::from_static("video/*,application/octet-stream"),
            );
            headers.insert(
                USER_AGENT,
                HeaderValue::from_static("video-harness/0.3 Video Harness"),
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
        media
            .validate()
            .map_err(|error| validation(error.to_string()))?;
        let MediaSource::LocalFile { path } = &media.source else {
            let MediaSource::RemoteUrl { url } = &media.source else {
                unreachable!()
            };
            return StagedMedia::remote(media.role, url.clone())
                .map_err(|error| validation(error.to_string()));
        };

        let (sha256, size_bytes) = media_sha256(path)
            .await
            .map_err(|error| validation(format!("Could not read local media: {error}")))?;
        if size_bytes == 0 {
            return Err(validation("Local media file is empty"));
        }
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
        let (amount, basis, confidence) =
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
        || !matches!(category, "text-to-video" | "image-to-video")
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
    let Some(input_schema) = openapi_input_schema(openapi) else {
        return Ok(None);
    };
    let Some(output_schema) = openapi_output_schema(openapi) else {
        return Ok(None);
    };
    let resolved_input = resolve_refs(&input_schema, openapi, 0)?;
    let resolved_output = resolve_refs(&output_schema, openapi, 0)?;
    if !schema_has_video_url(&resolved_output) {
        return Ok(None);
    }
    let field_map = common_field_map(&resolved_input);
    if !field_map.contains_key("prompt") {
        return Ok(None);
    }
    let properties = schema_properties(&resolved_input);
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
    let normalized = json!({
        "id": endpoint_id,
        "name": metadata.and_then(|value| value.get("display_name")).and_then(Value::as_str).unwrap_or(endpoint_id),
        "description": metadata.and_then(|value| value.get("description")).and_then(Value::as_str).unwrap_or_default(),
        "supported_resolutions": enum_strings("resolution"),
        "supported_aspect_ratios": enum_strings("aspect_ratio"),
        "supported_sizes": enum_strings("size"),
        "supported_durations": durations,
        "supported_frame_images": supported_frame_images,
        "generate_audio": field_map.contains_key("generate_audio"),
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

fn openapi_input_schema(openapi: &Value) -> Option<Value> {
    openapi
        .get("paths")?
        .as_object()?
        .values()
        .find_map(|path| {
            path.get("post")?
                .get("requestBody")?
                .get("content")?
                .get("application/json")?
                .get("schema")
                .cloned()
        })
        .or_else(|| openapi.pointer("/components/schemas/Input").cloned())
}

fn openapi_output_schema(openapi: &Value) -> Option<Value> {
    openapi
        .get("paths")?
        .as_object()?
        .values()
        .find_map(|path| {
            let responses = path.get("post")?.get("responses")?.as_object()?;
            ["200", "201"]
                .into_iter()
                .find_map(|code| responses.get(code))?
                .get("content")?
                .get("application/json")?
                .get("schema")
                .cloned()
        })
        .or_else(|| openapi.pointer("/components/schemas/Output").cloned())
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
        (
            "generate_audio",
            &["generate_audio", "enable_audio", "audio"],
        ),
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

fn schema_has_video_url(schema: &Value) -> bool {
    fn walk(value: &Value, key: Option<&str>, seen_video: bool) -> bool {
        match value {
            Value::Object(object) => object.iter().any(|(child_key, value)| {
                let video = seen_video
                    || child_key.to_ascii_lowercase().contains("video")
                    || object
                        .get("contentMediaType")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.starts_with("video/"));
                let url = matches!(child_key.as_str(), "url" | "video_url" | "file_url")
                    && value
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "string");
                (url && (video || key.is_some_and(|key| key.contains("video"))))
                    || walk(value, Some(child_key), video)
            }),
            Value::Array(values) => values.iter().any(|value| walk(value, key, seen_video)),
            _ => false,
        }
    }
    walk(schema, None, false)
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
    let data = result_payload.get("data").unwrap_or(result_payload);
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
    fn collect(value: &Value, video_context: bool, output: &mut Vec<(String, Option<String>)>) {
        match value {
            Value::Object(object) => {
                let content_type = object
                    .get("content_type")
                    .or_else(|| object.get("mime_type"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let context = video_context
                    || content_type
                        .as_deref()
                        .is_some_and(|value| value.starts_with("video/"));
                if let Some(url) = object.get("url").and_then(Value::as_str) {
                    let extension = url.split('?').next().is_some_and(|value| {
                        [".mp4", ".webm", ".mov", ".mkv"]
                            .iter()
                            .any(|ext| value.ends_with(ext))
                    });
                    if context || extension {
                        output.push((url.to_owned(), content_type));
                    }
                }
                for (key, value) in object {
                    collect(
                        value,
                        context || key.to_ascii_lowercase().contains("video"),
                        output,
                    );
                }
            }
            Value::Array(values) => {
                for value in values {
                    collect(value, video_context, output);
                }
            }
            Value::String(url) if video_context && url.starts_with("https://") => {
                output.push((url.clone(), None));
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    collect(value, false, &mut values);
    values.sort();
    values.dedup_by(|left, right| left.0 == right.0);
    values
        .into_iter()
        .enumerate()
        .filter_map(|(index, (url, content_type))| {
            let mut artifact = VideoArtifact::new(url, index).ok()?;
            artifact.content_type = content_type;
            Some(artifact)
        })
        .collect()
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
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsafe_endpoint(
            "Video URL must be public HTTPS without credentials",
        ));
    }
    Ok(())
}

fn validate_upload_url(url: &Url) -> Result<(), ProviderError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(unsafe_endpoint(
            "fal CDN upload URL must be public HTTPS without credentials",
        ));
    }
    let host = url.host_str().unwrap_or_default();
    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.ends_with(".local")
    {
        return Err(unsafe_endpoint("fal CDN returned a local upload host"));
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        let unsafe_address = match address {
            std::net::IpAddr::V4(address) => {
                address.is_private()
                    || address.is_loopback()
                    || address.is_link_local()
                    || address.is_unspecified()
                    || address.is_broadcast()
                    || address.is_documentation()
            }
            std::net::IpAddr::V6(address) => {
                address.is_loopback()
                    || address.is_unspecified()
                    || address.is_unique_local()
                    || address.is_unicast_link_local()
            }
        };
        if unsafe_address {
            return Err(unsafe_endpoint("fal CDN returned a non-public upload host"));
        }
    }
    Ok(())
}

fn upload_child_url(base: &Url, suffix: &str) -> Result<Url, ProviderError> {
    validate_upload_url(base)?;
    let mut url = base.clone();
    let path = format!("{}/{}", base.path().trim_end_matches('/'), suffix);
    url.set_path(&path);
    Ok(url)
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
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
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
