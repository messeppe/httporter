//! The Bunny Storage client: its configuration, its HTTP clients, URLs, deletes, and how
//! answers map to errors. Listings live in `list.rs`, uploads and downloads in `transfer.rs`;
//! the account API (`account.rs`) builds its client and maps its answers with the same pieces.
//!
//! A zone gets two HTTP clients. Listings, deletes and the reachability probe are short
//! requests and share one with a total timeout, [`BunnyConfig::request_timeout`]; a listing has
//! its own, usually shorter, [`BunnyConfig::list_timeout`]. Uploads and downloads can take far
//! longer on a slow link, so they use a client with no total timeout and are abandoned only
//! when they stop making progress (see `transfer.rs`).
//!
//! Callers own retries: every operation runs under [`BunnyConfig::retry`] (a listing under
//! [`BunnyConfig::list_retry`] when set), which [`BunnyConfig::for_site`] makes a single attempt.

use std::error::Error as _;
use std::fmt;
use std::num::IntErrorKind;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use httporter_core::{
    AuthHint, ByteStream, Capabilities, ChecksumKind, EmptyFolders, HttpProtocol, Listing,
    MAX_RETRY_AFTER, ObjectInfo, ProviderError, ProxyMode, RemotePath, RetryPolicy,
    StorageProvider, TransferSettings, UploadOptions, UploadSource, encode_segment, with_retry,
};
use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use reqwest::{RequestBuilder, StatusCode, redirect};
use tokio::sync::Semaphore;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a short request may take, and a listing by default.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an upload or download may make no progress by default.
const TRANSFER_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long one try at a listing may take on the client a site uses.
const SITE_LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// The shortest request and idle timeouts used, whatever is configured (ledger 181): a zero
/// idle timeout would cut off every transfer before it began.
const MIN_TIMEOUT: Duration = Duration::from_secs(5);
const USER_AGENT: &str = "HTTPorter/0.1";
/// The header the zone password travels in (and the account API key, see `account.rs`).
pub(crate) const ACCESS_KEY: HeaderName = HeaderName::from_static("accesskey");
/// How much of an answer is read when only its status matters, or to quote it.
pub(crate) const ERROR_BODY_MAX: usize = 4 * 1024;
/// How much of it can end up in an error message.
const ERROR_SNIPPET_MAX: usize = 200;

/// Directory listings the service tolerates at once before it starts answering "busy".
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
    /// How long an upload or download may make no progress before it is abandoned. Transfers
    /// have no overall time limit. At least 5 s, whatever is set here.
    pub transfer_idle_timeout: Duration,
    /// How long one try at a listing may take in all, from connecting to the last byte of the
    /// answer.
    pub list_timeout: Duration,
    /// How long a short request (a delete, the reachability probe) may take in all, and how
    /// long the server has to answer an upload once the body is sent. At least 5 s.
    pub request_timeout: Duration,
    /// The retries for every operation, and for listings unless [`Self::list_retry`] is set.
    pub retry: RetryPolicy,
    /// The retries for listings, when they should differ from [`Self::retry`] (a folder
    /// someone is waiting for gives up sooner than a transfer).
    pub list_retry: Option<RetryPolicy>,
    /// Never offer HTTP/2, only HTTP/1.1.
    pub http1_only: bool,
    /// Keep connections open between requests. Off, every request opens a new one.
    pub keep_alive: bool,
    /// How requests reach the internet.
    pub proxy: ProxyMode,
    /// Replaces `https://{host}`, for tests and local servers. Such an endpoint is never
    /// reached through the Windows proxy (Ruling P15); a custom proxy still applies.
    pub base_url: Option<String>,
}

