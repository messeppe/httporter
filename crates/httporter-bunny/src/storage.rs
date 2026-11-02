//! The Bunny Storage HTTP client: request building, per-type concurrency
//! limits, retries, latency reporting and HTTP-status error mapping.
//!
//! There are two HTTP clients. Listings and deletes are short requests and
//! share a client with a total timeout: 120 s for a delete, and
//! [`BunnyConfig::list_timeout`] for a listing (someone browsing a folder
//! wants an answer, or a failure, far sooner). Uploads and downloads can
//! legitimately take far longer on a slow link, so they use a client with no
//! total timeout and are abandoned only when they stop making progress for
//! [`BunnyConfig::transfer_idle_timeout`]. reqwest's own `read_timeout` is not
//! enough for that: it only resets between response-body chunks, and the
//! deadline for the response headers is never reset, so it would cut off any
//! upload that takes longer than the timeout to send.

use std::error::Error as _;
use std::fmt;
use std::future::Future;
use std::io;
use std::num::IntErrorKind;
use std::path::Path;
use std::pin::pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future::{Either, select};
use futures_util::stream::{self, Stream, StreamExt};
use httporter_core::{
    AuthHint, ByteStream, Capabilities, ChecksumKind, EmptyFolders, Entry, Listing,
    MAX_RETRY_AFTER, ProviderError, RemotePath, RequestInfo, RetryPolicy, StorageProvider,
    UploadOptions, UploadSource, encode_segment, with_retry,
};
use reqwest::header::{
    ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
};
use reqwest::{Body, StatusCode, Version, redirect};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::io::ReaderStream;

use crate::model::{BunnyObject, to_entry};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The total time a delete, and by default a listing, may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an upload or download may make no progress by default.
const TRANSFER_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const USER_AGENT: &str = "HTTPorter/0.1";
/// The size of the pieces an upload body is fed to the connection in.
const BODY_CHUNK: usize = 64 * 1024;
/// How much of an error response body is ever read.
const ERROR_BODY_MAX: usize = 4 * 1024;
/// How much of it can end up in an error message.
const ERROR_SNIPPET_MAX: usize = 200;

/// Directory listings the service tolerates at once before it starts
/// answering "busy".
const LIST_CONCURRENCY: usize = 5;
const DELETE_CONCURRENCY_MAX: usize = 30;
const TRANSFER_CONCURRENCY_MAX: usize = 96;

/// Everything needed to talk to one storage zone.
#[derive(Clone)]
pub struct BunnyConfig {
    pub zone: String,
    /// The regional API hostname, for example `storage.bunnycdn.com`.
    pub host: String,
    /// The storage zone password, sent as the `AccessKey` header.
    pub password: String,
    /// Uploads and downloads in flight at once; capped at 96.
    pub transfer_concurrency: usize,
    /// Capped at 30 in the advertised capabilities.
    pub delete_concurrency: usize,
    /// How long an upload or download may make no progress before it is
    /// abandoned (and retried). Transfers have no overall time limit.
    pub transfer_idle_timeout: Duration,
    /// How long one try at a listing may take in all, from connecting to the
    /// last byte of the answer. 120 s by default; deletes always have 120 s.
    pub list_timeout: Duration,
    /// The retries for uploads, downloads and deletes, and for listings
    /// unless [`Self::list_retry`] is set.
    pub retry: RetryPolicy,
    /// The retries for listings, when they should differ from [`Self::retry`]
    /// (a folder someone is waiting for gives up sooner than a transfer).
    pub list_retry: Option<RetryPolicy>,
    /// Replaces `https://{host}`, so tests can point at a local server.
    pub base_url: Option<String>,
}

impl BunnyConfig {
    pub fn new(
        zone: impl Into<String>,
        host: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            zone: zone.into(),
            host: host.into(),
            password: password.into(),
            transfer_concurrency: 32,
            delete_concurrency: 16,
            transfer_idle_timeout: TRANSFER_IDLE_TIMEOUT,
            list_timeout: REQUEST_TIMEOUT,
            retry: RetryPolicy::default(),
            list_retry: None,
            base_url: None,
        }
    }
}

