// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live probes of an S3-compatible service (plan Task 11), run against the user's Cloudflare R2
//! test bucket. Every test here is `#[ignore]`d: it talks to the real service and runs only when
//! asked for:
//!
//! ```text
//! cargo test --locked -p httporter-s3 --test live -- --ignored --nocapture
//! ```
//!
//! **Credentials.** They come from the git-ignored `.env.test` in the repository root (found by
//! walking up from this crate) and are read by this process alone: `HTTPORTER_TEST_S3_PRESET`
//! (`r2`), `HTTPORTER_TEST_S3_ACCOUNT_OR_ENDPOINT`, `HTTPORTER_TEST_S3_REGION`,
//! `HTTPORTER_TEST_S3_BUCKET` (which must start with `httporter-test`),
//! `HTTPORTER_TEST_S3_KEY_ID` and `HTTPORTER_TEST_S3_SECRET`. The file is read without touching
//! the environment, and a line it cannot parse is never quoted. Before its first request each
//! test checks the bucket name, and panics for any other bucket.
//!
//! **Output.** Each test prints what it saw (statuses, error codes, header names, the shape of
//! answers) through [`Live::observe`]. The account ID is part of every URL, so no URL is ever
//! printed; every printed text first goes through [`Live::scrub`], which takes out the account
//! ID, the key ID and the secret, and nothing prints a request, a configuration or the
//! provider's `Debug` (which shows the endpoint).
//!
//! **Cleanup.** Everything a test makes lives under its own `probe-<time>-<name>/` prefix. At the
//! end, also when the test fails, unfinished multipart uploads under it are aborted, the prefix is
//! deleted, and a listing without a delimiter checks that no key under it is left. The bucket
//! root is counted before and after.
//!
//! Requests the provider hides (a raw `DeleteObjects` to see its answer, `HEAD` for the served
//! headers, `If-None-Match`) are signed with this crate's own [`sign`] and sent on a plain
//! reqwest client; everything else uses [`S3Storage`]. Findings go into
//! `docs/notes/s3-presets.md`.

use std::collections::HashMap;
use std::future::Future;
use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::Utc;
use futures_util::{FutureExt, StreamExt};
use httporter_core::{
    ChecksumKind, Listing, ProviderError, RemotePath, S3Preset, StorageProvider, TransferSettings,
    UploadOptions, UploadSource, content_type_for, encode_segment,
};
use httporter_s3::{
    Credentials, Endpoint, PART_SIZE, S3Config, S3Storage, SignInput, Url, bucket_url, endpoint,
    object_url, sha256_hex, sign,
};
use md5::{Digest as _, Md5};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use sha2::Sha256;
use tokio::sync::Mutex;

/// The only buckets these tests may touch.
const BUCKET_PREFIX: &str = "httporter-test";

/// One probe at a time, so that each one's observations are its own.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

// ---------------------------------------------------------------------------------------------
// Setup and cleanup
// ---------------------------------------------------------------------------------------------

/// The `HTTPORTER_TEST_*` values of the nearest `.env.test` above this crate. The file is parsed
/// here, not loaded into the environment, and its errors are reported without the line they
/// are about (which may hold a secret).
fn test_env() -> HashMap<String, String> {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let file = loop {
        let candidate = dir.join(".env.test");
        if candidate.is_file() {
            break candidate;
        }
        dir = dir
            .parent()
            .unwrap_or_else(|| panic!("no .env.test in any folder above this crate"));
    };
    let entries =
        dotenvy::from_path_iter(&file).unwrap_or_else(|_| panic!(".env.test could not be opened"));
    let mut values = HashMap::new();
    for entry in entries {
        match entry {
            Ok((key, value)) if key.starts_with("HTTPORTER_TEST_") => {
                values.insert(key, value);
            }
            Ok(_) => {}
            Err(_) => panic!("an entry of .env.test could not be read"),
        }
    }
    values
}

fn required(values: &HashMap<String, String>, name: &str) -> String {
    match values.get(name).map(|v| v.trim()) {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => panic!("{name} is not set in .env.test"),
    }
}

