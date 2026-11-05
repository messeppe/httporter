//! The S3 provider: [`S3Storage`] speaks the S3 REST API (path-style or virtual-hosted) to AWS
//! and to S3-compatible services, signing every request with SigV4.
//!
//! **One attempt per call.** Whoever calls the provider owns the retries (the transfer engine
//! with its policy, a folder listing with a short one), so nothing here loops on a failure.
//! A server's `Retry-After` is clamped to [`httporter_core::MAX_RETRY_AFTER`] before it is
//! returned.
//!
//! **Two HTTP clients.** Listings, deletes and the like are short requests and share a client
//! with a total timeout ([`S3Config::request_timeout`], and [`S3Config::list_timeout`] for
//! listings). Uploads and downloads can legitimately take long on a slow link, so they use a
//! client with no total timeout and are abandoned only when they stop making progress for
//! [`S3Config::transfer_idle_timeout`] (see `body.rs`). Neither follows redirects, so a request
//! is never replayed against another host; the `Authorization` header and the session token are
//! marked sensitive, which keeps them out of debug output.
//!
//! **Large files.** A file of more than [`MULTIPART_THRESHOLD`] is uploaded in parts, several
//! side by side, and resumed from the part journal after an interruption; see
//! [`multipart`](crate::multipart).
//!
//! **Where S3 differs from the [`StorageProvider`] contract**
//! - S3 has no folders, only keys. A folder exists while a key starts with `name/`, so
//!   [`list`](StorageProvider::list) answers [`ProviderError::NotFound`] for a folder with no
//!   key under it, and a folder made with `create_folder` is a zero-byte `name/` marker.
//! - `DeleteObject` succeeds for a key that is not there, so deleting a missing file is `Ok`,
//!   not `NotFound`. (A `HEAD` first would double the requests for an answer nobody needs.)
//! - A file and a folder may share a name (`a` and `a/b`), and an upload does not check for
//!   it: that would cost a request per file. Both then appear in listings.
//! - A key `RemotePath` cannot represent is left out of listings, and counted; see
//!   [`S3Storage::skipped_keys`].

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use futures_util::future::join_all;
use futures_util::stream::FuturesUnordered;
use httporter_core::{
    ByteStream, Capabilities, Checksum, ChecksumKind, EmptyFolders, Entry, HttpProtocol, Listing,
    ProviderError, ProxyMode, RemotePath, RequestInfo, StorageProvider, TransferSettings,
    UploadOptions, UploadSource, encode_segment,
};
use md5::{Digest, Md5};
use percent_encoding::percent_decode_str;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Body, Method, Version, redirect};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use url::Url;

use crate::body::{guard_download, read_capped, read_limited, until_stalled, upload_body};
use crate::errors::{self, ERROR_BODY_MAX, network_error};
use crate::multipart::{MULTIPART_THRESHOLD, PART_SIZE};
use crate::presets::{Endpoint, bucket_url, object_url};
use crate::sigv4::{
    Credentials, EMPTY_PAYLOAD_SHA256, SignInput, UNSIGNED_PAYLOAD, sha256_hex, sign,
};
use crate::xml::{self, DELETE_MAX, DeleteFailure, ListPage};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "HTTPorter/0.1";

/// Listings in flight at once (spec: S3 8).
const LIST_CONCURRENCY: usize = 8;
/// Delete requests in flight at once (spec: S3 8, each one carrying up to 1,000 keys).
const DELETE_CONCURRENCY: usize = 8;
/// Uploads and downloads in flight at once, at most (spec: S3 128).
pub const TRANSFER_CONCURRENCY_MAX: usize = 128;

/// The largest answer to a listing, a bucket list or a delete that is read. A page of 1,000
/// keys of the longest possible length is about 2 MB.
pub(crate) const ANSWER_MAX: usize = 32 * 1024 * 1024;

/// The metadata header that carries an upload's SHA-256, for comparing files later.
pub(crate) const SHA256_META: &str = "x-amz-meta-httporter-sha256";

/// The content type of a folder marker, which is what the S3 console writes.
const FOLDER_CONTENT_TYPE: &str = "application/x-directory";

/// Everything needed to talk to one bucket.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// The bucket. May be empty for a provider that is only used to
    /// [list buckets](S3Storage::list_buckets); every other call then fails.
    pub bucket: String,
    pub endpoint: Endpoint,
    pub creds: Credentials,
    /// Uploads and downloads in flight at once; capped at 128.
    pub transfer_concurrency: usize,
    /// How long one listing request may take in all, from connecting to the last byte of the
    /// answer.
    pub list_timeout: Duration,
    /// How long an upload or download may make no progress before it is abandoned. Transfers
    /// have no overall time limit.
    pub transfer_idle_timeout: Duration,
    /// Never negotiate HTTP/2.
    pub http1_only: bool,
    /// Reuse connections between requests.
    pub keep_alive: bool,
    /// How long a short request (delete, create folder, list buckets) may take in all.
    pub request_timeout: Duration,
    pub proxy: ProxyMode,
    /// The size of the parts of a multipart upload (ruling Q25). [`PART_SIZE`] unless the
    /// service wants less: [`part_size_for`](crate::part_size_for) answers per preset, and
    /// whoever builds the configuration for a saved site sets it. Services take parts of 5 MiB to 5 GiB, so it is
    /// kept within that, and raised for a file so large that this many parts would be more than
    /// the 10,000 an upload may have. An upload resumed from the journal keeps the part size it
    /// began with, whatever this says.
    pub part_size: u64,
}