impl fmt::Debug for BunnyConfig {
    /// Leaves the password out so a config can be logged safely.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BunnyConfig")
            .field("zone", &self.zone)
            .field("host", &self.host)
            .field("password", &"<redacted>")
            .field("transfer_concurrency", &self.transfer_concurrency)
            .field("delete_concurrency", &self.delete_concurrency)
            .field("transfer_idle_timeout", &self.transfer_idle_timeout)
            .field("list_timeout", &self.list_timeout)
            .field("retry", &self.retry)
            .field("list_retry", &self.list_retry)
            .field("base_url", &self.base_url)
            .finish()
    }
}

pub struct BunnyStorage {
    /// For listings and deletes: bounded by [`REQUEST_TIMEOUT`], and a
    /// listing by `list_timeout`.
    client: reqwest::Client,
    /// For uploads and downloads: no total timeout, see the module docs.
    transfer_client: reqwest::Client,
    /// `scheme://host[:port]` with no trailing slash.
    base: String,
    zone: String,
    retry: RetryPolicy,
    list_retry: RetryPolicy,
    list_timeout: Duration,
    capabilities: Capabilities,
    list_slots: Semaphore,
    delete_slots: Semaphore,
    /// Shared by uploads and downloads. Owned permits, because a download
    /// keeps its slot for as long as its body stream is alive.
    transfer_slots: Arc<Semaphore>,
    transfer_idle_timeout: Duration,
}

impl BunnyStorage {
    pub fn new(cfg: BunnyConfig) -> Result<Self, ProviderError> {
        let mut access_key = HeaderValue::from_str(&cfg.password).map_err(|_| {
            ProviderError::Permanent("the storage password cannot be sent as an HTTP header".into())
        })?;
        access_key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static("accesskey"), access_key);

        // A semaphore with no permits would stall every operation forever.
        let delete_concurrency = cfg.delete_concurrency.clamp(1, DELETE_CONCURRENCY_MAX);
        let transfer_concurrency = cfg.transfer_concurrency.clamp(1, TRANSFER_CONCURRENCY_MAX);

        let proxy = proxy_mode(cfg.base_url.as_deref());
        let client = build_client(
            headers.clone(),
            LIST_CONCURRENCY + delete_concurrency,
            Some(REQUEST_TIMEOUT),
            proxy,
        )?;
        let transfer_client = build_client(headers, transfer_concurrency, None, proxy)?;