/// Panics unless `bucket` is a test bucket. Called before any request.
fn assert_test_bucket(bucket: &str) {
    assert!(
        bucket.starts_with(BUCKET_PREFIX),
        "refusing to touch bucket {bucket:?}: live tests only use buckets starting with \
         {BUCKET_PREFIX:?}"
    );
}

/// The preset named in `.env.test`. Only R2 has been probed; the others are here so the same
/// tests can be pointed at another test bucket.
fn preset(name: &str) -> S3Preset {
    match name.to_ascii_lowercase().as_str() {
        "r2" | "cloudflare-r2" | "cloudflarer2" => S3Preset::CloudflareR2,
        "aws" | "s3" | "aws-s3" => S3Preset::AwsS3,
        "b2" | "backblaze" => S3Preset::BackblazeB2,
        "wasabi" => S3Preset::Wasabi,
        "spaces" | "digitalocean" => S3Preset::DigitalOceanSpaces,
        "minio" => S3Preset::Minio,
        "other" => S3Preset::Other,
        _ => panic!("HTTPORTER_TEST_S3_PRESET {name:?} is not a known preset"),
    }
}

/// A test's view of the bucket: the provider under test and a plain HTTP client for what the
/// provider hides.
struct Live {
    bucket: String,
    endpoint: Endpoint,
    creds: Credentials,
    /// The account ID (or endpoint), the key ID and the secret: never printed.
    secrets: Vec<String>,
    http: reqwest::Client,
    storage: S3Storage,
    /// This test's own folder, `probe-<time>-<name>`; nothing outside it is written.
    folder: RemotePath,
    /// Entries in the bucket root when the test began.
    root_before: usize,
}

impl Live {
    async fn start(name: &str) -> Self {
        let values = test_env();
        let bucket = required(&values, "HTTPORTER_TEST_S3_BUCKET");
        assert_test_bucket(&bucket);
        let preset = preset(&required(&values, "HTTPORTER_TEST_S3_PRESET"));
        let account = required(&values, "HTTPORTER_TEST_S3_ACCOUNT_OR_ENDPOINT");
        let region = values
            .get("HTTPORTER_TEST_S3_REGION")
            .map(|r| r.trim().to_owned())
            .unwrap_or_default();
        let key_id = required(&values, "HTTPORTER_TEST_S3_KEY_ID");
        let secret = required(&values, "HTTPORTER_TEST_S3_SECRET");
        let secrets = vec![account.clone(), key_id.clone(), secret.clone()];

        // The error is a sentence for a form field; it never repeats the account ID.
        let endpoint = endpoint(preset, &region, &account, &account, true)
            .unwrap_or_else(|message| panic!("no endpoint: {message}"));
        let creds = Credentials {
            access_key_id: key_id,
            secret_access_key: secret,
            session_token: None,
        };
        let config = S3Config::for_site(
            bucket.clone(),
            endpoint.clone(),
            creds.clone(),
            &TransferSettings::default(),
        );
        let storage = S3Storage::new(config)
            .unwrap_or_else(|_| panic!("could not build the provider (credentials unusable)"));
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .user_agent("HTTPorter-live-probe")
            .build()
            .unwrap_or_else(|_| panic!("could not build the HTTP client"));

        let stamp = Utc::now().format("%Y%m%d-%H%M%S-%3f");
        let folder = RemotePath::parse(&format!("probe-{stamp}-{name}"))
            .unwrap_or_else(|_| panic!("bad probe folder name"));

        let mut live = Self {
            bucket,
            endpoint,
            creds,
            secrets,
            http,
            storage,
            folder,
            root_before: 0,
        };
        // The first request: if the bucket cannot be reached, every probe stops here.
        let root = match live.storage.list(&RemotePath::root()).await {
            Ok(listing) => listing,
            Err(error) => panic!("S3 bucket not reachable: {}", live.kind(&error)),
        };
        live.root_before = root.entries.len();
        live.observe(
            "bucket",
            format!(
                "{} ({}, region {:?}, path style {}): root has {} entries; listing over {}",
                live.bucket,
                preset.label(),
                live.endpoint.region,
                live.endpoint.path_style,
                root.entries.len(),
                root.info.http_version
            ),
        );
        live
    }