impl S3Config {
    /// The configuration for a saved site, with the Transfers settings applied: the transfer
    /// concurrency (at most what S3 allows), the protocol, keep-alive, proxy and the request
    /// timeout, which also bounds listings and the idle time of transfers.
    pub fn for_site(
        bucket: impl Into<String>,
        endpoint: Endpoint,
        creds: Credentials,
        s: &TransferSettings,
    ) -> Self {
        let timeout = s.request_timeout();
        Self {
            bucket: bucket.into(),
            endpoint,
            creds,
            transfer_concurrency: s.transfer_limit(TRANSFER_CONCURRENCY_MAX),
            list_timeout: timeout,
            transfer_idle_timeout: timeout,
            http1_only: s.protocol == HttpProtocol::Http1Only,
            keep_alive: s.keep_alive,
            request_timeout: timeout,
            proxy: s.proxy.clone(),
            part_size: PART_SIZE,
        }
    }
}

/// A [`StorageProvider`] for one bucket of AWS S3 or an S3-compatible service.
///
/// Where it deviates from the provider contract, on purpose:
/// - **Ruling Q33:** [`delete_file`](StorageProvider::delete_file) of a key that is not there
///   returns `Ok(())`, not [`ProviderError::NotFound`]. S3 answers `204` either way, and a
///   `HEAD` first would double the requests for an answer nobody needs.
/// - **Ruling Q34:** [`upload`](StorageProvider::upload) does not check whether the name is
///   already taken by a folder (or a parent of it by a file): S3 allows both `a` and `a/b`, and
///   checking would cost a request per file. Keeping the two apart is the planner's job, which
///   knows the listing it works from.
pub struct S3Storage {
    /// For listings, deletes and other short requests: bounded by `request_timeout`, and a
    /// listing by `list_timeout`.
    pub(crate) client: reqwest::Client,
    /// For uploads and downloads: no total timeout, see the module docs.
    pub(crate) transfer_client: reqwest::Client,
    bucket: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) creds: Credentials,
    list_timeout: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) transfer_idle_timeout: Duration,
    /// The part size a new multipart upload starts with, see [`S3Config::part_size`].
    pub(crate) part_size: u64,
    capabilities: Capabilities,
    list_slots: Semaphore,
    delete_slots: Semaphore,
    /// Shared by uploads and downloads. Owned permits, because a download keeps its slot for
    /// as long as its body stream is alive.
    transfer_slots: Arc<Semaphore>,
    /// Keys and prefixes left out of listings because no [`RemotePath`] can hold them.
    skipped: AtomicU64,
}

impl fmt::Debug for S3Storage {
    /// The bucket and the address only: nothing here is worth more than that in a log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Storage")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint.base.as_str())
            .finish_non_exhaustive()
    }
}

impl S3Storage {
    /// Builds the HTTP clients. Fails with [`ProviderError::Auth`] for credentials that can
    /// never work (an empty access key, say, or one that cannot be written in a header) and
    /// with [`ProviderError::Permanent`] for a proxy address that is not usable.
    pub fn new(cfg: S3Config) -> Result<Self, ProviderError> {
        let mut creds = cfg.creds;
        // An empty token is no token.
        if creds.session_token.as_deref() == Some("") {
            creds.session_token = None;
        }
        check_credentials(&creds)?;

        // A semaphore with no permits would stall every operation forever.
        let transfer_concurrency = cfg.transfer_concurrency.clamp(1, TRANSFER_CONCURRENCY_MAX);
        let client = build_client(
            &cfg.proxy,
            cfg.http1_only,
            cfg.keep_alive,
            LIST_CONCURRENCY + DELETE_CONCURRENCY,
            Some(cfg.request_timeout),
        )?;
        let transfer_client = build_client(
            &cfg.proxy,
            cfg.http1_only,
            cfg.keep_alive,
            transfer_concurrency,
            None,
        )?;

        Ok(Self {
            client,
            transfer_client,
            bucket: cfg.bucket,
            endpoint: cfg.endpoint,
            creds,
            list_timeout: cfg.list_timeout,
            request_timeout: cfg.request_timeout,
            transfer_idle_timeout: cfg.transfer_idle_timeout,
            part_size: cfg.part_size,
            capabilities: Capabilities {
                checksum_in_listing: Some(ChecksumKind::Md5),
                server_side_copy: true,
                recursive_delete: false,
                batch_delete_max: u32::try_from(DELETE_MAX).unwrap_or(1000),
                empty_folders: EmptyFolders::Markers,
                list_concurrency: LIST_CONCURRENCY,
                delete_concurrency: DELETE_CONCURRENCY,
                transfer_concurrency_max: TRANSFER_CONCURRENCY_MAX,
                presign: false, // I4
                read_only: false,
                // A PUT replaces the object whole or not at all, and a multipart upload is only
                // an object once completed (ruling Q55).
                cut_uploads_delete_late: false,
            },
            list_slots: Semaphore::new(LIST_CONCURRENCY),
            delete_slots: Semaphore::new(DELETE_CONCURRENCY),
            transfer_slots: Arc::new(Semaphore::new(transfer_concurrency)),
            skipped: AtomicU64::new(0),
        })
    }

