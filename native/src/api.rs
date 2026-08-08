//! Asynchronous, origin-safe OpenRouter video API client.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    LOCATION, RETRY_AFTER, USER_AGENT,
};
use reqwest::{Method, StatusCode};
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use url::Url;

use crate::config::partial_path;
use crate::domain::{
    DomainError, JobLocator, VideoCatalog, VideoJob, VideoRequest, ip_address_is_non_public,
    validate_public_https_url,
};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_APP_TITLE: &str = "Video Harness";
pub const MAX_REDIRECTS: usize = 5;
pub const MAX_VIDEO_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const DOWNLOAD_SPACE_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 1024 * 1024;
const OPENROUTER_TYPED_REFERENCE_MODELS: &[&str] =
    &["bytedance/seedance-2.0", "bytedance/seedance-2.0-fast"];

fn retryable_statuses() -> BTreeSet<u16> {
    [408, 425, 429, 500, 502, 503, 504].into_iter().collect()
}

fn catalog_needs_input_modality_enrichment(payload: &Value) -> bool {
    payload
        .get("data")
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models.iter().any(|model| {
                model
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(is_typed_reference_model)
            })
        })
}

fn is_typed_reference_model(model_id: &str) -> bool {
    OPENROUTER_TYPED_REFERENCE_MODELS.contains(&model_id)
}

fn enrich_video_model_input_modalities(video_catalog: &mut Value, model_catalog: &Value) {
    let advertised = model_catalog
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            if !is_typed_reference_model(id) {
                return None;
            }
            let modalities = model
                .get("architecture")
                .and_then(|architecture| architecture.get("input_modalities"))
                .or_else(|| model.get("input_modalities"))
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .filter(|modality| matches!(*modality, "image" | "video" | "audio"))
                .map(|modality| Value::String(modality.to_owned()))
                .collect::<Vec<_>>();
            Some((id.to_owned(), modalities))
        })
        .collect::<BTreeMap<_, _>>();

    let Some(models) = video_catalog.get_mut("data").and_then(Value::as_array_mut) else {
        return;
    };
    for model in models {
        let Some(object) = model.as_object_mut() else {
            continue;
        };
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(modalities) = advertised.get(id) else {
            continue;
        };
        object.insert("input_modalities".into(), Value::Array(modalities.clone()));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    Authentication,
    InsufficientCredits,
    RequestValidation,
    ContentPolicy,
    ResourceNotFound,
    RateLimit,
    Provider,
    Network,
    SubmissionUncertain,
    ResponseFormat,
    UnsafeUrl,
    Download,
    Configuration,
}

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
    pub status_code: Option<u16>,
    pub code: Option<String>,
    pub details: Map<String, Value>,
    pub retry_after: Option<Duration>,
}

impl ApiError {
    fn simple(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status_code: None,
            code: None,
            details: Map::new(),
            retry_after: None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        self.kind != ApiErrorKind::SubmissionUncertain
            && (self.kind == ApiErrorKind::Network
                || self
                    .status_code
                    .is_some_and(|status| retryable_statuses().contains(&status)))
    }
}

pub(crate) struct OwnedPartialFile {
    path: PathBuf,
}

impl OwnedPartialFile {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for OwnedPartialFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl From<DomainError> for ApiError {
    fn from(error: DomainError) -> Self {
        Self::simple(ApiErrorKind::RequestValidation, error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyInfo {
    pub label: String,
    pub limit: Option<Decimal>,
    pub limit_remaining: Option<Decimal>,
    pub limit_reset: Option<String>,
    pub usage: Option<Decimal>,
    pub is_free_tier: bool,
    pub expires_at: Option<String>,
    pub raw: Value,
}

impl KeyInfo {
    pub fn from_api(payload: &Value) -> Result<Self, ApiError> {
        let data = payload
            .as_object()
            .and_then(|object| object.get("data"))
            .and_then(Value::as_object)
            .ok_or_else(|| {
                ApiError::simple(
                    ApiErrorKind::ResponseFormat,
                    "Key validation response did not contain a data object",
                )
            })?;
        Ok(Self {
            label: data
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            limit: data.get("limit").and_then(value_as_decimal),
            limit_remaining: data.get("limit_remaining").and_then(value_as_decimal),
            limit_reset: optional_string(data.get("limit_reset")),
            usage: data.get("usage").and_then(value_as_decimal),
            is_free_tier: data
                .get("is_free_tier")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            expires_at: optional_string(data.get("expires_at")),
            raw: Value::Object(data.clone()),
        })
    }
}

fn value_as_decimal(value: &Value) -> Option<Decimal> {
    if value.is_null() || value.is_boolean() {
        return None;
    }
    let text = value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string());
    Decimal::from_str(&text).ok()
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| {
        if value.is_null() {
            None
        } else {
            let text = value
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string());
            (!text.is_empty()).then_some(text)
        }
    })
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub base_url: Url,
    pub http_referer: Option<String>,
    pub app_title: String,
    pub timeout: Duration,
    pub max_retries: usize,
    pub backoff_base: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            base_url: Url::parse(DEFAULT_BASE_URL).expect("the built-in API URL is valid"),
            http_referer: None,
            app_title: DEFAULT_APP_TITLE.into(),
            timeout: Duration::from_secs(60),
            max_retries: 3,
            backoff_base: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    pub written: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Error)]