    /// Aborts unfinished uploads under the test's prefix, deletes it, and checks that no key
    /// under it is left and the bucket root is as it was.
    async fn clean_up(&self) {
        for (key, upload_id) in self.open_uploads(&self.folder).await {
            let path = RemotePath::parse(&key).unwrap_or_else(|_| panic!("odd upload key"));
            let aborted = self.storage.abort_multipart(&path, &upload_id).await;
            self.observe(
                "cleanup: aborted an unfinished upload",
                format!("{aborted:?}"),
            );
        }
        let deleted = self.storage.delete_folder(&self.folder).await;
        if !matches!(deleted, Ok(()) | Err(ProviderError::NotFound)) {
            self.observe("cleanup: delete_folder", format!("{deleted:?}"));
        }
        let left = self.keys_under(&self.folder).await;
        let uploads = self.open_uploads(&self.folder).await.len();
        let root = self.storage.list(&RemotePath::root()).await;
        let after = root.as_ref().map_or(usize::MAX, |l| l.entries.len());
        self.observe(
            "cleanup",
            format!(
                "{} keys and {uploads} unfinished uploads left under the prefix; root entries \
                 before {}, after {after}",
                left.len(),
                self.root_before
            ),
        );
        // Fewer only after the sweep: a probe writes nothing outside its prefix.
        assert!(
            left.is_empty() && uploads == 0 && after <= self.root_before,
            "the bucket is not as it was"
        );
    }

    // --- Output -------------------------------------------------------------------------------

    /// `text` with the account ID, key ID and secret taken out.
    fn scrub(&self, text: impl Into<String>) -> String {
        let mut text = text.into();
        for secret in &self.secrets {
            if !secret.is_empty() {
                text = text.replace(secret.as_str(), "<redacted>");
            }
        }
        text
    }

    fn observe(&self, what: &str, text: impl Into<String>) {
        println!("[s3] {what}: {}", self.scrub(text));
    }

    fn kind(&self, error: &ProviderError) -> String {
        self.scrub(format!("{error:?}"))
    }

    fn ok<T>(&self, result: Result<T, ProviderError>, what: &str) -> T {
        result.unwrap_or_else(|error| panic!("{what} failed: {}", self.kind(&error)))
    }