    /// The names of the buckets these credentials can see, for the Add Site sheet. Works
    /// without a bucket in the configuration. Some keys may not list buckets at all
    /// ([`ProviderError::Auth`] with [`AuthHint::Unknown`](httporter_core::AuthHint)); the user
    /// then types the name.
    pub async fn list_buckets(&self) -> Result<Vec<String>, ProviderError> {
        let _slot = self
            .list_slots
            .acquire()
            .await
            .map_err(|_| limiter_closed("listing"))?;
        let url = bucket_url(&self.endpoint, "");
        let response = self
            .call(Method::GET, url, &[], EMPTY_PAYLOAD_SHA256, None, None)
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response, self.request_timeout).await);
        }
        let body = read_limited(response, ANSWER_MAX).await?;
        xml::parse_buckets(&body, status.as_u16())
    }

    /// How many keys and folders listings have left out so far because no [`RemotePath`] can
    /// hold them: an empty segment (`a//b`, a leading `/`), a `.` or `..` segment, or a name
    /// that is not valid UTF-8. A folder that stands for several such keys (`a//` for `a//b`
    /// and `a//c`) counts once. The count is cumulative over the provider's life and only
    /// grows; [`Listing`] has no field for it, so whoever shows a notice about skipped names
    /// reads it here, before and after a listing. Those keys stay in the bucket, and cannot be
    /// deleted through this provider either.
    pub fn skipped_keys(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    fn bucket(&self) -> Result<&str, ProviderError> {
        if self.bucket.is_empty() {
            Err(ProviderError::Permanent(
                "No bucket is set for this site.".into(),
            ))
        } else {
            Ok(&self.bucket)
        }
    }

    /// The headers for `method url`: the caller's `extra` headers (all of them signed) plus the
    /// ones the signature adds. The signature is made now, so build the request right away.
    pub(crate) fn sign_request(
        &self,
        method: &Method,
        url: &Url,
        extra: &[(String, String)],
        payload_sha256: &str,
    ) -> Result<HeaderMap, ProviderError> {
        signed_headers(
            &self.creds,
            &self.endpoint.region,
            method,
            url,
            extra,
            payload_sha256,
        )
    }

    /// Sends one signed request on the short-request client. A `timeout` replaces the
    /// client's.
    pub(crate) async fn call(
        &self,
        method: Method,
        url: Url,
        extra: &[(String, String)],
        payload_sha256: &str,
        body: Option<Vec<u8>>,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, ProviderError> {
        let headers = self.sign_request(&method, &url, extra, payload_sha256)?;
        let mut request = self.client.request(method, url).headers(headers);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some(body) = body {
            // Declared even for an empty body, which reqwest would send without a length; S3
            // refuses a `PUT` that has none.
            request = request.header(CONTENT_LENGTH, body.len()).body(body);
        }
        request.send().await.map_err(network_error)
    }

    /// One attempt at fetching and parsing a page of a listing.
    async fn list_page(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
        max_keys: Option<u32>,
    ) -> Result<(ListPage, RequestInfo), ProviderError> {
        // Held until the body is read, so at most `LIST_CONCURRENCY` listings are in flight.
        let _slot = self
            .list_slots
            .acquire()
            .await
            .map_err(|_| limiter_closed("listing"))?;

        // Queueing for a slot is not request latency.
        let started = Instant::now();
        let url = self.list_url(bucket, prefix, token, max_keys);
        let response = self
            .call(
                Method::GET,
                url,
                &[],
                EMPTY_PAYLOAD_SHA256,
                None,
                Some(self.list_timeout),
            )
            .await?;
        let status = response.status();
        let http_version = http_version(response.version());
        if !status.is_success() {
            return Err(failure(response, self.list_timeout).await);
        }
        let body = read_limited(response, ANSWER_MAX).await?;
        let latency = started.elapsed();
        Ok((
            xml::parse_list(&body, status.as_u16())?,
            RequestInfo {
                latency,
                http_version: http_version.to_owned(),
            },
        ))
    }

    /// `ListObjectsV2` for the direct children of `prefix`. Keys are asked for URL-encoded
    /// because a key may hold characters XML cannot carry; a service that does not know the
    /// parameter just answers without encoding, and says so.
    fn list_url(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
        max_keys: Option<u32>,
    ) -> Url {
        let max_keys = max_keys.map(|n| n.to_string());
        let mut pairs = vec![
            ("list-type", "2"),
            ("delimiter", "/"),
            ("encoding-type", "url"),
        ];
        if !prefix.is_empty() {
            pairs.push(("prefix", prefix));
        }
        if let Some(token) = token {
            pairs.push(("continuation-token", token));
        }
        if let Some(max_keys) = &max_keys {
            pairs.push(("max-keys", max_keys));
        }
        let query = pairs
            .iter()
            .map(|(name, value)| format!("{name}={}", encode_segment(value)))
            .collect::<Vec<_>>()
            .join("&");
        let mut url = bucket_url(&self.endpoint, bucket);
        url.set_query(Some(&query));
        url
    }

    /// Every page of the listing of `dir`, with the names this one call had to leave out. Both
    /// [`list`](StorageProvider::list) and the walk of [`delete_folder`](StorageProvider::delete_folder)
    /// are made of it.
    async fn list_dir(&self, dir: &RemotePath) -> Result<(Listing, Skips), ProviderError> {
        let bucket = self.bucket()?;
        let prefix = if dir.is_root() {
            String::new()
        } else {
            format!("{}/", dir.as_str())
        };
        let mut entries = Vec::new();
        let mut skips = Skips::default();
        let mut info = None;
        let mut exists = false;
        let mut token: Option<String> = None;
        loop {
            let (page, page_info) = self
                .list_page(bucket, &prefix, token.as_deref(), None)
                .await?;
            info.get_or_insert(page_info);
            exists |= !page.objects.is_empty() || !page.prefixes.is_empty();
            let next = page.truncated.then(|| page.next_token.clone());
            let mut page_skips = Skips::default();
            collect(dir, &prefix, page, &mut entries, &mut page_skips);
            self.skipped.fetch_add(page_skips.count, Ordering::Relaxed);
            skips.merge(page_skips);
            match next {
                None => break,
                Some(None) => {
                    return Err(ProviderError::Permanent(
                        "unexpected response from server (listing is cut short without a continuation token)"
                            .into(),
                    ));
                }
                Some(Some(next)) if token.as_deref() == Some(next.as_str()) => {
                    return Err(ProviderError::Permanent(
                        "unexpected response from server (listing repeats its continuation token)"
                            .into(),
                    ));
                }
                Some(Some(next)) => token = Some(next),
            }
        }
        if !exists && !dir.is_root() {
            return Err(ProviderError::NotFound);
        }
        let info = info.ok_or_else(|| ProviderError::Permanent("no listing was fetched".into()))?;
        Ok((Listing { entries, info }, skips))
    }

    /// Every file and folder below `root` (not `root` itself), each once, folders listed side
    /// by side, and the names left out of all of those listings. The same walk as
    /// [`httporter_core::walk`], over [`Self::list_dir`] because that knows what it skipped for
    /// this call alone: the cumulative [`skipped_keys`](Self::skipped_keys) also counts what
    /// other calls skip at the same time.
    async fn walk_dir(&self, root: &RemotePath) -> Result<(Vec<Entry>, Skips), ProviderError> {
        let mut waiting = VecDeque::from([root.clone()]);
        let mut listing = FuturesUnordered::new();
        let mut found = Vec::new();
        let mut skips = Skips::default();
        loop {
            while listing.len() < LIST_CONCURRENCY
                && let Some(dir) = waiting.pop_front()
            {
                listing.push(async move {
                    let result = self.list_dir(&dir).await;
                    (dir, result)
                });
            }
            let Some((dir, result)) = listing.next().await else {
                return Ok((found, skips));
            };
            let (listed, listed_skips) = result?;
            skips.merge(listed_skips);
            for entry in listed.entries {
                // The folder itself (S3 lists the marker `a/` under the prefix `a/`) would be
                // walked again forever, and a path from elsewhere leads out of `root`.
                if entry.path == dir || !entry.path.is_within(&dir) {
                    continue;
                }
                if entry.is_folder {
                    waiting.push_back(entry.path.clone());
                }
                found.push(entry);
            }
        }
    }

    /// One attempt at a single `DELETE`.
    async fn delete_target(&self, bucket: &str, target: &Target) -> Result<(), ProviderError> {
        let _slot = self
            .delete_slots
            .acquire()
            .await
            .map_err(|_| limiter_closed("delete"))?;
        let url = target.url(&self.endpoint, bucket);
        let response = self
            .call(Method::DELETE, url, &[], EMPTY_PAYLOAD_SHA256, None, None)
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(failure(response, self.request_timeout).await)
    }

    /// One attempt at a `DeleteObjects` request for `keys` (at most [`DELETE_MAX`], all
    /// [writable in XML](xml::is_xml_safe)). Answers with the keys the service refused to
    /// delete.
    async fn post_delete(
        &self,
        bucket: &str,
        keys: &[&str],
    ) -> Result<Vec<DeleteFailure>, ProviderError> {
        let _slot = self
            .delete_slots
            .acquire()
            .await
            .map_err(|_| limiter_closed("delete"))?;
        // The body is built once a slot is free, so only requests in flight hold one.
        let body = xml::delete_request(keys).into_bytes();
        let extra = vec![
            ("content-md5".to_owned(), b64(&Md5::digest(&body))),
            ("content-type".to_owned(), "application/xml".to_owned()),
        ];
        let payload = sha256_hex(&body);
        let mut url = bucket_url(&self.endpoint, bucket);
        url.set_query(Some("delete"));
        let response = self
            .call(Method::POST, url, &extra, &payload, Some(body), None)
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(failure(response, self.request_timeout).await);
        }
        let body = read_limited(response, ANSWER_MAX).await?;
        xml::parse_delete_result(&body, status.as_u16())
    }

    /// Deletes `targets`, answering for each in order: `DeleteObjects` for up to 1,000 keys per
    /// request, a plain `DELETE` for a key XML cannot carry.
    async fn delete_targets(
        &self,
        bucket: &str,
        targets: &[Target],
    ) -> Vec<Result<(), ProviderError>> {
        let mut results: Vec<Option<Result<(), ProviderError>>> = vec![None; targets.len()];
        let mut batched: Vec<(usize, String)> = Vec::new();
        let mut alone: Vec<usize> = Vec::new();
        for (index, target) in targets.iter().enumerate() {
            if target.path.is_root() {
                results[index] = Some(Err(ProviderError::Refused(
                    "The bucket itself can't be deleted as a file".into(),
                )));
                continue;
            }
            let key = target.key();
            if xml::is_xml_safe(&key) {
                batched.push((index, key));
            } else {
                alone.push(index);
            }
        }

        let requests = batched.chunks(DELETE_MAX).map(|chunk| async move {
            let keys: Vec<&str> = chunk.iter().map(|(_, key)| key.as_str()).collect();
            (chunk, self.post_delete(bucket, &keys).await)
        });
        let singles = alone.iter().map(|&index| async move {
            (index, self.delete_target(bucket, &targets[index]).await)
        });
        let (answers, singles) =
            futures_util::future::join(join_all(requests), join_all(singles)).await;

        for (chunk, answer) in answers {
            match answer {
                Ok(failures) => {
                    for (index, result) in outcomes(chunk, failures) {
                        results[index] = Some(result);
                    }
                }
                Err(error) => {
                    for (index, _) in chunk {
                        results[*index] = Some(Err(error.clone()));
                    }
                }
            }
        }
        for (index, result) in singles {
            results[index] = Some(result);
        }
        results
            .into_iter()
            .map(|result| {
                result.unwrap_or_else(|| {
                    Err(ProviderError::Permanent(
                        "the delete was not carried out".into(),
                    ))
                })
            })
            .collect()
    }
}