#[error("HTTP transport failed")]
pub struct TransportError;

pub type HttpBody = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send + 'static>>;

pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub final_url: Url,
    pub body: HttpBody,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("final_url", &self.final_url)
            .field("body", &"<stream>")
            .finish()
    }
}

impl HttpResponse {
    pub fn from_bytes(
        status: StatusCode,
        final_url: Url,
        headers: HeaderMap,
        body: impl Into<Bytes>,
    ) -> Self {
        let body = body.into();
        Self {
            status,
            headers,
            final_url,
            body: Box::pin(futures_util::stream::once(async move { Ok(body) })),
        }
    }

    pub fn from_json(
        status: StatusCode,
        final_url: Url,
        value: &Value,
    ) -> Result<Self, serde_json::Error> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(Self::from_bytes(
            status,
            final_url,
            headers,
            serde_json::to_vec(value)?,
        ))
    }
}

#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub json_body: Option<Value>,
    /// Streamed media transfers use the client's connect and per-read idle
    /// deadlines, but no short total request deadline.
    pub stream_response: bool,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let safe_headers = self
            .headers
            .keys()
            .map(HeaderName::as_str)
            .collect::<Vec<_>>();
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("header_names", &safe_headers)
            .field("json_body", &self.json_body)
            .field("stream_response", &self.stream_response)
            .finish()
    }
}

#[async_trait]
pub trait HttpExecutor: Send + Sync {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

pub struct ReqwestExecutor {
    client: reqwest::Client,
    request_timeout: Duration,
}

/// Resolve names at connection time and expose only globally routable
/// addresses to reqwest. This closes the gap between lexical URL validation
/// and DNS resolution without changing TLS SNI or following redirects
/// implicitly.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PublicDnsResolver;

impl Resolve for PublicDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            let addresses = public_socket_addresses(addresses)?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

fn public_socket_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
    let addresses = addresses
        .into_iter()
        .filter(|address| !ip_address_is_non_public(address.ip()))
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "DNS name did not resolve to a public address",
        )));
    }
    Ok(addresses)
}

impl ReqwestExecutor {
    pub fn new(timeout: Duration) -> Result<Self, ApiError> {
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            // A system proxy would resolve the destination itself and bypass
            // the public-address-only resolver used to prevent DNS rebinding.
            .no_proxy()
            .dns_resolver(PublicDnsResolver)
            .build()
            .map_err(|_| {
                ApiError::simple(
                    ApiErrorKind::Configuration,
                    "Could not initialize the HTTPS client",
                )
            })?;
        Ok(Self {
            client,
            request_timeout: timeout,
        })
    }
}

#[async_trait]
impl HttpExecutor for ReqwestExecutor {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let stream_response = request.stream_response;
        let mut builder = self
            .client
            .request(request.method, request.url)
            .headers(request.headers);
        if !stream_response {
            builder = builder.timeout(self.request_timeout);
        }
        if let Some(body) = request.json_body {
            builder = builder.json(&body);
        }
        let response = builder.send().await.map_err(|_| TransportError)?;
        let status = response.status();
        let headers = response.headers().clone();
        let final_url = response.url().clone();
        let body = response
            .bytes_stream()
            .map(|item| item.map_err(|_| TransportError));
        Ok(HttpResponse {
            status,
            headers,
            final_url,
            body: Box::pin(body),
        })
    }
}

pub struct OpenRouterClient {
    api_key: SecretString,
    options: ClientOptions,
    executor: Arc<dyn HttpExecutor>,
    api_origin: (String, String, u16),
    api_path: String,
}