impl BunnyConfig {
    /// A zone with the general defaults: up to six attempts per call (the default
    /// [`RetryPolicy`]), 120 s limits, HTTP/2 when offered, kept-alive connections and the
    /// Windows proxy.
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
            request_timeout: REQUEST_TIMEOUT,
            retry: RetryPolicy::default(),
            list_retry: None,
            http1_only: false,
            keep_alive: true,
            proxy: ProxyMode::System,
            base_url: None,
        }
    }

    /// The one client a site uses for browsing and transfers: single attempt per call (callers
    /// own retries), listings time out after 20 s, transfers after `transfer_idle_timeout` of
    /// no progress.
    ///
    /// The Transfers settings decide the rest: transfers at once (at most 96), the request
    /// timeout (which is also the idle timeout), the protocol, keep-alive and the proxy.
    ///
    /// An upload gets extra time once its whole body has been handed to the connection: the
    /// idle timeout again, for what the connection buffered to cross the link unseen, plus the
    /// request timeout for the server to answer.
    ///
    /// Very slow links can still be cut off (Ruling Q35). Mid-body, that is a link moving less
    /// than about one 64 KiB piece per idle period. At the end, it is one that can't drain the
    /// buffered tail (a few hundred KiB) in the extra time: a few KiB/s. At the Settings
    /// choices (60, 120 or 300 s) the worst case is about 1 KiB/s mid-body and 3 KiB/s for the
    /// tail. It bites mainly at the shorter timeouts only hand-edited settings reach (5 s
    /// floor).
    pub fn for_site(
        zone: impl Into<String>,
        host: impl Into<String>,
        password: impl Into<String>,
        s: &TransferSettings,
    ) -> Self {
        Self {
            transfer_concurrency: s.transfer_limit(TRANSFER_CONCURRENCY_MAX),
            transfer_idle_timeout: s.request_timeout(),
            list_timeout: SITE_LIST_TIMEOUT,
            request_timeout: s.request_timeout(),
            retry: RetryPolicy::immediate(0),
            list_retry: None,
            http1_only: s.protocol == HttpProtocol::Http1Only,
            keep_alive: s.keep_alive,
            proxy: s.proxy.clone(),
            ..Self::new(zone, host, password)
        }
    }
}

impl fmt::Debug for BunnyConfig {
    /// Leaves the password out so a config can be logged safely (a proxy URL with credentials
    /// is redacted by [`ProxyMode`]'s own `Debug`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BunnyConfig")
            .field("zone", &self.zone)
            .field("host", &self.host)
            .field("password", &"<redacted>")
            .field("transfer_concurrency", &self.transfer_concurrency)
            .field("delete_concurrency", &self.delete_concurrency)
            .field("transfer_idle_timeout", &self.transfer_idle_timeout)
            .field("list_timeout", &self.list_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("retry", &self.retry)
            .field("list_retry", &self.list_retry)
            .field("http1_only", &self.http1_only)
            .field("keep_alive", &self.keep_alive)
            .field("proxy", &self.proxy)
            .field("base_url", &self.base_url)
            .finish()
    }
}

pub struct BunnyStorage {
    /// For listings, deletes and the probe: bounded by `request_timeout`, and a listing by
    /// `list_timeout`.
    pub(crate) client: reqwest::Client,
    /// For uploads and downloads: no total timeout, see `transfer.rs`.
    pub(crate) transfer_client: reqwest::Client,
    /// The zone password, marked sensitive. Every request but the probe carries it.
    access_key: HeaderValue,
    /// `scheme://host[:port]` with no trailing slash.
    base: String,
    zone: String,
    pub(crate) retry: RetryPolicy,
    pub(crate) list_retry: RetryPolicy,
    pub(crate) list_timeout: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) transfer_idle_timeout: Duration,
    capabilities: Capabilities,
    pub(crate) list_slots: Semaphore,
    delete_slots: Semaphore,
    /// Shared by uploads and downloads. Owned permits, because a download keeps its slot for
    /// as long as its body stream is alive.
    pub(crate) transfer_slots: Arc<Semaphore>,
}

impl BunnyStorage {
    pub fn new(cfg: BunnyConfig) -> Result<Self, ProviderError> {
        let mut access_key = HeaderValue::from_str(&cfg.password).map_err(|_| {
            ProviderError::Permanent("the storage password cannot be sent as an HTTP header".into())
        })?;
        access_key.set_sensitive(true);

        // A semaphore with no permits would stall every operation forever.
        let delete_concurrency = cfg.delete_concurrency.clamp(1, DELETE_CONCURRENCY_MAX);
        let transfer_concurrency = cfg.transfer_concurrency.clamp(1, TRANSFER_CONCURRENCY_MAX);
        let request_timeout = cfg.request_timeout.max(MIN_TIMEOUT);

        let connections = Connections {
            http1_only: cfg.http1_only,
            keep_alive: cfg.keep_alive,
            proxy: effective_proxy(&cfg.proxy, cfg.base_url.as_deref()),
        };
        let client = build_client(
            &connections,
            LIST_CONCURRENCY + delete_concurrency,
            Some(request_timeout),
        )?;
        let transfer_client = build_client(&connections, transfer_concurrency, None)?;

        let base = match &cfg.base_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => format!("https://{}", cfg.host),
        };