#[async_trait]
impl StorageProvider for S3Storage {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn endpoint(&self) -> String {
        bucket_url(&self.endpoint, &self.bucket).to_string()
    }

    /// The direct children of `dir`, folders from the common prefixes, all pages of the
    /// listing. The latency and HTTP version are those of the first request. A folder with no
    /// key under it does not exist: [`ProviderError::NotFound`].
    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        self.list_dir(dir).await.map(|(listing, _)| listing)
    }

    /// One `ListObjectsV2` request: the children of `dir` whose names start with `prefix`, at
    /// most `limit` of them. No key under the prefix is an empty listing, not
    /// [`ProviderError::NotFound`].
    async fn list_prefix(
        &self,
        dir: &RemotePath,
        prefix: &str,
        limit: u32,
    ) -> Result<Listing, ProviderError> {
        let bucket = self.bucket()?;
        let dir_prefix = if dir.is_root() {
            String::new()
        } else {
            format!("{}/", dir.as_str())
        };
        let key_prefix = format!("{dir_prefix}{prefix}");
        let (page, info) = self
            .list_page(bucket, &key_prefix, None, Some(limit.max(1)))
            .await?;
        let mut entries = Vec::new();
        let mut skips = Skips::default();
        collect(dir, &dir_prefix, page, &mut entries, &mut skips);
        self.skipped.fetch_add(skips.count, Ordering::Relaxed);
        Ok(Listing { entries, info })
    }

    /// `PutObject` with the content type, `Content-MD5` when `opts.md5` is set (the service
    /// rejects the upload with `BadDigest`, [`ProviderError::ChecksumMismatch`], if the bytes
    /// differ) and `opts.sha256` recorded as `x-amz-meta-httporter-sha256` for later
    /// comparisons; S3 cannot check a SHA-256 itself. The body is not covered by the signature
    /// (`UNSIGNED-PAYLOAD`), and goes through `opts.meter` piece by piece.
    ///
    /// A file of more than [`MULTIPART_THRESHOLD`] (64 MiB) is not sent in one piece but as a
    /// multipart upload, in parts of [`S3Config::part_size`] with four in flight at once, each
    /// with its own `Content-MD5`. `opts.parts` keeps its progress so that the next attempt
    /// carries on where this one stopped; `opts.md5` is not used then, since a service can
    /// check a whole-file digest only of a single `PutObject`. A byte source is always sent in
    /// one piece. See the [`multipart`](crate::multipart) module.
    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        let bucket = self.bucket()?;
        if path.is_root() {
            return Err(ProviderError::Refused(
                "The bucket root is a folder, not a file".into(),
            ));
        }
        let url = object_url(&self.endpoint, bucket, path);
        let _slot = self
            .transfer_slots
            .acquire()
            .await
            .map_err(|_| limiter_closed("transfer"))?;

        if let UploadSource::File { path, len } = &source
            && *len > MULTIPART_THRESHOLD
        {
            return self.upload_in_parts(&url, path, *len, opts).await;
        }

        let upload = upload_body(&source, opts.meter.clone()).await?;
        let mut extra = object_headers(opts);
        if let Some(md5) = &opts.md5 {
            extra.push(("content-md5".to_owned(), b64(md5)));
        }
        let headers = self.sign_request(&Method::PUT, &url, &extra, UNSIGNED_PAYLOAD)?;
        let request = self
            .transfer_client
            .put(url)
            .headers(headers)
            // Declared, so the body is sent with a length: S3 refuses a chunked upload.
            .header(CONTENT_LENGTH, upload.len)
            .body(Body::wrap_stream(upload.stream));

        // The service answers only after the whole body has arrived, so the wait covers
        // sending it as well; what is limited is the time since the connection last asked for
        // more of the body.
        let sent = until_stalled(
            request.send(),
            &upload.progress,
            self.transfer_idle_timeout,
            "upload",
        )
        .await?;
        let response = match sent {
            Ok(response) => response,
            // A body that failed because the file was not what it was said to be.
            Err(error) => {
                return Err(upload.fault.take().unwrap_or_else(|| network_error(error)));
            }
        };
        if response.status().is_success() {
            return Ok(());
        }
        Err(failure(response, self.transfer_idle_timeout).await)
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        let bucket = self.bucket()?;
        if path.is_root() {
            return Err(ProviderError::Refused(
                "The bucket root is a folder, not a file".into(),
            ));
        }
        let url = object_url(&self.endpoint, bucket, path);
        let slot = Arc::clone(&self.transfer_slots)
            .acquire_owned()
            .await
            .map_err(|_| limiter_closed("transfer"))?;
        let headers = self.sign_request(&Method::GET, &url, &[], EMPTY_PAYLOAD_SHA256)?;
        let sent = tokio::time::timeout(
            self.transfer_idle_timeout,
            self.transfer_client.get(url).headers(headers).send(),
        )
        .await
        .map_err(|_| errors::stall_error("download", self.transfer_idle_timeout))?;
        let response = sent.map_err(network_error)?;
        if !response.status().is_success() {
            return Err(failure(response, self.transfer_idle_timeout).await);
        }
        Ok(guard_download(
            Box::pin(response.bytes_stream()),
            slot,
            self.transfer_idle_timeout,
        ))
    }

    /// Succeeds for a key that is not there as well: S3 does not say.
    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        let bucket = self.bucket()?;
        if path.is_root() {
            return Err(ProviderError::Refused(
                "The bucket root is a folder, not a file".into(),
            ));
        }
        self.delete_target(bucket, &Target::file(path)).await
    }

    /// Lists everything below `dir` (a request per folder, side by side), then deletes the
    /// files, the markers of the folders and `dir`'s own marker with `DeleteObjects`. The
    /// bucket root is refused: that is emptying the bucket.
    ///
    /// It deletes everything it can, and then answers honestly:
    /// - If some keys cannot be deleted, the rest still are, and the first failure is returned.
    /// - A key `RemotePath` cannot hold (`a//b`, a `..` segment, a name that is not UTF-8) is
    ///   left out of the walk and so survives. Rather than claim the folder is gone, the call
    ///   ends with [`ProviderError::Refused`] naming one of those keys, once everything else is
    ///   deleted. (A failed delete comes first: a retry may clear it, and ends in the refusal.)
    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        let bucket = self.bucket()?;
        if dir.is_root() {
            return Err(ProviderError::Refused(
                "Deleting the whole bucket isn't allowed".into(),
            ));
        }
        let (found, skips) = self.walk_dir(dir).await?;
        let mut targets: Vec<Target> = found
            .into_iter()
            .map(|entry| Target {
                path: entry.path,
                folder: entry.is_folder,
            })
            .collect();
        targets.push(Target {
            path: dir.clone(),
            folder: true,
        });
        let deleted = self.delete_targets(bucket, &targets).await;
        if let Some(error) = deleted.into_iter().find_map(Result::err) {
            return Err(error);
        }
        match skips.first {
            None => Ok(()),
            Some(key) => Err(ProviderError::Refused(format!(
                "Some files in this folder have names HTTPorter can't handle, such as \"{}\". \
                 Delete them in your storage provider's console.",
                shown(&key)
            ))),
        }
    }

    /// Writes a zero-byte `dir/` marker; writing it again is harmless.
    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        let bucket = self.bucket()?;
        if dir.is_root() {
            return Ok(());
        }
        let url = Target {
            path: dir.clone(),
            folder: true,
        }
        .url(&self.endpoint, bucket);
        let extra = [("content-type".to_owned(), FOLDER_CONTENT_TYPE.to_owned())];
        let response = self
            .call(
                Method::PUT,
                url,
                &extra,
                EMPTY_PAYLOAD_SHA256,
                Some(Vec::new()),
                None,
            )
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(failure(response, self.request_timeout).await)
    }

    /// `DeleteObjects`, 1,000 keys per request, requests side by side. A request that fails as
    /// a whole fails each of its keys; one that answers names the keys it could not delete.
    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        let bucket = match self.bucket() {
            Ok(bucket) => bucket,
            Err(error) => return paths.iter().map(|_| Err(error.clone())).collect(),
        };
        let targets: Vec<Target> = paths.iter().map(Target::file).collect();
        self.delete_targets(bucket, &targets).await
    }

    /// Asks for a single key, much lighter than the default (a listing of the root), and takes
    /// any answer at all as the service being there, as Bunny's probe does (review I2): only no
    /// answer (no connection, a reset, a timeout) is an error. A refusal or a server error is
    /// for the next real request to report, which stops the site or fails the job with the
    /// right words; it must not keep a site "offline" while the network is fine.
    async fn reachable(&self) -> Result<(), ProviderError> {
        let bucket = self.bucket()?;
        let url = self.list_url(bucket, "", None, Some(1));
        let response = self
            .call(
                Method::GET,
                url,
                &[],
                EMPTY_PAYLOAD_SHA256,
                None,
                Some(self.list_timeout),
            )
            .await?;
        // Read (a little of) it so the connection can be reused; what it says doesn't matter.
        let _ = read_capped(response, ERROR_BODY_MAX).await;
        Ok(())
    }

    /// `AbortMultipartUpload`. An upload the service no longer has is as good as aborted.
    async fn abort_multipart(
        &self,
        path: &RemotePath,
        upload_id: &str,
    ) -> Result<(), ProviderError> {
        let bucket = self.bucket()?;
        let object = object_url(&self.endpoint, bucket, path);
        self.abort_upload(&object, upload_id).await
    }
}