        let base = match &cfg.base_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => format!("https://{}", cfg.host),
        };

        Ok(Self {
            client,
            transfer_client,
            base,
            zone: cfg.zone,
            list_retry: cfg.list_retry.unwrap_or_else(|| cfg.retry.clone()),
            retry: cfg.retry,
            list_timeout: cfg.list_timeout,
            capabilities: Capabilities {
                checksum_in_listing: Some(ChecksumKind::Sha256),
                server_side_copy: false,
                recursive_delete: true,
                batch_delete_max: 1,
                empty_folders: EmptyFolders::Unknown,
                list_concurrency: LIST_CONCURRENCY,
                delete_concurrency,
                transfer_concurrency_max: TRANSFER_CONCURRENCY_MAX,
                presign: false,
                read_only: false,
            },
            list_slots: Semaphore::new(LIST_CONCURRENCY),
            delete_slots: Semaphore::new(delete_concurrency),
            transfer_slots: Arc::new(Semaphore::new(transfer_concurrency)),
            transfer_idle_timeout: cfg.transfer_idle_timeout,
        })
    }

    /// The URL of a directory: the zone, then the encoded path, then a
    /// trailing slash (which is what makes Bunny list rather than fetch it).
    fn dir_url(&self, dir: &RemotePath) -> String {
        format!(
            "{}/{}/{}",
            self.base,
            encode_segment(&self.zone),
            dir.to_url_path(true)
        )
    }

    /// The URL of a file: like [`Self::dir_url`] without the trailing slash.
    /// The root has no such URL, and sending the zone's own URL to a file
    /// operation (a `DELETE` above all) would act on the whole zone.
    fn file_url(&self, file: &RemotePath) -> Result<String, ProviderError> {
        if file.is_root() {
            return Err(ProviderError::Refused(
                "The zone root is a folder, not a file".into(),
            ));
        }
        Ok(format!(
            "{}/{}/{}",
            self.base,
            encode_segment(&self.zone),
            file.to_url_path(false)
        ))
    }

    /// One attempt at fetching and parsing a directory listing.
    async fn fetch_listing(
        &self,
        url: &str,
    ) -> Result<(Vec<BunnyObject>, RequestInfo), ProviderError> {
        // Held until the body is read, so at most `LIST_CONCURRENCY` listings
        // are in flight. Acquired per attempt, so a retry's backoff does not
        // hold a slot.
        let _slot = self
            .list_slots
            .acquire()
            .await
            .map_err(|_| ProviderError::Permanent("the listing limiter was closed".into()))?;

        // Queueing for a slot is not request latency.
        let started = Instant::now();
        let response = self
            .client
            .get(url)
            .header(ACCEPT, "application/json")
            .timeout(self.list_timeout)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        let http_version = http_version(response.version());
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            return Err(map_status(status, retry_after, Op::List));
        }
        let body = response.bytes().await.map_err(network_error)?;
        let latency = started.elapsed();

        Ok((
            parse_listing(&body)?,
            RequestInfo {
                latency,
                http_version: http_version.to_owned(),
            },
        ))
    }

    /// One attempt at uploading `source`. Called afresh for every retry, so a
    /// file is opened (and a body built) again each time.
    async fn put_once(
        &self,
        url: &str,
        source: &UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        let _slot = self
            .transfer_slots
            .acquire()
            .await
            .map_err(|_| transfer_limiter_closed())?;

        let activity = Activity::new();
        let (body, len) = match source {
            UploadSource::Bytes(bytes) => (
                Body::wrap_stream(activity.track(chunked(bytes.clone()))),
                bytes.len() as u64,
            ),
            UploadSource::File { path, len } => {
                let file = tokio::fs::File::open(path)
                    .await
                    .map_err(|e| local_file_error(path, &e))?;
                (
                    Body::wrap_stream(
                        activity.track(ReaderStream::with_capacity(file, BODY_CHUNK)),
                    ),
                    *len,
                )
            }
        };
        let mut request = self
            .transfer_client
            .put(url)
            .header(CONTENT_TYPE, opts.content_type.as_str())
            // Declared, so the body is sent with a length rather than chunked.
            .header(CONTENT_LENGTH, len)
            .body(body);
        if let Some(sha256) = &opts.sha256 {
            request = request.header("Checksum", hex_upper(sha256));
        }

        // The server answers only after the whole body has arrived, so the
        // wait covers sending it as well; what is limited is the time since
        // the connection last asked for more of the body.
        let response = until_stalled(
            request.send(),
            &activity,
            self.transfer_idle_timeout,
            "upload",
        )
        .await?
        .map_err(network_error)?;

        let status = response.status();
        if matches!(status.as_u16(), 200 | 201) {
            return Ok(());
        }
        let with_checksum = opts.sha256.is_some();
        if status == StatusCode::BAD_REQUEST {
            // A 400 can be an overloaded server or a rejected upload, and only
            // the body tells them apart.
            let body = tokio::time::timeout(
                self.transfer_idle_timeout,
                read_capped(response, ERROR_BODY_MAX),
            )
            .await
            .map_err(|_| stall_error("upload", self.transfer_idle_timeout))?
            .map_err(network_error)?;
            return Err(map_upload_400(&body, with_checksum));
        }
        let retry_after = parse_retry_after(response.headers());
        Err(map_status(
            status,
            retry_after,
            Op::Upload { with_checksum },
        ))
    }

    /// One attempt at starting a download: the response headers, plus the
    /// transfer slot the body will keep occupying.
    async fn get_once(
        &self,
        url: &str,
    ) -> Result<(reqwest::Response, OwnedSemaphorePermit), ProviderError> {
        let slot = Arc::clone(&self.transfer_slots)
            .acquire_owned()
            .await
            .map_err(|_| transfer_limiter_closed())?;

        let sent = tokio::time::timeout(
            self.transfer_idle_timeout,
            self.transfer_client.get(url).send(),
        )
        .await
        .map_err(|_| stall_error("download", self.transfer_idle_timeout))?;
        let response = sent.map_err(network_error)?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = parse_retry_after(response.headers());
            return Err(map_status(status, retry_after, Op::Download));
        }
        Ok((response, slot))
    }

    /// One attempt at a `DELETE`.
    async fn delete_once(&self, url: &str) -> Result<(), ProviderError> {
        let _slot = self
            .delete_slots
            .acquire()
            .await
            .map_err(|_| ProviderError::Permanent("the delete limiter was closed".into()))?;

        let response = self
            .client
            .delete(url)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let retry_after = parse_retry_after(response.headers());
        Err(map_status(status, retry_after, Op::Delete))
    }
}