        Ok(Self {
            client,
            transfer_client,
            access_key,
            base,
            zone: cfg.zone,
            list_retry: cfg.list_retry.unwrap_or_else(|| cfg.retry.clone()),
            retry: cfg.retry,
            list_timeout: cfg.list_timeout,
            request_timeout,
            transfer_idle_timeout: cfg.transfer_idle_timeout.max(MIN_TIMEOUT),
            capabilities: Capabilities {
                checksum_in_listing: Some(ChecksumKind::Sha256),
                server_side_copy: false,
                recursive_delete: true,
                batch_delete_max: 1,
                // Live probe (Task 7): a folder made with `create_folder`, or one whose last
                // file goes, stays and lists as `[]`.
                empty_folders: EmptyFolders::Supported,
                list_concurrency: LIST_CONCURRENCY,
                delete_concurrency,
                transfer_concurrency_max: TRANSFER_CONCURRENCY_MAX,
                presign: false,
                read_only: false,
                // Live probe (ruling Q55, `tests/live.rs`): when Bunny ends an upload cut off
                // mid-body (at once when it sees the connection close, about 60 s after the last
                // bytes when it doesn't), it deletes the path, a newer upload answered 201
                // included. Acceptance S5 lost 2-3 files of a resume this way.
                cut_uploads_delete_late: true,
            },
            list_slots: Semaphore::new(LIST_CONCURRENCY),
            delete_slots: Semaphore::new(delete_concurrency),
            transfer_slots: Arc::new(Semaphore::new(transfer_concurrency)),
        })
    }

    /// `request` with the zone password attached.
    pub(crate) fn with_key(&self, request: RequestBuilder) -> RequestBuilder {
        request.header(ACCESS_KEY, self.access_key.clone())
    }

    /// The URL of a directory: the zone, then the encoded path, then a trailing slash (which
    /// is what makes Bunny list rather than fetch it).
    pub(crate) fn dir_url(&self, dir: &RemotePath) -> String {
        format!(
            "{}/{}/{}",
            self.base,
            encode_segment(&self.zone),
            dir.to_url_path(true)
        )
    }

    /// The URL of a file: like [`Self::dir_url`] without the trailing slash. The root has no
    /// such URL, and sending the zone's own URL to a file operation (a `DELETE` above all)
    /// would act on the whole zone.
    pub(crate) fn file_url(&self, file: &RemotePath) -> Result<String, ProviderError> {
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

    /// One attempt at a `DELETE`.
    async fn delete_once(&self, url: &str) -> Result<(), ProviderError> {
        let _slot = self
            .delete_slots
            .acquire()
            .await
            .map_err(|_| ProviderError::Permanent("the delete limiter was closed".into()))?;

        let mut response = self
            .with_key(self.client.delete(url))
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        // Read the answer whatever it says, so the connection can carry the next delete
        // (ledger 143). It says nothing the status doesn't.
        let _ = read_capped(&mut response, ERROR_BODY_MAX).await;
        if status.is_success() {
            Ok(())
        } else {
            Err(map_status(status, retry_after, Op::Delete))
        }
    }

    /// One attempt at making the folder `dir`, whose URL is `url`.
    async fn create_folder_once(&self, url: &str, dir: &RemotePath) -> Result<(), ProviderError> {
        let mut response = self
            .with_key(self.client.put(url))
            // Declared, so the empty body goes with a length rather than none.
            .header(CONTENT_LENGTH, 0)
            .body(Vec::new())
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        // Read the answer whatever it says (ledger 143); only the body tells a busy 400.
        let body = read_capped(&mut response, ERROR_BODY_MAX)
            .await
            .map_err(network_error)?;
        if status.is_success() {
            return Ok(());
        }
        if status != StatusCode::BAD_REQUEST {
            return Err(map_status(status, retry_after, Op::CreateFolder));
        }
        match map_400(&body, Op::CreateFolder) {
            busy @ ProviderError::Server { .. } => Err(busy),
            // The folder may already be there: only the parent's listing can tell.
            rejected => {
                if self.has_folder(dir).await? {
                    Ok(())
                } else {
                    Err(rejected)
                }
            }
        }
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
        self.list_dir(dir).await
    }

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        self.upload_to(path, &source, opts).await
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        self.download_from(path).await
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

    /// `PUT /{zone}/{dir}/` with an empty body. Bunny answers `201 "Directory created."` and
    /// makes the missing parents too; the folder then lists, and stays while it is empty (live
    /// probe B1, Task 7). The zone root is always there.
    ///
    /// A folder that is already there is refused with `400 "Unable to upload file. Invalid path
    /// specified."`, the same words as for a path the service won't take, so a 400 is settled
    /// by the parent's listing: when it shows the folder, the folder is there, which is all the
    /// caller asked for. Bunny's busy 400 is retried like any other.
    ///
    /// A file of the same name does not stop the `PUT`: Bunny then lists a file and a folder
    /// under one name. As for S3 (ruling Q34), keeping them apart is the planner's job, which
    /// knows the listing it works from; checking here would cost a listing per folder.
    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        if dir.is_root() {
            return Ok(());
        }
        let url = self.dir_url(dir);
        let url = url.as_str();
        with_retry(&self.retry, move |_| self.create_folder_once(url, dir)).await
    }

    /// `GET https://{host}/` without the password: any answer at all means the service is
    /// reachable. Tried once, within the listing time limit.
    async fn reachable(&self) -> Result<(), ProviderError> {
        let mut response = self
            .client
            .get(format!("{}/", self.base))
            .timeout(self.list_timeout)
            .send()
            .await
            .map_err(network_error)?;
        let _ = read_capped(&mut response, ERROR_BODY_MAX).await;
        Ok(())
    }

    /// The parent's listing entry plus `DateCreated`. `copy` and `presign` keep the defaults:
    /// Bunny Storage can do neither. `UploadOptions::cache_control` is not sent either: the
    /// pull zone decides how files are cached.
    async fn stat(&self, path: &RemotePath) -> Result<ObjectInfo, ProviderError> {
        self.stat_path(path).await
    }
}