    fn listed(&self, result: &Result<Listing, ProviderError>) -> String {
        match result {
            Ok(listing) => format!(
                "Ok, {} entries [{}]",
                listing.entries.len(),
                listing
                    .entries
                    .iter()
                    .map(|e| format!("{}{}", e.name(), if e.is_folder { "/" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(error) => format!("Err({})", self.kind(error)),
        }
    }

    // --- Paths and URLs -----------------------------------------------------------------------

    fn path(&self, rel: &str) -> RemotePath {
        rel.split('/').fold(self.folder.clone(), |path, segment| {
            path.join(segment)
                .unwrap_or_else(|_| panic!("bad segment {segment:?}"))
        })
    }

    /// The key of `rel` inside the test's folder.
    fn key(&self, rel: &str) -> String {
        self.path(rel).as_str().to_owned()
    }

    fn object(&self, rel: &str) -> Url {
        object_url(&self.endpoint, &self.bucket, &self.path(rel))
    }

    /// The bucket URL with `query` (already encoded).
    fn bucket_query(&self, query: &str) -> Url {
        let mut url = bucket_url(&self.endpoint, &self.bucket);
        url.set_query(Some(query));
        url
    }

    // --- Requests -----------------------------------------------------------------------------

    /// One signed request on the plain client. `extra` headers are signed and sent; a body is
    /// sent (with its length) for `PUT` and `POST`.
    async fn raw(
        &self,
        method: Method,
        url: Url,
        extra: &[(&str, String)],
        body: Vec<u8>,
    ) -> Answer {
        let extra: Vec<(String, String)> = extra
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect();
        let payload = sha256_hex(&body);
        let input = SignInput {
            method: method.as_str(),
            url: &url,
            headers: &extra,
            payload_sha256: &payload,
        };
        let signed = sign(&input, &self.creds, &self.endpoint.region, "s3", Utc::now());
        let mut headers = HeaderMap::new();
        for (name, value) in extra.iter().chain(signed.iter()) {
            let name = HeaderName::from_bytes(name.as_bytes())
                .unwrap_or_else(|_| panic!("bad header name"));
            let mut value =
                HeaderValue::from_str(value).unwrap_or_else(|_| panic!("bad header value"));
            if name == reqwest::header::AUTHORIZATION {
                value.set_sensitive(true);
            }
            headers.insert(name, value);
        }
        let sends_body = method == Method::PUT || method == Method::POST;
        let mut request = self.http.request(method, url).headers(headers);
        if sends_body {
            request = request.header("content-length", body.len()).body(body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => panic!(
                "request failed: {}",
                self.scrub(error.without_url().to_string())
            ),
        };
        let status = response.status();
        let headers = response.headers().clone();
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => panic!(
                "reading failed: {}",
                self.scrub(error.without_url().to_string())
            ),
        };
        Answer {
            status,
            headers,
            body,
        }
    }

    /// Every key under `dir/`, from a listing without a delimiter (so markers count too).
    async fn keys_under(&self, dir: &RemotePath) -> Vec<String> {
        let query = format!(
            "list-type=2&prefix={}",
            encode_segment(&format!("{}/", dir.as_str()))
        );
        let answer = self
            .raw(Method::GET, self.bucket_query(&query), &[], Vec::new())
            .await;
        assert!(
            answer.status.is_success(),
            "listing keys: {}",
            self.scrub(answer.summary())
        );
        elements(&answer.text_raw(), "Key")
    }

    /// The unfinished multipart uploads under `dir/`, as `(key, upload id)`.
    async fn open_uploads(&self, dir: &RemotePath) -> Vec<(String, String)> {
        let query = format!(
            "prefix={}&uploads",
            encode_segment(&format!("{}/", dir.as_str()))
        );
        let answer = self
            .raw(Method::GET, self.bucket_query(&query), &[], Vec::new())
            .await;
        if !answer.status.is_success() {
            self.observe("listing unfinished uploads", answer.summary());
            return Vec::new();
        }
        answer
            .text_raw()
            .split("<Upload>")
            .skip(1)
            .filter_map(|upload| {
                let key = elements(upload, "Key").into_iter().next()?;
                let id = elements(upload, "UploadId").into_iter().next()?;
                Some((key, id))
            })
            .collect()
    }

    /// A `DeleteObjects` request for `keys`, raw, so its answer can be seen as it is.
    async fn delete_objects(&self, keys: &[String], quiet: bool) -> Answer {
        let mut body = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
        );
        if quiet {
            body.push_str("<Quiet>true</Quiet>");
        }
        for key in keys {
            body.push_str("<Object><Key>");
            body.push_str(key);
            body.push_str("</Key></Object>");
        }
        body.push_str("</Delete>");
        let md5 = base64_md5(body.as_bytes());
        self.raw(
            Method::POST,
            self.bucket_query("delete"),
            &[
                ("content-md5", md5),
                ("content-type", "application/xml".to_owned()),
            ],
            body.into_bytes(),
        )
        .await
    }

    async fn put_bytes(&self, rel: &str, data: &[u8], content_type: &str) {
        let result = self
            .storage
            .upload(
                &self.path(rel),
                UploadSource::Bytes(Bytes::copy_from_slice(data)),
                &UploadOptions::new(content_type),
            )
            .await;
        self.ok(result, &format!("uploading {rel}"));
    }

    /// The whole of a download, hashed as it streams in.
    async fn fetch_digest(&self, path: &RemotePath) -> Result<([u8; 32], u64), ProviderError> {
        let mut stream = self.storage.download(path).await?;
        let mut hasher = Sha256::new();
        let mut len = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            len += chunk.len() as u64;
            hasher.update(&chunk);
        }
        Ok((hasher.finalize().into(), len))
    }
}

/// What the service answered to a raw request.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    /// The body as it is.
    fn text_raw(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The status, then the body as one quoted line of at most 400 characters.
    fn summary(&self) -> String {
        format!("{} {}", self.status.as_u16(), short_text(&self.body))
    }

    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .map_or_else(|| "(none)".to_owned(), |v| format!("{v:?}"))
    }

    fn header_names(&self) -> String {
        let mut names: Vec<&str> = self.headers.keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        names.join(", ")
    }

    /// The S3 error `<Code>` in the body, if any.
    fn code(&self) -> String {
        elements(&self.text_raw(), "Code")
            .into_iter()
            .next()
            .unwrap_or_else(|| "(no code)".to_owned())
    }
}

fn short_text(body: &[u8]) -> String {
    if body.is_empty() {
        return "(empty body)".to_owned();
    }
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut short: String = text.chars().take(400).collect();
    if text.chars().count() > 400 {
        short.push_str("...");
    }
    format!("{short:?}")
}

/// The text of every `<name>` element in `xml`, in order. Enough for the probes' own keys,
/// which need no unescaping.
fn elements(xml: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    xml.split(open.as_str())
        .skip(1)
        .filter_map(|rest| {
            rest.split_once(close.as_str())
                .map(|(text, _)| text.to_owned())
        })
        .collect()
}

fn base64_md5(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(Md5::digest(data))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn md5(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

/// Runs `body` against a fresh [`Live`] for the test `name`, then cleans up, also when `body`
/// panics (the panic is passed on afterwards).
async fn probe<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<Live>) -> Fut,
    Fut: Future<Output = ()>,
{
    let _turn = ONE_AT_A_TIME.lock().await;
    let live = Arc::new(Live::start(name).await);
    let outcome = AssertUnwindSafe(body(Arc::clone(&live)))
        .catch_unwind()
        .await;
    live.clean_up().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

// ---------------------------------------------------------------------------------------------
// The probes
// ---------------------------------------------------------------------------------------------

/// Not a probe: removes `probe-*` folders an interrupted run left in the bucket root, with the
/// unfinished uploads under them. Nothing else in the bucket is touched.
#[tokio::test]
#[ignore = "live: needs .env.test and the S3 test bucket"]
async fn sweep_leftover_probe_folders() {
    probe("sweep", |live| async move {
        let root = live.ok(live.storage.list(&RemotePath::root()).await, "root listing");
        for entry in root.entries {
            if !entry.is_folder || !entry.name().starts_with("probe-") {
                continue;
            }
            for (key, upload_id) in live.open_uploads(&entry.path).await {
                let path = RemotePath::parse(&key).unwrap_or_else(|_| panic!("odd upload key"));
                let aborted = live.storage.abort_multipart(&path, &upload_id).await;
                live.observe("sweep: aborted an upload", format!("{aborted:?}"));
            }
            let deleted = live.storage.delete_folder(&entry.path).await;
            live.observe(
                &format!("sweep: {}", entry.name()),
                format!("delete_folder {deleted:?}"),
            );
        }
    })
    .await;
}

/// A small upload and a multipart one, each checked by listing, `HEAD` and download; a wrong
/// `Content-MD5`; and `If-None-Match: *` on a key that exists.
#[tokio::test]
#[ignore = "live: needs .env.test and the S3 test bucket"]
async fn round_trip_small_and_multipart() {
    probe("roundtrip", |live| async move {
        // --- Small, in one PutObject.
        let data = b"HTTPorter live probe: a small object.\n".to_vec();
        let small = live.path("small.txt");
        let mut opts = UploadOptions::new("text/plain");
        opts.md5 = Some(md5(&data));
        opts.sha256 = Some(sha256(&data));
        let sent = live
            .storage
            .upload(
                &small,
                UploadSource::Bytes(Bytes::from(data.clone())),
                &opts,
            )
            .await;
        live.observe("small: upload", format!("{sent:?}"));
        live.ok(sent, "the small upload");

        let listing = live.ok(live.storage.list(&live.folder).await, "listing");
        let entry = listing.entries.iter().find(|e| e.name() == "small.txt");
        let checksum = entry.and_then(|e| e.checksum.clone());
        live.observe(
            "small: listing",
            format!(
                "size {:?}, modified {:?}, checksum {:?} (MD5 matches: {})",
                entry.map(|e| e.size),
                entry.and_then(|e| e.modified),
                checksum.as_ref().map(|c| c.kind),
                checksum.as_ref().is_some_and(
                    |c| c.kind == ChecksumKind::Md5 && c.hex == hex::encode(md5(&data))
                )
            ),
        );
        let head = live
            .raw(Method::HEAD, live.object("small.txt"), &[], Vec::new())
            .await;
        live.observe(
            "small: HEAD",
            format!(
                "{} etag {} content-type {} sha256 meta matches: {} [headers: {}]",
                head.status.as_u16(),
                head.header("etag"),
                head.header("content-type"),
                head.headers
                    .get("x-amz-meta-httporter-sha256")
                    .and_then(|v| v.to_str().ok())
                    == Some(hex::encode(sha256(&data)).as_str()),
                head.header_names()
            ),
        );
        let back = live.fetch_digest(&small).await;
        live.observe(
            "small: download",
            format!(
                "same bytes: {}",
                back.as_ref().ok() == Some(&(sha256(&data), data.len() as u64))
            ),
        );

        // A Content-MD5 that does not match.
        let mut wrong = UploadOptions::new("text/plain");
        wrong.md5 = Some(md5(b"something else"));
        let refused = live
            .storage
            .upload(
                &live.path("bad-md5.txt"),
                UploadSource::Bytes(Bytes::from(data.clone())),
                &wrong,
            )
            .await;
        live.observe(
            "small: upload with a wrong Content-MD5",
            format!("{refused:?}"),
        );

        // If-None-Match on a key that exists.
        let conditional = live
            .raw(
                Method::PUT,
                live.object("small.txt"),
                &[("if-none-match", "*".to_owned())],
                b"overwrite?".to_vec(),
            )
            .await;
        live.observe(
            "small: PUT with If-None-Match: * on an existing key",
            format!(
                "{} code {}",
                conditional.status.as_u16(),
                conditional.code()
            ),
        );
        let after = live.fetch_digest(&small).await;
        live.observe(
            "small: content afterwards",
            if after.as_ref().ok().map(|(sum, _)| *sum) == Some(sha256(&data)) {
                "unchanged"
            } else {
                "CHANGED"
            },
        );

        // --- Multipart: just over the threshold, so five parts of 16 MiB (the last 1 MiB).
        let len = 64 * 1024 * 1024 + 1024 * 1024;
        let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("no temp dir"));
        let file = dir.path().join("big.bin");
        let mut hasher = Sha256::new();
        {
            let mut out = std::fs::File::create(&file).unwrap_or_else(|_| panic!("temp file"));
            let block: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
            for n in 0..len / block.len() {
                // Each MiB differs, so a part sent twice or out of order shows.
                let mut piece = block.clone();
                piece[..8].copy_from_slice(&(n as u64).to_le_bytes());
                hasher.update(&piece);
                out.write_all(&piece).unwrap_or_else(|_| panic!("writing"));
            }
        }
        let big_sha256: [u8; 32] = hasher.finalize().into();
        let big = live.path("big.bin");
        let mut opts = UploadOptions::new("application/octet-stream");
        opts.sha256 = Some(big_sha256);
        let started = Instant::now();
        let sent = live
            .storage
            .upload(
                &big,
                UploadSource::File {
                    path: file.clone(),
                    len: len as u64,
                },
                &opts,
            )
            .await;
        live.observe(
            "multipart: upload",
            format!(
                "{sent:?} in {:.1} s ({} MiB parts)",
                started.elapsed().as_secs_f64(),
                PART_SIZE >> 20
            ),
        );
        live.ok(sent, "the multipart upload");
        let open = live.open_uploads(&live.folder).await.len();
        live.observe("multipart: unfinished uploads afterwards", open.to_string());

        let listing = live.ok(live.storage.list(&live.folder).await, "listing");
        let entry = listing.entries.iter().find(|e| e.name() == "big.bin");
        live.observe(
            "multipart: listing",
            format!(
                "size {:?}, checksum {:?}",
                entry.map(|e| e.size),
                entry.and_then(|e| e.checksum.as_ref()).map(|c| (
                    c.kind,
                    c.hex.split_once('-').map(|(_, parts)| format!("-{parts}"))
                ))
            ),
        );
        let head = live
            .raw(Method::HEAD, live.object("big.bin"), &[], Vec::new())
            .await;
        live.observe(
            "multipart: HEAD",
            format!(
                "{} content-length {} content-type {} sha256 meta matches: {}",
                head.status.as_u16(),
                head.header("content-length"),
                head.header("content-type"),
                head.headers
                    .get("x-amz-meta-httporter-sha256")
                    .and_then(|v| v.to_str().ok())
                    == Some(hex::encode(big_sha256).as_str())
            ),
        );
        let back = live.fetch_digest(&big).await;
        let same = back.as_ref().ok() == Some(&(big_sha256, len as u64));
        live.observe("multipart: download", format!("same bytes: {same}"));
        assert!(same, "the multipart object downloads as it was sent");
    })
    .await;
}

/// The `Content-Type` the service serves back for common site files, as the provider uploads
/// them, and for an object stored without one.
#[tokio::test]
#[ignore = "live: needs .env.test and the S3 test bucket"]
async fn content_type_served_for_html_css_js_webp() {
    probe("ctype", |live| async move {
        let mut wrong = Vec::new();
        for name in ["index.html", "style.css", "app.js", "img.webp"] {
            let sent_type = content_type_for(name);
            live.put_bytes(name, name.as_bytes(), &sent_type).await;
            let get = live
                .raw(Method::GET, live.object(name), &[], Vec::new())
                .await;
            let served = get
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("(none)")
                .to_owned();
            live.observe(
                &format!("ctype: {name}"),
                format!(
                    "sent {sent_type:?}, served {served:?} ({})",
                    get.status.as_u16()
                ),
            );
            if served != sent_type {
                wrong.push(name);
            }
        }
        let bare = live
            .raw(Method::PUT, live.object("bare"), &[], b"bare".to_vec())
            .await;
        let get = live
            .raw(Method::GET, live.object("bare"), &[], Vec::new())
            .await;
        live.observe(
            "ctype: PUT without a Content-Type",
            format!(
                "{}; served {}",
                bare.status.as_u16(),
                get.header("content-type")
            ),
        );
        assert!(wrong.is_empty(), "served another content type: {wrong:?}");
    })
    .await;
}

/// Folder markers, `DeleteObjects` (its quiet answer as R2 sends it, and its key limit), the
/// provider's batch delete and folder delete.
#[tokio::test]
#[ignore = "live: needs .env.test and the S3 test bucket"]
async fn delete_batch_and_marker_folders() {
    probe("delete", |live| async move {
        // --- Markers.
        let made = live.storage.create_folder(&live.path("empty")).await;
        live.observe("markers: create_folder empty", format!("{made:?}"));
        let top = live.storage.list(&live.folder).await;
        live.observe("markers: listing the probe folder", live.listed(&top));
        let inside = live.storage.list(&live.path("empty")).await;
        live.observe(
            "markers: listing the marker-only folder",
            live.listed(&inside),
        );
        let again = live.storage.create_folder(&live.path("empty")).await;
        live.observe("markers: create_folder again", format!("{again:?}"));

        for rel in ["d/a.txt", "d/c.txt", "d/sub/b.txt"] {
            live.put_bytes(rel, rel.as_bytes(), "text/plain").await;
        }
        let made = live.storage.create_folder(&live.path("d/sub")).await;
        live.observe(
            "markers: create_folder d/sub (has a file)",
            format!("{made:?}"),
        );
        let keys = live.keys_under(&live.folder).await;
        let prefix = format!("{}/", live.folder.as_str());
        live.observe(
            "markers: every key under the prefix",
            keys.iter()
                .map(|k| k.strip_prefix(&prefix).unwrap_or(k))
                .collect::<Vec<_>>()
                .join(", "),
        );

        // --- DeleteObjects as R2 answers it.
        let quiet = live
            .delete_objects(&[live.key("d/c.txt"), live.key("never-was.txt")], true)
            .await;
        live.observe(
            "DeleteObjects quiet, two keys (one missing)",
            format!(
                "{} content-type {} content-length {} transfer-encoding {}",
                quiet.summary(),
                quiet.header("content-type"),
                quiet.header("content-length"),
                quiet.header("transfer-encoding")
            ),
        );
        let still = live.keys_under(&live.path("d")).await;
        live.observe(
            "after it, d/c.txt is",
            if still.contains(&live.key("d/c.txt")) {
                "STILL THERE"
            } else {
                "gone"
            },
        );
        live.put_bytes("d/e.txt", b"e", "text/plain").await;
        let existing = live.delete_objects(&[live.key("d/e.txt")], true).await;
        live.observe("DeleteObjects quiet, one existing key", existing.summary());
        let missing = live
            .delete_objects(&[live.key("never-was-3.txt")], true)
            .await;
        live.observe("DeleteObjects quiet, one missing key", missing.summary());
        live.put_bytes("d/f.txt", b"f", "text/plain").await;
        let loud = live
            .delete_objects(&[live.key("d/f.txt"), live.key("never-was-2.txt")], false)
            .await;
        live.observe(
            "DeleteObjects not quiet, one existing and one missing key",
            loud.summary(),
        );

        let batch = live
            .storage
            .delete_batch(&[live.path("d/a.txt"), live.path("missing.txt")])
            .await;
        live.observe(
            "the provider's delete_batch (a.txt, missing.txt)",
            format!("{batch:?}"),
        );

        // --- The key limit, with keys that are not there.
        for count in [1000usize, 1001] {
            let keys: Vec<String> = (0..count)
                .map(|n| live.key(&format!("nothing-{n:04}.txt")))
                .collect();
            let answer = live.delete_objects(&keys, true).await;
            live.observe(
                &format!("DeleteObjects quiet with {count} keys"),
                format!(
                    "{} code {} ({} body bytes)",
                    answer.status.as_u16(),
                    answer.code(),
                    answer.body.len()
                ),
            );
        }

        // --- Folder deletes.
        let gone = live.storage.delete_folder(&live.path("d")).await;
        live.observe("delete_folder d", format!("{gone:?}"));
        let d = live.storage.list(&live.path("d")).await;
        live.observe("listing d afterwards", live.listed(&d));
        let gone = live.storage.delete_folder(&live.path("empty")).await;
        live.observe("delete_folder empty", format!("{gone:?}"));
        let top = live.storage.list(&live.folder).await;
        live.observe("listing the probe folder afterwards", live.listed(&top));
        let left = live.keys_under(&live.folder).await;
        live.observe("keys left under the prefix", left.len().to_string());

        assert!(
            batch.iter().all(Result::is_ok),
            "delete_batch reads R2's answer"
        );
        assert!(left.is_empty(), "the folder deletes leave nothing behind");
    })
    .await;
}

/// Whether these credentials may list the account's buckets (an R2 token can be limited to one
/// bucket). Only the count is printed, and whether the test bucket is among them.
#[tokio::test]
#[ignore = "live: needs .env.test and the S3 test bucket"]
async fn list_buckets_permission() {
    probe("buckets", |live| async move {
        let buckets = live.storage.list_buckets().await;
        live.observe(
            "list_buckets",
            match &buckets {
                Ok(names) => format!(
                    "Ok: {} buckets, the test bucket among them: {}",
                    names.len(),
                    names.contains(&live.bucket)
                ),
                Err(error) => format!("Err({})", live.kind(error)),
            },
        );
        let raw = live
            .raw(Method::GET, bucket_url(&live.endpoint, ""), &[], Vec::new())
            .await;
        live.observe(
            "ListBuckets raw",
            format!("{} code {}", raw.status.as_u16(), raw.code()),
        );
    })
    .await;
}