#[async_trait]
impl StorageProvider for BunnyStorage {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn endpoint(&self) -> String {
        format!("{}/{}/", self.base, self.zone)
    }

    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        let url = self.dir_url(dir);
        let url = url.as_str();
        let (objects, info) =
            with_retry(&self.list_retry, move |_| self.fetch_listing(url)).await?;
        Ok(Listing {
            entries: to_entries(objects, dir)?,
            info,
        })
    }

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        let url = self.file_url(path)?;
        let url = url.as_str();
        let source = &source;
        with_retry(&self.retry, move |_| self.put_once(url, source, opts)).await
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        let url = self.file_url(path)?;
        let url = url.as_str();
        let (response, slot) = with_retry(&self.retry, move |_| self.get_once(url)).await?;
        Ok(guard_download(
            Box::pin(response.bytes_stream()),
            slot,
            self.transfer_idle_timeout,
        ))
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        let url = self.file_url(path)?;
        let url = url.as_str();
        with_retry(&self.retry, move |_| self.delete_once(url)).await
    }

    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        if dir.is_root() {
            return Err(ProviderError::Refused(
                "Deleting the whole zone isn't allowed".into(),
            ));
        }
        let url = self.dir_url(dir);
        let url = url.as_str();
        with_retry(&self.retry, move |_| self.delete_once(url)).await
    }

    /// Bunny has no request that makes an empty folder; one appears with its
    /// first file. (The live probe may change this.)
    async fn create_folder(&self, _dir: &RemotePath) -> Result<(), ProviderError> {
        Err(ProviderError::Refused(
            "Bunny Storage creates a folder with its first file.".into(),
        ))
    }
}

/// The kind of request a response answers, which decides what a 400 means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    List,
    Download,
    Upload { with_checksum: bool },
    Delete,
}

/// Turns a non-success HTTP status into the error the engine acts on.
fn map_status(status: StatusCode, retry_after: Option<Duration>, op: Op) -> ProviderError {
    match status.as_u16() {
        // Bunny never says which part of the credentials it rejected.
        401 | 403 => ProviderError::Auth {
            hint: AuthHint::Unknown,
        },
        404 => ProviderError::NotFound,
        429 => ProviderError::RateLimited { retry_after },
        // Bunny answers 400 "Unable to process request" when it is overloaded.
        400 => match op {
            Op::List | Op::Delete => server_busy(),
            Op::Upload {
                with_checksum: true,
            } => ProviderError::ChecksumMismatch,
            Op::Upload { .. } | Op::Download => {
                ProviderError::Permanent(format!("request rejected by server ({status})"))
            }
        },
        code @ 500..=599 => ProviderError::Server {
            status: code,
            detail: status.canonical_reason().unwrap_or_default().to_owned(),
        },
        _ => ProviderError::Permanent(format!("unexpected response from server ({status})")),
    }
}

/// Bunny's "Unable to process request": the server is overloaded, so the
/// request is worth another try, and fewer at once.
fn server_busy() -> ProviderError {
    ProviderError::Server {
        status: 400,
        detail: "busy".into(),
    }
}

/// What a 400 answering an upload means. Bunny says "Unable to process
/// request" when it is overloaded, which is worth another try; any other 400
/// rejected the upload itself.
fn map_upload_400(body: &[u8], with_checksum: bool) -> ProviderError {
    let body = String::from_utf8_lossy(body);
    if body
        .to_ascii_lowercase()
        .contains("unable to process request")
    {
        return server_busy();
    }
    match map_status(StatusCode::BAD_REQUEST, None, Op::Upload { with_checksum }) {
        // Without a checksum there is nothing to blame, so say what the
        // server said. It is server text, never anything we sent.
        ProviderError::Permanent(message) => {
            let words = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let snippet: String = words.chars().take(ERROR_SNIPPET_MAX).collect();
            if snippet.is_empty() {
                ProviderError::Permanent(message)
            } else if snippet.len() < words.len() {
                ProviderError::Permanent(format!("{message}: {snippet}..."))
            } else {
                ProviderError::Permanent(format!("{message}: {snippet}"))
            }
        }
        other => other,
    }
}