/// The kind of request a response answers, which decides what a 400 means. `Account` is any
/// call to the account API (`api.bunny.net`), where a 400 is a plain rejected request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    List,
    Download,
    Upload {
        with_checksum: bool,
    },
    Delete,
    /// `PUT /{zone}/{dir}/`, see `create_folder`.
    CreateFolder,
    Account,
}

/// Turns a non-success HTTP status into the error the engine acts on.
pub(crate) fn map_status(
    status: StatusCode,
    retry_after: Option<Duration>,
    op: Op,
) -> ProviderError {
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
            Op::Upload { .. } | Op::Download | Op::CreateFolder | Op::Account => {
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

/// Bunny's "Unable to process request": the server is overloaded, so the request is worth
/// another try, and fewer at once.
fn server_busy() -> ProviderError {
    ProviderError::Server {
        status: 400,
        detail: "busy".into(),
    }
}

/// What a 400 answering an upload means. Bunny says "Unable to process request" when it is
/// overloaded, which is worth another try; any other 400 rejected the upload itself. A
/// `Checksum` that does not match the body gets `{"HttpCode":400,"Message":"Unable to upload
/// file: Exception when writing file: Checksum missmatch, skipping commit"}` (live probe, Task 7),
/// and the file is not stored.
pub(crate) fn map_upload_400(body: &[u8], with_checksum: bool) -> ProviderError {
    map_400(body, Op::Upload { with_checksum })
}

/// What a 400 answering `op` means, from its body: busy when Bunny says "Unable to process
/// request" (overloaded, worth another try), otherwise what [`map_status`] makes of a 400 for
/// `op`, with the server's words added when that is a plain rejection.
pub(crate) fn map_400(body: &[u8], op: Op) -> ProviderError {
    let body = String::from_utf8_lossy(body);
    if body
        .to_ascii_lowercase()
        .contains("unable to process request")
    {
        return server_busy();
    }
    match map_status(StatusCode::BAD_REQUEST, None, op) {
        // Without a checksum there is nothing to blame, so say what the server said. It is
        // server text, never anything we sent.
        ProviderError::Permanent(message) => match snippet(&body) {
            Some(snippet) => ProviderError::Permanent(format!("{message}: {snippet}")),
            None => ProviderError::Permanent(message),
        },
        other => other,
    }
}

/// Server text fit for an error message: control characters and bidi overrides (which could
/// garble or disguise what the UI shows) become spaces, runs of whitespace one space, and only
/// the first [`ERROR_SNIPPET_MAX`] characters are kept (then "..."). `None` when nothing is left.
pub(crate) fn snippet(text: &str) -> Option<String> {
    let cleaned: String = text
        .chars()
        .map(|c| if is_unsafe_in_text(c) { ' ' } else { c })
        .collect();
    let words = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let kept: String = words.chars().take(ERROR_SNIPPET_MAX).collect();
    if kept.is_empty() {
        None
    } else if kept.len() < words.len() {
        Some(format!("{kept}..."))
    } else {
        Some(kept)
    }
}

/// Control characters, and the invisible marks that reorder or hide the text around them.
fn is_unsafe_in_text(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
        )
}

/// Up to `max` bytes of a response body. It reads on until the body ends or `max` is passed,
/// so a short body is read to its end and its connection can carry the next request; a longer
/// one is abandoned, and its connection with it.
pub(crate) async fn read_capped(
    response: &mut reqwest::Response,
    max: usize,
) -> reqwest::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut seen = 0;
    while seen <= max {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        seen += chunk.len();
        let room = max - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    Ok(body)
}

/// `Retry-After` as a number of seconds, at most [`MAX_RETRY_AFTER`] (Ruling Q17: the
/// caller's policy decides whether to wait); the HTTP-date form is ignored.
pub(crate) fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let wait = match value.parse::<u64>() {
        Ok(seconds) => Duration::from_secs(seconds),
        // Too big for a number is still a very long wait.
        Err(e) if *e.kind() == IntErrorKind::PosOverflow => MAX_RETRY_AFTER,
        Err(_) => return None,
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

/// A request that never produced a usable response (no connection, a reset, a timeout): a
/// network error, worth another try, unless the request itself could not be built.
pub(crate) fn network_error(error: reqwest::Error) -> ProviderError {
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

/// What the HTTP clients of one zone (or of the account API client) have in common.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Connections {
    pub(crate) http1_only: bool,
    pub(crate) keep_alive: bool,
    pub(crate) proxy: ProxyMode,
}

/// The proxy setting a client follows. Ruling P15: a `base_url` override (tests, local
/// servers) is never routed through the Windows proxy; a proxy chosen by hand still applies.
pub(crate) fn effective_proxy(proxy: &ProxyMode, base_url: Option<&str>) -> ProxyMode {
    match proxy {
        ProxyMode::System if base_url.is_some() => ProxyMode::None,
        other => other.clone(),
    }
}

/// Every client carries a user agent, never follows redirects (one would forward the access
/// key to whatever host it names) and gives up connecting after 10 s. Only the short-request
/// client has a total timeout; see the module docs.
fn client_builder(
    connections: &Connections,
    pool_size: usize,
    total_timeout: Option<Duration>,
) -> Result<reqwest::ClientBuilder, ProviderError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        // Without keep-alive no connection waits in the pool, so each request opens its own.
        .pool_max_idle_per_host(if connections.keep_alive { pool_size } else { 0 })
        .redirect(redirect::Policy::none());
    if let Some(timeout) = total_timeout {
        builder = builder.timeout(timeout);
    }
    if connections.http1_only {
        builder = builder.http1_only();
    }
    builder = match &connections.proxy {
        // reqwest's default: the Windows settings and the environment.
        ProxyMode::System => builder,
        ProxyMode::None => builder.no_proxy(),
        ProxyMode::Custom(url) => {
            // The URL may hold a password, so the message doesn't repeat it.
            let proxy = reqwest::Proxy::all(url.as_str()).map_err(|_| {
                ProviderError::Permanent("the custom proxy address is not a valid URL".into())
            })?;
            builder.proxy(proxy)
        }
    };
    Ok(builder)
}

pub(crate) fn build_client(
    connections: &Connections,
    pool_size: usize,
    total_timeout: Option<Duration>,
) -> Result<reqwest::Client, ProviderError> {
    client_builder(connections, pool_size, total_timeout)?
        .build()
        .map_err(|e| ProviderError::Permanent(format!("could not create the HTTP client: {e}")))
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
        for op in [
            Op::List,
            Op::Download,
            Op::Delete,
            Op::CreateFolder,
            Op::Account,
        ] {
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
            Op::CreateFolder,
            Op::Account,
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

    /// Bunny's own words for a `Checksum` that does not match, as the live probe (Task 7) saw
    /// them: an upload failure, not the busy answer, although both begin "Unable to".
    #[test]
    fn bunnys_checksum_mismatch_answer_is_a_mismatch_not_busy() {
        let body = r#"{"HttpCode":400,"Message":"Unable to upload file: Exception when writing file: Checksum missmatch, skipping commit"}"#;
        assert_eq!(
            map_upload_400(body.as_bytes(), true),
            ProviderError::ChecksumMismatch
        );
    }

    /// Bunny's answer to a folder `PUT` it won't take (the folder is there already, or the path
    /// is not allowed) is a plain rejection that quotes it; its busy answer stays busy.
    #[test]
    fn a_create_folder_400_quotes_the_server_unless_busy() {
        let body = r#"{"HttpCode":400,"Message":"Unable to upload file. Invalid path specified."}"#;
        match map_400(body.as_bytes(), Op::CreateFolder) {
            ProviderError::Permanent(message) => {
                assert!(message.starts_with(REJECTED), "{message}");
                assert!(message.contains("Invalid path specified."), "{message}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        assert_eq!(
            map_400(b"Unable to process request", Op::CreateFolder),
            busy()
        );
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

    fn quoted(body: &str) -> String {
        match map_upload_400(body.as_bytes(), false) {
            ProviderError::Permanent(message) => message,
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    const REJECTED: &str = "request rejected by server (400 Bad Request)";

    #[test]
    fn an_unchecksummed_upload_400_quotes_a_short_trimmed_snippet() {
        assert_eq!(quoted(""), REJECTED);
        assert_eq!(quoted(" \n\t "), REJECTED);
        assert_eq!(quoted("Bad thing"), format!("{REJECTED}: Bad thing"));
        assert_eq!(
            quoted("  Bad\r\n  thing\n"),
            format!("{REJECTED}: Bad thing"),
            "trimmed, with runs of whitespace collapsed"
        );
        assert_eq!(
            quoted(&"x".repeat(5000)),
            format!("{REJECTED}: {}...", "x".repeat(ERROR_SNIPPET_MAX))
        );
        // Exactly the limit is not cut.
        assert_eq!(
            quoted(&"y".repeat(ERROR_SNIPPET_MAX)),
            format!("{REJECTED}: {}", "y".repeat(ERROR_SNIPPET_MAX))
        );
    }

    /// Ledger 187: server text reaches the UI, so terminal escapes, NULs and bidi overrides in
    /// it don't.
    #[test]
    fn the_snippet_strips_control_characters() {
        assert_eq!(
            quoted("Bad\u{1b}[31m thing\u{0}"),
            format!("{REJECTED}: Bad [31m thing")
        );
        assert_eq!(
            quoted("evil\u{202e}txt.exe\u{7f}"),
            format!("{REJECTED}: evil txt.exe")
        );
        assert_eq!(quoted("\u{1b}\u{0}\u{7}"), REJECTED);
        assert_eq!(
            quoted("caf\u{e9} \u{2014} ok"),
            format!("{REJECTED}: café, ok")
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

    fn retry_after(value: &'static str) -> Option<Duration> {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
        parse_retry_after(&headers)
    }

    #[test]
    fn retry_after_reads_whole_seconds_only() {
        assert_eq!(retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_is_clamped_to_five_minutes() {
        let max = Some(Duration::from_secs(300));
        assert_eq!(retry_after("99999999999"), max);
        // Too large even for a u64 is still "a very long time", not "no hint".
        assert_eq!(retry_after("99999999999999999999999999"), max);
        assert_eq!(retry_after("301"), max);
        assert_eq!(retry_after("300"), max);
        assert_eq!(retry_after("299"), Some(Duration::from_secs(299)));
    }

    fn connections() -> Connections {
        Connections {
            http1_only: false,
            keep_alive: true,
            proxy: ProxyMode::None,
        }
    }

    /// reqwest offers no way to read a client's settings back, so this looks at the builder's
    /// `Debug` output.
    fn described(connections: &Connections) -> String {
        format!("{:?}", client_builder(connections, 4, None).unwrap())
    }

    #[test]
    fn http1_only_setting_disables_h2() {
        let h1 = Connections {
            http1_only: true,
            ..connections()
        };
        assert!(
            described(&h1).contains("http1_only: true"),
            "{}",
            described(&h1)
        );
        assert!(!described(&connections()).contains("http1_only"));
        build_client(&h1, 4, None).unwrap();

        let settings = |protocol| TransferSettings {
            protocol,
            ..TransferSettings::default()
        };
        let cfg = |protocol| BunnyConfig::for_site("z", "h", "p", &settings(protocol));
        assert!(cfg(HttpProtocol::Http1Only).http1_only);
        assert!(!cfg(HttpProtocol::Http2WhenAvailable).http1_only);
    }

    #[test]
    fn proxy_none_and_custom_are_honoured() {
        let custom = ProxyMode::Custom("http://proxy.test:8080".into());
        // What the clients follow, with and without a test endpoint (Ruling P15).
        let local = Some("http://127.0.0.1:9");
        assert_eq!(effective_proxy(&ProxyMode::None, None), ProxyMode::None);
        assert_eq!(effective_proxy(&ProxyMode::None, local), ProxyMode::None);
        assert_eq!(effective_proxy(&custom, None), custom);
        assert_eq!(effective_proxy(&custom, local), custom);
        assert_eq!(effective_proxy(&ProxyMode::System, None), ProxyMode::System);
        assert_eq!(effective_proxy(&ProxyMode::System, local), ProxyMode::None);

        // Both clients of a zone, as built. `build()` adds the Windows proxy unless `no_proxy()`
        // or a custom proxy turned it off, and a built client's `Debug` lists its proxies.
        let clients = |proxy: &ProxyMode, base_url: Option<&str>| {
            let mut cfg = BunnyConfig::new("zone", "storage.bunnycdn.com", "secret");
            cfg.proxy = proxy.clone();
            cfg.base_url = base_url.map(str::to_owned);
            let storage = BunnyStorage::new(cfg).unwrap();
            [
                format!("{:?}", storage.client),
                format!("{:?}", storage.transfer_client),
            ]
        };
        // Not printed: it would show this machine's proxy settings.
        for client in clients(&ProxyMode::System, None) {
            assert!(client.contains("proxies"), "System keeps the Windows proxy");
        }
        for client in clients(&ProxyMode::None, None) {
            assert!(!client.contains("proxies"), "{client}");
        }
        // Ruling P15: a test endpoint never goes through the Windows proxy.
        for client in clients(&ProxyMode::System, local) {
            assert!(!client.contains("proxies"), "{client}");
        }
        for base_url in [None, local] {
            for client in clients(&custom, base_url) {
                assert!(client.contains("proxies"), "{client}");
                assert!(client.contains("proxy.test:8080"), "{client}");
            }
        }

        // A bad address is refused, without repeating it (it may hold a password).
        let with = |proxy: &ProxyMode| Connections {
            proxy: proxy.clone(),
            ..connections()
        };
        let bad = ProxyMode::Custom("http://user:s3cret@[::1".into());
        match client_builder(&with(&bad), 4, None) {
            Err(ProviderError::Permanent(m)) => assert!(!m.contains("s3cret"), "{m}"),
            Err(other) => panic!("expected Permanent, got {other:?}"),
            Ok(_) => panic!("expected an error"),
        }
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
        let bounded = build_client(&connections(), 1, timeout).unwrap();
        let unbounded = build_client(&connections(), 1, None).unwrap();

        let error = bounded.get(server.uri()).send().await.unwrap_err();
        assert!(error.is_timeout(), "{error:?}");
        let response = unbounded.get(server.uri()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn request_and_idle_timeouts_are_at_least_five_seconds() {
        let storage = |timeout| {
            let mut cfg = BunnyConfig::new("zone", "storage.bunnycdn.com", "secret");
            cfg.request_timeout = timeout;
            cfg.transfer_idle_timeout = timeout;
            BunnyStorage::new(cfg).unwrap()
        };
        for (set, used) in [(0, 5), (1, 5), (5, 5), (6, 6), (300, 300)] {
            let s = storage(Duration::from_secs(set));
            assert_eq!(s.request_timeout, Duration::from_secs(used), "{set}");
            assert_eq!(s.transfer_idle_timeout, Duration::from_secs(used), "{set}");
        }
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

    #[test]
    fn the_access_key_is_sensitive() {
        let storage = BunnyStorage::new(BunnyConfig::new("zone", "h", "hunter2")).unwrap();
        assert!(storage.access_key.is_sensitive());
        let request = storage
            .with_key(storage.client.get("http://127.0.0.1:9/"))
            .build()
            .unwrap();
        assert!(request.headers()[ACCESS_KEY].is_sensitive());
        assert!(!format!("{:?}", request.headers()).contains("hunter2"));
    }
}