/// The names one listing call left out because no [`RemotePath`] can hold them.
#[derive(Debug, Default)]
struct Skips {
    count: u64,
    /// The smallest of the names (a key, or a folder prefix standing for several keys): an
    /// example for a message, the same whatever order the listings finished in.
    first: Option<String>,
}

impl Skips {
    fn note(&mut self, name: String) {
        self.count += 1;
        self.keep_smaller(Some(name));
    }

    fn merge(&mut self, other: Skips) {
        self.count += other.count;
        self.keep_smaller(other.first);
    }

    fn keep_smaller(&mut self, name: Option<String>) {
        if let Some(name) = name
            && self.first.as_ref().is_none_or(|first| name < *first)
        {
            self.first = Some(name);
        }
    }
}

/// Turns the keys and prefixes of one page of listing `dir` into entries. `prefix` is `dir`
/// with a trailing slash, or empty for the root. What cannot become an entry is noted in
/// `skips`.
fn collect(
    dir: &RemotePath,
    prefix: &str,
    page: ListPage,
    out: &mut Vec<Entry>,
    skips: &mut Skips,
) {
    let url_encoded = page.url_encoded;
    let text = |raw: &str| decode_key(raw, url_encoded);
    for object in page.objects {
        let Some(key) = text(&object.key) else {
            skips.note(lossy_key(&object.key, url_encoded));
            continue;
        };
        // The folder's own marker, and anything a misbehaving service lists from outside.
        let Some(name) = key.strip_prefix(prefix).filter(|name| !name.is_empty()) else {
            continue;
        };
        // The marker of a subfolder: the folder is shown from its common prefix.
        if name.ends_with('/') {
            continue;
        }
        match dir.join(name) {
            Ok(path) => out.push(Entry {
                path,
                is_folder: false,
                size: object.size,
                modified: object.modified.as_deref().and_then(parse_time),
                checksum: checksum_from_etag(object.etag.as_deref()),
                content_type: None,
                replicated_regions: Vec::new(),
            }),
            Err(_) => skips.note(key),
        }
    }
    for common in page.prefixes {
        let Some(full) = text(&common) else {
            skips.note(lossy_key(&common, url_encoded));
            continue;
        };
        let Some(rest) = full.strip_prefix(prefix) else {
            continue;
        };
        match dir.join(rest.strip_suffix('/').unwrap_or(rest)) {
            Ok(path) => out.push(Entry {
                path,
                is_folder: true,
                size: 0,
                modified: None,
                checksum: None,
                content_type: None,
                replicated_regions: Vec::new(),
            }),
            Err(_) => skips.note(full),
        }
    }
}