impl fmt::Debug for OpenRouterClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterClient")
            .field("api_key", &"[REDACTED]")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl OpenRouterClient {
    pub fn new(api_key: SecretString) -> Result<Self, ApiError> {
        Self::with_options(api_key, ClientOptions::default())
    }

    pub fn from_key(api_key: impl Into<String>) -> Result<Self, ApiError> {
        Self::new(SecretString::from(api_key.into()))
    }

    pub fn with_options(api_key: SecretString, options: ClientOptions) -> Result<Self, ApiError> {
        let executor = Arc::new(ReqwestExecutor::new(options.timeout)?);
        Self::with_executor(api_key, options, executor)
    }

    pub fn with_executor(
        api_key: SecretString,
        mut options: ClientOptions,
        executor: Arc<dyn HttpExecutor>,
    ) -> Result<Self, ApiError> {
        let normalized_key = api_key.expose_secret().trim().to_owned();
        if normalized_key.is_empty() {
            return Err(ApiError::simple(
                ApiErrorKind::Configuration,
                "An OpenRouter API key is required",
            ));
        }
        if normalized_key.chars().any(char::is_whitespace) {
            return Err(ApiError::simple(
                ApiErrorKind::Configuration,
                "OpenRouter API keys cannot contain whitespace",
            ));
        }
        validate_base_url(&options.base_url)?;
        let path = options.base_url.path().trim_end_matches('/').to_owned();
        options.base_url.set_path(&path);
        let api_origin = origin(&options.base_url).ok_or_else(|| {
            ApiError::simple(ApiErrorKind::Configuration, "base_url must be an HTTPS URL")
        })?;
        validate_header_option(options.http_referer.as_deref(), "HTTP-Referer")?;
        validate_header_option(Some(&options.app_title), "X-Title")?;
        Ok(Self {
            api_key: SecretString::from(normalized_key),
            api_path: path,
            options,
            executor,
            api_origin,
        })
    }

    pub fn options(&self) -> &ClientOptions {
        &self.options
    }

    pub fn is_openrouter_api_url(&self, url: &Url) -> bool {
        let path = url.path().trim_end_matches('/');
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && origin(url).as_ref() == Some(&self.api_origin)
            && (path == self.api_path || path.starts_with(&format!("{}/", self.api_path)))
    }

    pub async fn validate_key(&self) -> Result<KeyInfo, ApiError> {
        let payload = self
            .request_json(Method::GET, self.api_url("key")?, true, true, None)
            .await?;
        KeyInfo::from_api(&redact_value(&payload, self.api_key.expose_secret()))
    }

    pub async fn list_video_models(&self) -> Result<VideoCatalog, ApiError> {
        // Catalog metadata is public, so no credential is attached.
        let mut payload = self
            .request_json(
                Method::GET,
                self.api_url("videos/models")?,
                false,
                true,
                None,
            )
            .await?;
        // The dedicated generation catalog does not currently expose input
        // modalities. Enrich only models whose video endpoint is documented
        // to accept typed media, using the public general model catalog. A
        // failed enrichment deliberately leaves those capabilities unknown;
        // callers can still use text/image generation but must fail closed for
        // video and audio references.
        if catalog_needs_input_modality_enrichment(&payload) {
            let mut models_url = self.api_url("models")?;
            models_url
                .query_pairs_mut()
                .append_pair("output_modalities", "video");
            if let Ok(modality_catalog) = self
                .request_json(Method::GET, models_url, false, true, None)
                .await
            {
                enrich_video_model_input_modalities(&mut payload, &modality_catalog);
            }
        }
        VideoCatalog::from_api(&payload).map_err(|_| {
            ApiError::simple(
                ApiErrorKind::ResponseFormat,
                "Model catalog response does not contain a data list",
            )
        })
    }

    /// Submit exactly one paid POST. Ambiguous failures are never retried.
    pub async fn submit(&self, request: &VideoRequest) -> Result<VideoJob, ApiError> {
        let payload = self
            .request_json(
                Method::POST,
                self.api_url("videos")?,
                true,
                false,
                Some(request.to_payload()?),
            )
            .await?;
        let safe_payload = redact_value(&payload, self.api_key.expose_secret());
        let job = VideoJob::from_api(&safe_payload).map_err(|_| {
            ApiError::simple(
                ApiErrorKind::SubmissionUncertain,
                "OpenRouter returned an invalid accepted-job response. The job may exist; do not submit again.",
            )
        })?;
        if job.id.is_empty() || job.polling_url.is_empty() {
            return Err(ApiError::simple(
                ApiErrorKind::SubmissionUncertain,
                "OpenRouter's accepted-job response is missing id or polling_url. The job may exist; do not submit again.",
            ));
        }
        let polling_url = self.resolve_polling_url(&job.polling_url).map_err(|_| {
            ApiError::simple(
                ApiErrorKind::SubmissionUncertain,
                "OpenRouter returned an unsafe accepted-job locator. The job may exist; do not submit again.",
            )
        })?;
        let expected_url = self.job_url(&job.id).map_err(|_| {
            ApiError::simple(
                ApiErrorKind::SubmissionUncertain,
                "OpenRouter returned an invalid accepted-job id. The job may exist; do not submit again.",
            )
        })?;
        if polling_url != expected_url {
            return Err(ApiError::simple(
                ApiErrorKind::SubmissionUncertain,
                "OpenRouter returned an accepted-job id that does not match its polling locator. The job may exist; do not submit again.",
            ));
        }
        Ok(job)
    }

    pub async fn poll(&self, polling_url_or_job_id: &str) -> Result<VideoJob, ApiError> {
        let url = self.resolve_polling_url(polling_url_or_job_id)?;
        let payload = self
            .request_json(Method::GET, url.clone(), true, true, None)
            .await?;
        let safe_payload = redact_value(&payload, self.api_key.expose_secret());
        let mut job = VideoJob::from_api(&safe_payload).map_err(|_| {
            ApiError::simple(
                ApiErrorKind::ResponseFormat,
                "OpenRouter returned an invalid video job",
            )
        })?;
        if job.id.is_empty() {
            return Err(ApiError::simple(
                ApiErrorKind::ResponseFormat,
                "Video status response is missing the job id",
            ));
        }
        let expected_url = self.job_url(&job.id).map_err(|_| {
            ApiError::simple(
                ApiErrorKind::ResponseFormat,
                "Video status response contains an invalid job id",
            )
        })?;
        let reported_url = (!job.polling_url.is_empty())
            .then(|| self.resolve_polling_url(&job.polling_url))
            .transpose()
            .map_err(|_| {
                ApiError::simple(
                    ApiErrorKind::ResponseFormat,
                    "Video status response contains an invalid polling locator",
                )
            })?;
        if url != expected_url || reported_url.is_some_and(|reported| reported != expected_url) {
            return Err(ApiError::simple(
                ApiErrorKind::ResponseFormat,
                "Video status response does not match the requested job",
            ));
        }
        if job.polling_url.is_empty() {
            let polling_url = expected_url.to_string();
            job.polling_url = polling_url.clone();
            job.locator = JobLocator::OpenRouter { polling_url };
        }
        Ok(job)
    }

    pub fn content_url(&self, job_id: &str, index: usize) -> Result<Url, ApiError> {
        let mut url = self.job_url(job_id)?;
        url.path_segments_mut()
            .map_err(|_| ApiError::simple(ApiErrorKind::Configuration, "Invalid API base URL"))?
            .push("content");
        url.query_pairs_mut()
            .append_pair("index", &index.to_string());
        Ok(url)
    }

    fn job_url(&self, job_id: &str) -> Result<Url, ApiError> {
        let job_id = job_id.trim();
        if job_id.is_empty() || job_id.chars().any(char::is_control) {
            return Err(ApiError::simple(
                ApiErrorKind::RequestValidation,
                "job_id is required",
            ));
        }
        let mut url = self.options.base_url.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                ApiError::simple(ApiErrorKind::Configuration, "Invalid API base URL")
            })?;
            segments.pop_if_empty();
            segments.push("videos");
            segments.push(job_id);
        }
        Ok(url)
    }

    pub async fn download(
        &self,
        url: &Url,
        destination: &Path,
        progress: Option<mpsc::UnboundedSender<DownloadProgress>>,
    ) -> Result<PathBuf, ApiError> {
        validate_download_url(url)?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                ApiError::simple(
                    ApiErrorKind::Download,
                    format!("Could not create the video directory: {error}"),
                )
            })?;
        }
        let partial = partial_path(destination);
        if path_exists(destination).await || path_exists(&partial).await {
            return Err(ApiError::simple(
                ApiErrorKind::Download,
                format!(
                    "Refusing to overwrite an existing download: {}",
                    destination.display()
                ),
            ));
        }

        let mut last_error = None;
        for attempt in 0..=self.options.max_retries {
            match self.download_once(url, &partial, progress.as_ref()).await {
                Ok(partial_guard) => {
                    let metadata = match tokio::fs::metadata(&partial).await {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            return Err(ApiError::simple(
                                ApiErrorKind::Download,
                                format!("Could not inspect the downloaded video: {error}"),
                            ));
                        }
                    };
                    if metadata.len() == 0 {
                        return Err(ApiError::simple(
                            ApiErrorKind::Download,
                            "OpenRouter returned an empty video file",
                        ));
                    }
                    match tokio::fs::hard_link(&partial, destination).await {
                        Ok(()) => {
                            drop(partial_guard);
                            return Ok(destination.to_owned());
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            return Err(ApiError::simple(
                                ApiErrorKind::Download,
                                format!(
                                    "Refusing to overwrite an existing video: {}",
                                    destination.display()
                                ),
                            ));
                        }
                        Err(error) => {
                            return Err(ApiError::simple(
                                ApiErrorKind::Download,
                                format!("Could not save video: {error}"),
                            ));
                        }
                    }
                }
                Err(error) => {
                    let retryable = error.is_retryable();
                    if !retryable || attempt == self.options.max_retries {
                        return Err(error);
                    }
                    let delay = self.delay(attempt, error.retry_after);
                    last_error = Some(error);
                    tokio::time::sleep(delay).await;
                }
            }
        }
        Err(last_error
            .unwrap_or_else(|| ApiError::simple(ApiErrorKind::Download, "Video download failed")))
    }

    async fn download_once(
        &self,
        url: &Url,
        partial: &Path,
        progress: Option<&mpsc::UnboundedSender<DownloadProgress>>,
    ) -> Result<OwnedPartialFile, ApiError> {
        let response = self.open_download_response(url).await?;
        if !response.status.is_success() {
            return Err(self.error_from_http(response).await);
        }
        let total = response
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if total.is_some_and(|total| total > MAX_VIDEO_DOWNLOAD_BYTES) {
            return Err(ApiError::simple(
                ApiErrorKind::Download,
                "Video download exceeds the 4 GiB safety limit",
            ));
        }
        ensure_download_space(partial, total)
            .map_err(|message| ApiError::simple(ApiErrorKind::Download, message))?;
        let mut body = response.body;
        let partial_guard: OwnedPartialFile;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial)
            .await
            .map_err(|error| {
                ApiError::simple(
                    ApiErrorKind::Download,
                    format!("Could not create the partial video file: {error}"),
                )
            })?;
        partial_guard = OwnedPartialFile::new(partial.to_owned());
        let mut written = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|_| {
                ApiError::simple(
                    ApiErrorKind::Network,
                    "Network connection interrupted while downloading video",
                )
            })?;
            if chunk.is_empty() {
                continue;
            }
            written = checked_download_length(written, chunk.len()).ok_or_else(|| {
                ApiError::simple(
                    ApiErrorKind::Download,
                    "Video download exceeds the 4 GiB safety limit",
                )
            })?;
            file.write_all(&chunk).await.map_err(|error| {
                ApiError::simple(
                    ApiErrorKind::Download,
                    format!("Could not save video: {error}"),
                )
            })?;
            if let Some(sender) = progress {
                let _ = sender.send(DownloadProgress { written, total });
            }
        }
        file.flush().await.map_err(|error| {
            ApiError::simple(
                ApiErrorKind::Download,
                format!("Could not finish saving video: {error}"),
            )
        })?;
        file.sync_all().await.map_err(|error| {
            ApiError::simple(
                ApiErrorKind::Download,
                format!("Could not durably save video: {error}"),
            )
        })?;
        if written == 0 {
            return Err(ApiError::simple(
                ApiErrorKind::Download,
                "OpenRouter returned an empty video file",
            ));
        }
        Ok(partial_guard)
    }

    async fn open_download_response(&self, url: &Url) -> Result<HttpResponse, ApiError> {
        let mut current = url.clone();
        for redirect_count in 0..=MAX_REDIRECTS {
            validate_download_url(&current)?;
            let authorize = self.is_openrouter_api_url(&current)
                && current
                    .path()
                    .starts_with(&format!("{}/videos/", self.api_path));
            let request = HttpRequest {
                method: Method::GET,
                url: current.clone(),
                headers: self.headers(&current, authorize, false)?,
                json_body: None,
                stream_response: true,
            };
            let response = self.executor.execute(request).await.map_err(|_| {
                ApiError::simple(
                    ApiErrorKind::Network,
                    "Network connection interrupted while downloading video",
                )
            })?;
            if !matches!(
                response.status,
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                return Ok(response);
            }
            let location = response
                .headers
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    ApiError::simple(
                        ApiErrorKind::Download,
                        "Video download redirect did not include a destination",
                    )
                })?;
            if redirect_count == MAX_REDIRECTS {
                return Err(ApiError::simple(
                    ApiErrorKind::Download,
                    "Video download exceeded the redirect limit",
                ));
            }
            current = current.join(location).map_err(|_| {
                ApiError::simple(
                    ApiErrorKind::UnsafeUrl,
                    "Video download redirect contained an invalid URL",
                )
            })?;
        }
        Err(ApiError::simple(
            ApiErrorKind::Download,
            "Video download exceeded the redirect limit",
        ))
    }

    async fn request_json(
        &self,
        method: Method,
        url: Url,
        authorize: bool,
        retry_safe: bool,
        json_body: Option<Value>,
    ) -> Result<Value, ApiError> {
        for attempt in 0..=self.options.max_retries {
            let request = HttpRequest {
                method: method.clone(),
                url: url.clone(),
                headers: self.headers(&url, authorize, json_body.is_some())?,
                json_body: json_body.clone(),
                stream_response: false,
            };
            let response = match self.executor.execute(request).await {
                Ok(response) => response,
                Err(_) if !retry_safe => {
                    return Err(ApiError::simple(
                        ApiErrorKind::SubmissionUncertain,
                        "Connection failed during submission. The job may exist; do not submit again until history is checked.",
                    ));
                }
                Err(_) if attempt == self.options.max_retries => {
                    return Err(ApiError::simple(
                        ApiErrorKind::Network,
                        "Could not reach OpenRouter",
                    ));
                }
                Err(_) => {
                    tokio::time::sleep(self.delay(attempt, None)).await;
                    continue;
                }
            };

            if !response.status.is_success() {
                let status = response.status;
                let error = self.error_from_http(response).await;
                if !retry_safe && submission_status_is_ambiguous(status) {
                    return Err(submission_uncertain_from_http(error));
                }
                if retry_safe
                    && error
                        .status_code
                        .is_some_and(|status| retryable_statuses().contains(&status))
                    && attempt < self.options.max_retries
                {
                    tokio::time::sleep(self.delay(attempt, error.retry_after)).await;
                    continue;
                }
                return Err(error);
            }
            let body = match collect_body(response.body, MAX_JSON_BYTES).await {
                Ok(body) => body,
                Err(_) if !retry_safe => {
                    return Err(ApiError::simple(
                        ApiErrorKind::SubmissionUncertain,
                        "Connection failed during submission. The job may exist; do not submit again until history is checked.",
                    ));
                }
                Err(_) => {
                    return Err(ApiError::simple(
                        ApiErrorKind::Network,
                        "Network connection interrupted while reading OpenRouter's response",
                    ));
                }
            };
            let payload: Value = serde_json::from_slice(&body).map_err(|_| {
                ApiError::simple(
                    if retry_safe {
                        ApiErrorKind::ResponseFormat
                    } else {
                        ApiErrorKind::SubmissionUncertain
                    },
                    if retry_safe {
                        "OpenRouter returned an invalid JSON response"
                    } else {
                        "OpenRouter returned an unreadable submission response. The job may exist; do not submit again."
                    },
                )
            })?;
            if !payload.is_object() {
                return Err(ApiError::simple(
                    if retry_safe {
                        ApiErrorKind::ResponseFormat
                    } else {
                        ApiErrorKind::SubmissionUncertain
                    },
                    if retry_safe {
                        "OpenRouter returned a non-object JSON response"
                    } else {
                        "OpenRouter returned an invalid submission response. The job may exist; do not submit again."
                    },
                ));
            }
            return Ok(payload);
        }
        Err(ApiError::simple(
            ApiErrorKind::Network,
            "Could not reach OpenRouter",
        ))
    }

    async fn error_from_http(&self, response: HttpResponse) -> ApiError {
        let status = response.status.as_u16();
        let retry_after = parse_retry_after(response.headers.get(RETRY_AFTER));
        let body = collect_body(response.body, MAX_ERROR_BYTES)
            .await
            .unwrap_or_default();
        let (message, code, details) = self.extract_error(&body);
        let policy_hint =
            format!("{} {message}", code.as_deref().unwrap_or_default()).to_ascii_lowercase();
        let policy_error = status == 403
            || [
                "content policy",
                "content_policy",
                "moderation",
                "safety policy",
            ]
            .iter()
            .any(|token| policy_hint.contains(token));
        let kind = if policy_error {
            ApiErrorKind::ContentPolicy
        } else {
            match status {
                401 => ApiErrorKind::Authentication,
                402 => ApiErrorKind::InsufficientCredits,
                404 => ApiErrorKind::ResourceNotFound,
                429 => ApiErrorKind::RateLimit,
                400 | 422 => ApiErrorKind::RequestValidation,
                value if value >= 500 => ApiErrorKind::Provider,
                _ => ApiErrorKind::Provider,
            }
        };
        let friendly = match kind {
            ApiErrorKind::Authentication => "API key was rejected by OpenRouter".into(),
            ApiErrorKind::InsufficientCredits => {
                "OpenRouter credits are insufficient for this request".into()
            }
            ApiErrorKind::RateLimit => "OpenRouter rate limit reached; try again shortly".into(),
            ApiErrorKind::ContentPolicy if message.is_empty() => {
                "OpenRouter denied this request because of an account or content policy".into()
            }
            ApiErrorKind::RequestValidation if message.is_empty() => {
                "OpenRouter rejected the request".into()
            }
            ApiErrorKind::ResourceNotFound if message.is_empty() => {
                "OpenRouter resource was not found".into()
            }
            ApiErrorKind::Provider if message.is_empty() && status >= 500 => {
                "OpenRouter or the video provider is temporarily unavailable".into()
            }
            _ if message.is_empty() => format!("OpenRouter request failed ({status})"),
            _ => message,
        };
        ApiError {
            kind,
            message: friendly,
            status_code: Some(status),
            code,
            details,
            retry_after,
        }
    }

    fn extract_error(&self, body: &[u8]) -> (String, Option<String>, Map<String, Value>) {
        let Ok(payload) = serde_json::from_slice::<Value>(body) else {
            let text = String::from_utf8_lossy(body)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            return (self.redact(&truncate(&text, 500)), None, Map::new());
        };
        let Some(payload_object) = payload.as_object() else {
            return (String::new(), None, Map::new());
        };
        let error = payload_object.get("error").unwrap_or(&payload);
        if let Some(object) = error.as_object() {
            let code = object
                .get("code")
                .map(|value| self.redact(&value_to_string(value)));
            let details = object
                .get("metadata")
                .or_else(|| object.get("details"))
                .and_then(Value::as_object)
                .map(|object| self.redact_map(object))
                .unwrap_or_default();
            let message = object
                .get("message")
                .or_else(|| object.get("error"))
                .map(value_to_string)
                .unwrap_or_default();
            (self.redact(&truncate(&message, 1_000)), code, details)
        } else {
            (
                self.redact(&truncate(&value_to_string(error), 1_000)),
                None,
                Map::new(),
            )
        }
    }

    fn redact(&self, value: &str) -> String {
        value.replace(self.api_key.expose_secret(), "[REDACTED]")
    }

    fn redact_map(&self, value: &Map<String, Value>) -> Map<String, Value> {
        value
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    redact_value(value, self.api_key.expose_secret()),
                )
            })
            .collect()
    }

    fn headers(
        &self,
        url: &Url,
        authorize: bool,
        has_json_body: bool,
    ) -> Result<HeaderMap, ApiError> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!(
                "video-harness/",
                env!("CARGO_PKG_VERSION"),
                " Video Harness"
            )),
        );
        if let Some(referer) = &self.options.http_referer {
            headers.insert(
                HeaderName::from_static("http-referer"),
                header_value(referer, "HTTP-Referer")?,
            );
        }
        if !self.options.app_title.is_empty() {
            headers.insert(
                HeaderName::from_static("x-title"),
                header_value(&self.options.app_title, "X-Title")?,
            );
        }
        if has_json_body {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if authorize {
            if !self.is_openrouter_api_url(url) {
                return Err(ApiError::simple(
                    ApiErrorKind::UnsafeUrl,
                    "Refusing to send authorization outside the OpenRouter API",
                ));
            }
            let mut value = header_value(
                &format!("Bearer {}", self.api_key.expose_secret()),
                "Authorization",
            )?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        Ok(headers)
    }

    fn api_url(&self, path: &str) -> Result<Url, ApiError> {
        let mut url = self.options.base_url.clone();
        url.set_path(&format!(
            "{}/{}",
            self.api_path,
            path.trim_start_matches('/')
        ));
        Ok(url)
    }

    fn resolve_polling_url(&self, value: &str) -> Result<Url, ApiError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ApiError::simple(
                ApiErrorKind::RequestValidation,
                "A polling URL or job ID is required",
            ));
        }
        let url = if let Ok(url) = Url::parse(value) {
            url
        } else if value.starts_with('/') {
            let (scheme, host, port) = &self.api_origin;
            Url::parse(&format!("{scheme}://{host}:{port}{value}"))
                .map_err(|_| ApiError::simple(ApiErrorKind::UnsafeUrl, "Polling URL is invalid"))?
        } else if !value.contains('/') {
            let mut url = self.api_url("videos")?;
            url.path_segments_mut()
                .map_err(|_| ApiError::simple(ApiErrorKind::UnsafeUrl, "Polling URL is invalid"))?
                .push(value);
            url
        } else {
            let mut base = self.options.base_url.clone();
            if !base.path().ends_with('/') {
                let path = format!("{}/", base.path());
                base.set_path(&path);
            }
            base.join(value)
                .map_err(|_| ApiError::simple(ApiErrorKind::UnsafeUrl, "Polling URL is invalid"))?
        };
        if !self.is_openrouter_api_url(&url) {
            return Err(ApiError::simple(
                ApiErrorKind::UnsafeUrl,
                "Polling URL is not an OpenRouter API URL",
            ));
        }
        if !url
            .path()
            .starts_with(&format!("{}/videos/", self.api_path))
        {
            return Err(ApiError::simple(
                ApiErrorKind::UnsafeUrl,
                "Polling URL is not a video job URL",
            ));
        }
        Ok(url)
    }

    fn delay(&self, attempt: usize, retry_after: Option<Duration>) -> Duration {
        if let Some(delay) = retry_after {
            return delay;
        }
        let multiplier = 2u32.saturating_pow(u32::try_from(attempt).unwrap_or(u32::MAX));
        self.options
            .backoff_base
            .saturating_mul(multiplier)
            .min(Duration::from_secs(30))
    }
}