/// Up to `max` bytes of a response body; the rest is not read.
async fn read_capped(mut response: reqwest::Response, max: usize) -> reqwest::Result<Vec<u8>> {
    let mut body = Vec::new();
    while body.len() < max {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let room = max - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    Ok(body)
}

/// `Retry-After` as a number of seconds, at most [`MAX_RETRY_AFTER`]; the
/// HTTP-date form is ignored.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let wait = match value.parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        // Too big for a number is still a very long wait.
        Err(e) if *e.kind() == IntErrorKind::PosOverflow => MAX_RETRY_AFTER,
        Err(_) => return None,
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

fn http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    }
}

/// A request that never produced a usable response (no connection, a reset, a
/// timeout): a network error, worth another try, unless the request itself
/// could not be built.
fn network_error(error: reqwest::Error) -> ProviderError {
    let mut causes = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    let builder_error = error.is_builder();
    // The URL names the zone and path, which callers already know.
    let mut message = error.without_url().to_string();
    for cause in causes {
        message.push_str(": ");
        message.push_str(&cause);
    }
    if builder_error {
        ProviderError::Permanent(message)
    } else {
        ProviderError::Network(message)
    }
}

/// Whether a client consults the system proxy settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyMode {
    /// reqwest's default: environment variables and the Windows settings.
    System,
    /// Never use a proxy.
    Direct,
}

/// A configured `base_url` is the test and local-server override, and such an
/// endpoint must never be routed through a system proxy. The real Bunny host
/// (no override) honours the system proxy settings.
fn proxy_mode(base_url: Option<&str>) -> ProxyMode {
    if base_url.is_some() {
        ProxyMode::Direct
    } else {
        ProxyMode::System
    }
}

/// Both clients carry the access key and never follow redirects. Only the
/// listing and delete client has a total timeout; see the module docs.
fn build_client(
    headers: HeaderMap,
    pool_size: usize,
    total_timeout: Option<Duration>,
    proxy: ProxyMode,
) -> Result<reqwest::Client, ProviderError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .pool_max_idle_per_host(pool_size)
        // The API never redirects; following one would forward the access key
        // to whatever host it names.
        .redirect(redirect::Policy::none())
        .default_headers(headers);
    if let Some(timeout) = total_timeout {
        builder = builder.timeout(timeout);
    }
    if proxy == ProxyMode::Direct {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| ProviderError::Permanent(format!("could not create the HTTP client: {e}")))
}

/// Uppercase hex, which is how Bunny wants the `Checksum` header.
fn hex_upper(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02X}");
        out
    })
}

fn transfer_limiter_closed() -> ProviderError {
    ProviderError::Permanent("the transfer limiter was closed".into())
}

/// A local file that could not be opened. Retrying will not conjure it up.
fn local_file_error(path: &Path, error: &io::Error) -> ProviderError {
    ProviderError::Permanent(format!("could not read {}: {error}", path.display()))
}

/// No progress for `idle`: as good as no answer.
fn stall_error(what: &str, idle: Duration) -> ProviderError {
    ProviderError::Network(format!("the {what} stalled (no progress for {idle:?})"))
}

/// An upload body in pieces, so the connection asks for it a bit at a time.
fn chunked(bytes: Bytes) -> impl Stream<Item = io::Result<Bytes>> + Unpin + Send + 'static {
    let len = bytes.len();
    stream::iter(
        (0..len)
            .step_by(BODY_CHUNK)
            .map(move |start| Ok(bytes.slice(start..len.min(start + BODY_CHUNK)))),
    )
}

/// When an upload last made progress.
#[derive(Clone)]
struct Activity(Arc<Mutex<Instant>>);