/// A key as far as it can be read, for naming it to the user: the replacement character stands
/// for bytes that are not UTF-8.
fn lossy_key(raw: &str, url_encoded: bool) -> String {
    if url_encoded {
        percent_decode_str(&raw.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned()
    } else {
        raw.to_owned()
    }
}

/// The longest key shown in a message.
const SHOWN_MAX: usize = 120;

/// A key for a message: control characters become `?` (a key may hold anything), and a very
/// long one is cut.
fn shown(key: &str) -> String {
    let mut text: String = key
        .chars()
        .take(SHOWN_MAX)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if key.chars().count() > SHOWN_MAX {
        text.push_str("...");
    }
    text
}

/// A key to delete: a file, or the marker of a folder (`name/`), which a [`RemotePath`] cannot
/// spell because it has no trailing slash.
struct Target {
    path: RemotePath,
    folder: bool,
}

impl Target {
    fn file(path: &RemotePath) -> Self {
        Self {
            path: path.clone(),
            folder: false,
        }
    }

    /// The key as S3 knows it.
    fn key(&self) -> String {
        if self.folder {
            format!("{}/", self.path.as_str())
        } else {
            self.path.as_str().to_owned()
        }
    }

    fn url(&self, endpoint: &Endpoint, bucket: &str) -> Url {
        let mut url = object_url(endpoint, bucket, &self.path);
        if self.folder && !self.path.is_root() {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        url
    }
}

/// What a `DeleteObjects` answer means for each key of the request: deleted unless named in an
/// error. An error that names no key of this request (or none at all) cannot be placed, so it
/// counts against every key that has no error of its own: reporting a failed delete as done is
/// worse than reporting a done one as failed.
fn outcomes(
    chunk: &[(usize, String)],
    failures: Vec<DeleteFailure>,
) -> Vec<(usize, Result<(), ProviderError>)> {
    let in_request: std::collections::HashSet<&str> =
        chunk.iter().map(|(_, key)| key.as_str()).collect();
    let mut named: HashMap<String, ProviderError> = HashMap::new();
    let mut stray: Option<ProviderError> = None;
    for failure in failures {
        let error = errors::from_parts(
            200,
            failure.code.as_deref(),
            failure.message.as_deref(),
            None,
        );
        match failure.key {
            Some(key) if in_request.contains(key.as_str()) => {
                named.entry(key).or_insert(error);
            }
            _ => {
                stray.get_or_insert(error);
            }
        }
    }
    chunk
        .iter()
        .map(|(index, key)| {
            let error = named.get(key).cloned().or_else(|| stray.clone());
            (*index, error.map_or(Ok(()), Err))
        })
        .collect()
}

/// The headers that describe an object being written: its content type (S3 does not guess it,
/// and an object without one is served as `binary/octet-stream`) and its SHA-256 for later
/// comparisons.
pub(crate) fn object_headers(opts: &UploadOptions) -> Vec<(String, String)> {
    let content_type = if opts.content_type.is_empty() {
        "application/octet-stream"
    } else {
        &opts.content_type
    };
    let mut headers = vec![("content-type".to_owned(), content_type.to_owned())];
    if let Some(sha256) = &opts.sha256 {
        headers.push((SHA256_META.to_owned(), hex::encode(sha256)));
    }
    headers
}

/// The headers for `method url`: the caller's `extra` headers (all of them signed) plus the
/// ones the signature adds. The signature is made now, so build the request right away.
pub(crate) fn signed_headers(
    creds: &Credentials,
    region: &str,
    method: &Method,
    url: &Url,
    extra: &[(String, String)],
    payload_sha256: &str,
) -> Result<HeaderMap, ProviderError> {
    let input = SignInput {
        method: method.as_str(),
        url,
        headers: extra,
        payload_sha256,
    };
    let signed = sign(&input, creds, region, "s3", Utc::now());
    let mut headers = HeaderMap::with_capacity(extra.len() + signed.len());
    for (name, value) in extra.iter().chain(signed.iter()) {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| unsendable(name))?;
        let mut header_value = HeaderValue::from_str(value).map_err(|_| unsendable(name))?;
        if header_name == AUTHORIZATION || name == "x-amz-security-token" {
            header_value.set_sensitive(true);
        }
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

/// The error for an unsuccessful answer, from its status, `Retry-After` and (a part of) its
/// body. Reading the body is best effort: without it the status alone decides.
pub(crate) async fn failure(response: reqwest::Response, limit: Duration) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = errors::retry_after(response.headers());
    let body = tokio::time::timeout(limit, read_capped(response, ERROR_BODY_MAX))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    errors::from_response(status, retry_after, &body)
}

/// Credentials that can never work are caught here, with the field to blame, rather than as an
/// unreadable error from the first request.
fn check_credentials(creds: &Credentials) -> Result<(), ProviderError> {
    let sendable = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_graphic());
    if !sendable(&creds.access_key_id) {
        return Err(ProviderError::Auth {
            hint: httporter_core::AuthHint::AccessKey,
        });
    }
    if creds.secret_access_key.is_empty() {
        return Err(ProviderError::Auth {
            hint: httporter_core::AuthHint::Secret,
        });
    }
    if let Some(token) = &creds.session_token
        && !sendable(token)
    {
        return Err(ProviderError::Auth {
            hint: httporter_core::AuthHint::Unknown,
        });
    }
    Ok(())
}

/// Both clients never follow redirects (a request, and its signature, belong to one address)
/// and use the proxy the settings choose. Only the short-request client has a total timeout.
fn build_client(
    proxy: &ProxyMode,
    http1_only: bool,
    keep_alive: bool,
    pool_size: usize,
    total_timeout: Option<Duration>,
) -> Result<reqwest::Client, ProviderError> {
    // reqwest brings no TLS crypto (`rustls-no-provider`): use ring's. Later calls are no-ops.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .redirect(redirect::Policy::none())
        .pool_max_idle_per_host(if keep_alive { pool_size } else { 0 });
    if let Some(timeout) = total_timeout {
        builder = builder.timeout(timeout);
    }
    if http1_only {
        builder = builder.http1_only();
    }
    builder = match proxy {
        ProxyMode::System => builder,
        ProxyMode::None => builder.no_proxy(),
        ProxyMode::Custom(address) => {
            // The address may carry a password: it is never part of the message.
            let proxy = reqwest::Proxy::all(address.as_str())
                .map_err(|_| ProviderError::Permanent("The proxy address is not valid.".into()))?;
            builder.proxy(proxy)
        }
    };
    builder
        .build()
        .map_err(|e| ProviderError::Permanent(format!("could not create the HTTP client: {e}")))
}

fn unsendable(name: &str) -> ProviderError {
    ProviderError::Permanent(format!("The {name} value can't be sent as an HTTP header."))
}

fn limiter_closed(what: &str) -> ProviderError {
    ProviderError::Permanent(format!("the {what} limiter was closed"))
}

fn http_version(version: Version) -> &'static str {
    match version {
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    }
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A key or prefix as the service sent it: as is, or URL-decoded when the answer says it is
/// encoded (`+` is a space). `None` for a name that is not UTF-8 once decoded.
fn decode_key(raw: &str, url_encoded: bool) -> Option<String> {
    if !url_encoded {
        return Some(raw.to_owned());
    }
    percent_decode_str(&raw.replace('+', " "))
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// What an `ETag` says about the content. 32 hex digits are the MD5 of an object uploaded in one
/// piece (and not encrypted with a customer key); `<32 hex>-<parts>` is the ETag of a multipart
/// upload, which only equals another file's by being the same upload. Anything else (a SHA-1
/// from Backblaze, an opaque value from a KMS-encrypted object) says nothing we can compare.
fn checksum_from_etag(etag: Option<&str>) -> Option<Checksum> {
    let tag = etag?.trim();
    let tag = tag.strip_prefix("W/").unwrap_or(tag);
    let tag = tag.trim_matches('"').trim().to_ascii_lowercase();
    let is_md5 = |text: &str| text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit());
    if is_md5(&tag) {
        return Some(Checksum {
            kind: ChecksumKind::Md5,
            hex: tag,
        });
    }
    let (md5, parts) = tag.split_once('-')?;
    if is_md5(md5) && !parts.is_empty() && parts.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Checksum {
            kind: ChecksumKind::MultipartEtag,
            hex: tag,
        });
    }
    None
}
