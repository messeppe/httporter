//! The Bunny Storage HTTP client: request building, per-type concurrency
//! limits, retries, latency reporting and HTTP-status error mapping.
//!
//! Only listing is implemented so far; the transfer and delete operations
//! arrive with the next task.

use std::error::Error as _;
use std::fmt;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use httporter_core::{
    ByteStream, Capabilities, ChecksumKind, EmptyFolders, Entry, Listing, ProviderError,
    RemotePath, RequestInfo, RetryPolicy, StorageProvider, UploadOptions, UploadSource,
    encode_segment, with_retry,
};
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use reqwest::{StatusCode, Version, redirect};
use tokio::sync::Semaphore;

use crate::model::{BunnyObject, to_entry};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const USER_AGENT: &str = "HTTPorter/0.1";

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
    pub transfer_concurrency: usize,
    /// Capped at 30 in the advertised capabilities.
    pub delete_concurrency: usize,
    pub retry: RetryPolicy,
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
            retry: RetryPolicy::default(),
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
            .field("retry", &self.retry)
            .field("base_url", &self.base_url)
            .finish()
    }
}

pub struct BunnyStorage {
    client: reqwest::Client,
    /// `scheme://host[:port]` with no trailing slash.
    base: String,
    zone: String,
    retry: RetryPolicy,
    capabilities: Capabilities,
    list_slots: Semaphore,
}

impl BunnyStorage {
    pub fn new(cfg: BunnyConfig) -> Result<Self, ProviderError> {
        let mut access_key = HeaderValue::from_str(&cfg.password).map_err(|_| {
            ProviderError::Permanent("the storage password cannot be sent as an HTTP header".into())
        })?;
        access_key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static("accesskey"), access_key);

        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(cfg.transfer_concurrency)
            // The API never redirects; following one would forward the
            // access key to whatever host it names.
            .redirect(redirect::Policy::none())
            .default_headers(headers)
            .build()
            .map_err(|e| {
                ProviderError::Permanent(format!("could not create the HTTP client: {e}"))
            })?;

        let base = match &cfg.base_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => format!("https://{}", cfg.host),
        };

        Ok(Self {
            client,
            base,
            zone: cfg.zone,
            retry: cfg.retry,
            capabilities: Capabilities {
                checksum_in_listing: Some(ChecksumKind::Sha256),
                server_side_copy: false,
                recursive_delete: true,
                batch_delete_max: 1,
                empty_folders: EmptyFolders::Unknown,
                list_concurrency: LIST_CONCURRENCY,
                delete_concurrency: cfg.delete_concurrency.min(DELETE_CONCURRENCY_MAX),
                transfer_concurrency_max: TRANSFER_CONCURRENCY_MAX,
                presign: false,
                read_only: false,
            },
            list_slots: Semaphore::new(LIST_CONCURRENCY),
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
        let (objects, info) = with_retry(&self.retry, move |_| self.fetch_listing(url)).await?;
        Ok(Listing {
            entries: to_entries(objects, dir)?,
            info,
        })
    }

    async fn upload(
        &self,
        _path: &RemotePath,
        _source: UploadSource,
        _opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        Err(ProviderError::Permanent("not implemented".into()))
    }

    async fn download(&self, _path: &RemotePath) -> Result<ByteStream, ProviderError> {
        Err(ProviderError::Permanent("not implemented".into()))
    }

    async fn delete_file(&self, _path: &RemotePath) -> Result<(), ProviderError> {
        Err(ProviderError::Permanent("not implemented".into()))
    }

    async fn delete_folder(&self, _dir: &RemotePath) -> Result<(), ProviderError> {
        Err(ProviderError::Permanent("not implemented".into()))
    }
}

/// The kind of request a response answers, which decides what a 400 means.
// `Download`, `Upload` and `Delete` are constructed by the transfer and
// delete operations that arrive with the next task.
#[allow(dead_code)]
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
        401 | 403 => ProviderError::Auth,
        404 => ProviderError::NotFound,
        429 => ProviderError::RateLimited { retry_after },
        // Bunny answers 400 "Unable to process request" when it is overloaded.
        400 => match op {
            Op::List | Op::Delete => ProviderError::Transient("server busy".into()),
            Op::Upload {
                with_checksum: true,
            } => ProviderError::ChecksumMismatch,
            Op::Upload { .. } | Op::Download => {
                ProviderError::Permanent(format!("request rejected by server ({status})"))
            }
        },
        500..=599 => ProviderError::Transient(format!("server error ({status})")),
        _ => ProviderError::Permanent(format!("unexpected response from server ({status})")),
    }
}

/// `Retry-After` as a number of seconds; the HTTP-date form is ignored.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    }
}

/// A request that never produced a usable response: worth another try, unless
/// the request itself could not be built.
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
        ProviderError::Transient(message)
    }
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

    #[test]
    fn credentials_and_missing_things_map_directly() {
        for op in [Op::List, Op::Download, Op::Delete] {
            assert_eq!(map_status(status(401), None, op), ProviderError::Auth);
            assert_eq!(map_status(status(403), None, op), ProviderError::Auth);
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
        let busy = ProviderError::Transient("server busy".into());
        assert_eq!(map_status(status(400), None, Op::List), busy);
        assert_eq!(map_status(status(400), None, Op::Delete), busy);
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
    fn server_errors_are_transient_and_everything_else_is_permanent() {
        for code in [500, 502, 503, 599] {
            assert!(
                matches!(
                    map_status(status(code), None, Op::List),
                    ProviderError::Transient(_)
                ),
                "{code}"
            );
        }
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