fn submission_status_is_ambiguous(status: StatusCode) -> bool {
    !status.is_client_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status.canonical_reason().is_none()
}

fn submission_uncertain_from_http(mut error: ApiError) -> ApiError {
    let status = error.status_code.unwrap_or_default();
    let provider_message = error.message.trim();
    error.kind = ApiErrorKind::SubmissionUncertain;
    error.message = format!(
        "OpenRouter returned HTTP {status} after receiving the submission: {provider_message}. The job may exist; do not submit again until history is checked."
    );
    error
}

async fn collect_body(mut body: HttpBody, limit: usize) -> Result<Vec<u8>, TransportError> {
    let mut output = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if output.len().saturating_add(chunk.len()) > limit {
            return Err(TransportError);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn validate_base_url(url: &Url) -> Result<(), ApiError> {
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(ApiError::simple(
            ApiErrorKind::Configuration,
            "base_url must be an HTTPS URL",
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::simple(
            ApiErrorKind::Configuration,
            "base_url must not contain credentials, a query, or a fragment",
        ));
    }
    Ok(())
}

fn validate_download_url(url: &Url) -> Result<(), ApiError> {
    validate_public_https_url(url.as_str(), "Video download").map_err(|_| {
        ApiError::simple(
            ApiErrorKind::UnsafeUrl,
            "Video download URL must use public HTTPS without embedded credentials",
        )
    })
}

fn origin(url: &Url) -> Option<(String, String, u16)> {
    Some((
        url.scheme().to_ascii_lowercase(),
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?,
    ))
}

fn validate_header_option(value: Option<&str>, label: &str) -> Result<(), ApiError> {
    if let Some(value) = value {
        header_value(value, label)?;
    }
    Ok(())
}

fn header_value(value: &str, label: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|_| {
        ApiError::simple(
            ApiErrorKind::Configuration,
            format!("{label} contains invalid header characters"),
        )
    })
}

fn parse_retry_after(value: Option<&HeaderValue>) -> Option<Duration> {
    let value = value?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
    {
        return Some(Duration::from_secs_f64(seconds.clamp(0.0, 60.0)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    let delay = date.duration_since(SystemTime::now()).unwrap_or_default();
    Some(delay.min(Duration::from_secs(60)))
}

fn redact_value(value: &Value, secret: &str) -> Value {
    match value {
        Value::String(value) => Value::String(value.replace(secret, "[REDACTED]")),
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
        value => value.clone(),
    }
}

fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

async fn path_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

pub(crate) fn checked_download_length(written: u64, chunk_bytes: usize) -> Option<u64> {
    written
        .checked_add(u64::try_from(chunk_bytes).ok()?)
        .filter(|next| *next <= MAX_VIDEO_DOWNLOAD_BYTES)
}

pub(crate) fn ensure_download_space(
    destination: &Path,
    expected_bytes: Option<u64>,
) -> Result<(), String> {
    let Some(expected_bytes) = expected_bytes else {
        return Ok(());
    };
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let Ok(Some(available_bytes)) = available_download_space(parent) else {
        // Some virtual and network filesystems do not expose free-space data.
        // The streamed-byte ceiling and write errors remain the fail-safe bounds.
        return Ok(());
    };
    if !download_space_is_sufficient(available_bytes, expected_bytes) {
        return Err("Not enough free disk space to safely download this video".into());
    }
    Ok(())
}

fn download_space_is_sufficient(available_bytes: u64, expected_bytes: u64) -> bool {
    available_bytes >= expected_bytes.saturating_add(DOWNLOAD_SPACE_RESERVE_BYTES)
}

#[cfg(unix)]
fn available_download_space(path: &Path) -> io::Result<Option<u64>> {
    let statistics = rustix::fs::statvfs(path).map_err(io::Error::from)?;
    let fragment_size = if statistics.f_frsize == 0 {
        statistics.f_bsize
    } else {
        statistics.f_frsize
    };
    Ok(Some(statistics.f_bavail.saturating_mul(fragment_size)))
}

#[cfg(windows)]
fn available_download_space(path: &Path) -> io::Result<Option<u64>> {
    use std::os::windows::ffi::OsStrExt;

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available_bytes = 0u64;
    // SAFETY: `wide_path` is NUL-terminated and remains alive for the call;
    // the only non-null output pointer refers to a valid `u64`.
    let succeeded = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut available_bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Some(available_bytes))
    }
}

#[cfg(not(any(unix, windows)))]
fn available_download_space(_path: &Path) -> io::Result<Option<u64>> {
    Ok(None)
}

#[allow(dead_code)]
fn _utc_now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::{
        DOWNLOAD_SPACE_RESERVE_BYTES, MAX_VIDEO_DOWNLOAD_BYTES, checked_download_length,
        download_space_is_sufficient, public_socket_addresses,
    };

    #[test]
    fn streamed_download_length_is_bounded_without_overflow() {
        assert_eq!(
            checked_download_length(MAX_VIDEO_DOWNLOAD_BYTES - 1, 1),
            Some(MAX_VIDEO_DOWNLOAD_BYTES)
        );
        assert_eq!(checked_download_length(MAX_VIDEO_DOWNLOAD_BYTES, 1), None);
        assert_eq!(checked_download_length(u64::MAX, 1), None);
    }

    #[test]
    fn free_space_preflight_keeps_a_reserve() {
        assert!(!download_space_is_sufficient(
            DOWNLOAD_SPACE_RESERVE_BYTES,
            1
        ));
        assert!(download_space_is_sufficient(
            DOWNLOAD_SPACE_RESERVE_BYTES + 1,
            1
        ));
    }

    #[test]
    fn dns_resolution_exposes_only_public_addresses_and_fails_closed() {
        let public_v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443);
        let public_v6 = SocketAddr::new(
            IpAddr::V6("2606:4700:4700::1111".parse::<Ipv6Addr>().expect("IPv6")),
            443,
        );
        let private = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 443);

        assert_eq!(
            public_socket_addresses([private, public_v4, public_v6]).expect("public DNS answers"),
            vec![public_v4, public_v6]
        );
        assert!(public_socket_addresses([private]).is_err());
    }
}