impl Activity {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn touch(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle(&self) -> Duration {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    /// Passes `inner` through, noting each time it hands over a piece (or its
    /// end). The connection only asks for the next piece once it has taken the
    /// previous one, so a long gap means the transfer has stalled. Merely
    /// being polled does not count: a poll that finds nothing ready says
    /// nothing about progress.
    fn track<S>(&self, mut inner: S) -> impl Stream<Item = S::Item> + Send + 'static
    where
        S: Stream + Unpin + Send + 'static,
    {
        let activity = self.clone();
        stream::poll_fn(move |cx| {
            let poll = inner.poll_next_unpin(cx);
            if poll.is_ready() {
                activity.touch();
            }
            poll
        })
    }

    /// Completes once nothing has happened for `limit`.
    async fn stalled(&self, limit: Duration) {
        loop {
            let idle = self.idle();
            if idle >= limit {
                return;
            }
            tokio::time::sleep(limit - idle).await;
        }
    }
}

/// Runs `work`, giving up with a network error if `activity` shows no
/// progress for `limit`.
async fn until_stalled<T>(
    work: impl Future<Output = T>,
    activity: &Activity,
    limit: Duration,
    what: &str,
) -> Result<T, ProviderError> {
    match select(pin!(work), pin!(activity.stalled(limit))).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(((), _)) => Err(stall_error(what, limit)),
    }
}

/// A download body as the engine sees it. It fails with a network error if
/// no data arrives for `idle`, and it holds its transfer `slot` until it ends
/// or is dropped.
fn guard_download<S>(body: S, slot: OwnedSemaphorePermit, idle: Duration) -> ByteStream
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + Unpin + 'static,
{
    Box::pin(stream::try_unfold(
        (body, slot),
        move |(mut body, slot)| async move {
            match tokio::time::timeout(idle, body.next()).await {
                Err(_) => Err(stall_error("download", idle)),
                Ok(None) => Ok(None),
                Ok(Some(Err(error))) => Err(network_error(error)),
                Ok(Some(Ok(chunk))) => Ok(Some((chunk, (body, slot)))),
            }
        },
    ))
}

fn parse_listing(body: &[u8]) -> Result<Vec<BunnyObject>, ProviderError> {
    serde_json::from_slice(body).map_err(|e| {
        if e.is_data() {
            // Valid JSON, but not a list of objects.
            ProviderError::Permanent(format!(
                "unexpected response from server (unrecognised listing: {e})"
            ))
        } else {
            ProviderError::Permanent("unexpected response from server (not JSON)".into())
        }
    })
}

/// One malformed object fails the whole listing, and says which one.
fn to_entries(objects: Vec<BunnyObject>, dir: &RemotePath) -> Result<Vec<Entry>, ProviderError> {
    objects
        .into_iter()
        .map(|object| {
            let name = object.object_name.clone();
            to_entry(object, dir).map_err(|error| {
                let reason = match error {
                    ProviderError::Permanent(message) => message,
                    other => other.to_string(),
                };
                ProviderError::Permanent(format!(
                    "unexpected entry \"{name}\" in listing: {reason}"
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16) -> StatusCode {
        StatusCode::from_u16(code).unwrap()
    }

    fn busy() -> ProviderError {
        ProviderError::Server {
            status: 400,
            detail: "busy".into(),
        }
    }

    #[test]
    fn credentials_and_missing_things_map_directly() {
        // Bunny never says which part of the credentials it rejected.
        let auth = ProviderError::Auth {
            hint: AuthHint::Unknown,
        };
        for op in [Op::List, Op::Download, Op::Delete] {
            assert_eq!(map_status(status(401), None, op), auth);
            assert_eq!(map_status(status(403), None, op), auth);
            assert_eq!(map_status(status(404), None, op), ProviderError::NotFound);
        }
    }

    #[test]
    fn rate_limiting_carries_retry_after() {
        let two = Some(Duration::from_secs(2));
        assert_eq!(
            map_status(status(429), two, Op::List),
            ProviderError::RateLimited { retry_after: two }
        );
        assert_eq!(
            map_status(status(429), None, Op::Delete),
            ProviderError::RateLimited { retry_after: None }
        );
    }

    #[test]
    fn a_400_depends_on_the_operation() {
        assert_eq!(map_status(status(400), None, Op::List), busy());
        assert_eq!(map_status(status(400), None, Op::Delete), busy());
        assert_eq!(
            map_status(
                status(400),
                None,
                Op::Upload {
                    with_checksum: true
                }
            ),
            ProviderError::ChecksumMismatch
        );
        for op in [
            Op::Download,
            Op::Upload {
                with_checksum: false,
            },
        ] {
            assert!(
                matches!(
                    map_status(status(400), None, op),
                    ProviderError::Permanent(_)
                ),
                "{op:?}"
            );
        }
    }

    #[test]
    fn an_upload_400_is_busy_only_when_the_server_says_so() {
        let busy = busy();
        for with_checksum in [true, false] {
            for body in [
                "Unable to process request",
                "unable to process request.",
                r#"{"HttpCode":400,"Message":"UNABLE TO PROCESS REQUEST"}"#,
            ] {
                assert_eq!(
                    map_upload_400(body.as_bytes(), with_checksum),
                    busy,
                    "{body}"
                );
            }
        }
        assert!(busy.is_retryable());
        assert!(busy.is_slow_down());
    }

    #[test]
    fn any_other_upload_400_blames_the_checksum_when_one_was_sent() {
        for body in ["", "Checksum mismatch", "Bad thing", "\u{0}\u{ff}binary"] {
            assert_eq!(
                map_upload_400(body.as_bytes(), true),
                ProviderError::ChecksumMismatch,
                "{body:?}"
            );
        }
    }

    #[test]
    fn an_unchecksummed_upload_400_quotes_a_short_trimmed_snippet() {
        let message = |body: &str| match map_upload_400(body.as_bytes(), false) {
            ProviderError::Permanent(message) => message,
            other => panic!("expected Permanent, got {other:?}"),
        };
        let prefix = "request rejected by server (400 Bad Request)";
        assert_eq!(message(""), prefix);
        assert_eq!(message(" \n\t "), prefix);
        assert_eq!(message("Bad thing"), format!("{prefix}: Bad thing"));
        assert_eq!(
            message("  Bad\r\n  thing\n"),
            format!("{prefix}: Bad thing"),
            "trimmed, with runs of whitespace collapsed"
        );
        let long = message(&"x".repeat(5000));
        assert_eq!(
            long,
            format!("{prefix}: {}...", "x".repeat(ERROR_SNIPPET_MAX))
        );
    }

    #[test]
    fn server_errors_keep_their_status_and_everything_else_is_permanent() {
        for code in [500, 502, 503, 599] {
            match map_status(status(code), None, Op::List) {
                ProviderError::Server { status, .. } => assert_eq!(status, code),
                other => panic!("{code}: expected Server, got {other:?}"),
            }
        }
        assert_eq!(
            map_status(status(503), None, Op::Delete),
            ProviderError::Server {
                status: 503,
                detail: "Service Unavailable".into()
            }
        );
        for code in [301, 402, 405, 409, 413, 418] {
            assert!(
                matches!(
                    map_status(status(code), None, Op::List),
                    ProviderError::Permanent(_)
                ),
                "{code}"
            );
        }
    }

    #[test]
    fn retry_after_reads_whole_seconds_only() {
        let with = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            parse_retry_after(&headers)
        };
        assert_eq!(with("2"), Some(Duration::from_secs(2)));
        assert_eq!(with(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(with("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_is_clamped_to_five_minutes() {
        let with = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            parse_retry_after(&headers)
        };
        let max = Some(Duration::from_secs(300));
        assert_eq!(with("99999999999"), max);
        // Too large even for a u64 is still "a very long time", not "no hint".
        assert_eq!(with("99999999999999999999999999"), max);
        assert_eq!(with("301"), max);
        assert_eq!(with("300"), max);
        assert_eq!(with("299"), Some(Duration::from_secs(299)));
    }

    /// `count` items, one every 40 ms, after which the stream ends or hangs.
    fn trickle(count: u32, then_hang: bool) -> impl Stream<Item = u32> + Unpin + Send + 'static {
        Box::pin(stream::unfold(0, move |n| async move {
            if n < count {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Some((n, n + 1))
            } else if then_hang {
                std::future::pending().await
            } else {
                None
            }
        }))
    }

    async fn drain(stream: impl Stream<Item = u32> + Unpin) -> usize {
        stream.count().await
    }

    #[tokio::test]
    async fn a_transfer_is_cut_off_only_when_it_stops_moving() {
        let limit = Duration::from_millis(150);

        // Eight items take 320 ms, longer than the limit, but never go quiet
        // for anything like 150 ms.
        let activity = Activity::new();
        let moving = activity.track(trickle(8, false));
        let done = until_stalled(drain(moving), &activity, limit, "upload").await;
        assert_eq!(done, Ok(8));

        let activity = Activity::new();
        let stuck = activity.track(trickle(2, true));
        let started = Instant::now();
        let watchdog = until_stalled(drain(stuck), &activity, limit, "upload");
        let done = tokio::time::timeout(Duration::from_secs(5), watchdog)
            .await
            .expect("the watchdog must fire");
        match done {
            Err(ProviderError::Network(m)) => assert!(m.contains("upload stalled"), "{m}"),
            other => panic!("expected a stall, got {other:?}"),
        }
        // 80 ms of items, then the limit.
        assert!(started.elapsed() >= limit, "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_download_body_stalls_out_and_frees_its_slot() {
        let slots = Arc::new(Semaphore::new(1));
        let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
        let body = stream::iter([Ok(Bytes::from_static(b"x"))])
            .chain(stream::pending::<reqwest::Result<Bytes>>());
        let mut guarded = guard_download(body, slot, Duration::from_millis(100));

        assert_eq!(slots.available_permits(), 0, "held while the body is open");
        assert_eq!(guarded.next().await, Some(Ok(Bytes::from_static(b"x"))));
        match guarded.next().await {
            Some(Err(ProviderError::Network(m))) => {
                assert!(m.contains("download stalled"), "{m}");
            }
            other => panic!("expected a stall, got {other:?}"),
        }
        assert!(
            guarded.next().await.is_none(),
            "the body ends after an error"
        );
        assert_eq!(slots.available_permits(), 1, "freed once the body is over");
    }

    #[tokio::test]
    async fn a_download_body_frees_its_slot_when_dropped_early() {
        let slots = Arc::new(Semaphore::new(1));
        let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
        let body = stream::pending::<reqwest::Result<Bytes>>();
        let guarded = guard_download(body, slot, Duration::from_secs(60));
        assert_eq!(slots.available_permits(), 0);
        drop(guarded);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn only_a_client_built_with_a_total_timeout_has_one() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(400)))
            .mount(&server)
            .await;
        let timeout = Some(Duration::from_millis(150));
        let bounded = build_client(HeaderMap::new(), 1, timeout, ProxyMode::Direct).unwrap();
        let unbounded = build_client(HeaderMap::new(), 1, None, ProxyMode::Direct).unwrap();

        let error = bounded.get(server.uri()).send().await.unwrap_err();
        assert!(error.is_timeout(), "{error:?}");
        let response = unbounded.get(server.uri()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn a_base_url_override_bypasses_the_system_proxy() {
        // Ruling P15: the override is for tests and local servers, which must
        // never be routed through a Windows system proxy. The real endpoint
        // keeps honouring it.
        assert_eq!(proxy_mode(Some("http://127.0.0.1:8080")), ProxyMode::Direct);
        assert_eq!(proxy_mode(Some("https://example.test/")), ProxyMode::Direct);
        assert_eq!(proxy_mode(None), ProxyMode::System);

        let mut cfg = BunnyConfig::new("zone", "storage.bunnycdn.com", "secret");
        assert_eq!(proxy_mode(cfg.base_url.as_deref()), ProxyMode::System);
        cfg.base_url = Some("http://127.0.0.1:9".into());
        assert_eq!(proxy_mode(cfg.base_url.as_deref()), ProxyMode::Direct);
    }

    #[test]
    fn checksums_are_sent_as_uppercase_hex() {
        assert_eq!(hex_upper(&[0x00, 0x0a, 0xff, 0x5c]), "000AFF5C");
        assert_eq!(hex_upper(&[]), "");
    }

    #[tokio::test]
    async fn upload_bodies_are_fed_in_pieces_that_add_up() {
        let data: Vec<u8> = (0..BODY_CHUNK * 2 + 5).map(|i| (i % 251) as u8).collect();
        let pieces: Vec<Bytes> = chunked(Bytes::from(data.clone()))
            .map(Result::unwrap)
            .collect()
            .await;
        let sizes: Vec<usize> = pieces.iter().map(Bytes::len).collect();
        assert_eq!(sizes, [BODY_CHUNK, BODY_CHUNK, 5]);
        assert_eq!(pieces.concat(), data);
        assert_eq!(chunked(Bytes::new()).count().await, 0);
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_is_ignored() {
        let mut cfg = BunnyConfig::new("zone", "storage.bunnycdn.com", "secret");
        cfg.base_url = Some("http://127.0.0.1:9/".into());
        let storage = BunnyStorage::new(cfg).unwrap();
        assert_eq!(storage.endpoint(), "http://127.0.0.1:9/zone/");
        assert_eq!(
            storage.dir_url(&RemotePath::parse("a b").unwrap()),
            "http://127.0.0.1:9/zone/a%20b/"
        );
    }

    #[test]
    fn the_config_debug_output_hides_the_password() {
        let cfg = BunnyConfig::new("zone", "storage.bunnycdn.com", "hunter2");
        assert!(!format!("{cfg:?}").contains("hunter2"));
    }
}
