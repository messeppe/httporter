// SPDX-License-Identifier: MIT OR Apache-2.0

//! Automated acceptance harness for I2 (plan Task 26, Step 1), without the window: the real
//! [`Engine`], planners, providers and SQLite journal, wired as `httporter-app` wires them
//! (`site_hub.rs` builds the clients, `session/sites.rs` tests a connection, `transfers.rs` plans,
//! submits, resumes and applies settings), against the user's Bunny test zone and Cloudflare R2
//! test bucket. Every test is `#[ignore]`d. Run them one at a time, in a release build (hashing in
//! a debug build is far too slow to measure):
//!
//! ```text
//! cargo test --locked --release -p httporter-transfer --test acceptance -- --ignored --nocapture --test-threads=1 <name>
//! ```
//!
//! **Credentials.** `D:\Projects\httporter\.env.test` is parsed by this process alone, without
//! touching the environment (so the child of the crash test inherits none of it); a line it cannot
//! parse is never quoted. The zone and the bucket must start with `httporter-test`, and the
//! production zone is refused by name, before the first request.
//!
//! **Output.** Everything is printed through `say!`, which takes out the password, the account ID,
//! the key ID and the secret (and the altered ones of the wrong-credentials test). Errors are
//! printed by variant and status only. No URL, request, header or configuration is printed (the
//! R2 host holds the account ID), and no logger is installed.
//!
//! **Cleanup.** Each test works under its own `accept-<unix time>[-<name>]/` folder and deletes it
//! at the end (through `plan_delete` and the engine, as the app deletes), also when it fails. The
//! one exception is `s1_bunny_upload_export`, whose upload `bunny_delete_export` measures deleting
//! (it uploads the export itself when there is none): run them in that order.
//! `sweep_accept_prefixes` deletes every root `accept-*` item an interrupted run left, on both
//! services, and nothing else. Nothing outside an `accept-*` folder is ever deleted
//! ([`delete_entries`] checks), and the reference export is only read.
//!
//! **Diagnostics.** `bunny_protocol_comparison` times every upload call (a pass-through
//! [`Probe`]) over the default protocol setting and over "HTTP/1.1 only". The crash test probes its
//! resume the same way, and for any file missing at the end says whether it was in flight at the
//! kill, re-queued, uploaded again, and what a `GET` of it answers now. It leaves the resumed
//! engine running until the uploads the kill cut off have been checked on the zone (ruling Q55:
//! at least `CHECK_AFTER` after their answer) and reports what the checks found.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use httporter_bunny::{BunnyConfig, BunnyStorage, storage_host};
use httporter_core::{
    ByteStream, Capabilities, EngineSnapshot, EngineState, Entry, HttpProtocol, Job, Journal,
    Listing, ProviderError, RemotePath, Resolution, RetryPolicy, S3Preset, SiteId, StorageProvider,
    TransferSettings, UploadOptions, UploadSource, encode_segment, walk, with_retry,
};
use httporter_platform::SqliteJournal;
use httporter_s3::{
    Credentials, Endpoint, S3Config, S3Storage, SignInput, Url, bucket_url, endpoint, object_url,
    part_size_for, sha256_hex, sign,
};
use httporter_transfer::{
    Batch, CHECK_AFTER, Engine, MAX_CHECKS, SiteInfo, plan_delete, plan_upload,
    policy_to_resolution, resolve_uploads,
};
use tokio::runtime::Runtime;

/// The credentials of the test zone and bucket. Never printed.
const ENV_FILE: &str = r"D:\Projects\httporter\.env.test";
/// The reference export (13,795 files, 854 MiB). Only read.
const EXPORT: &str = r"D:\site-export";
/// What the crash test uploads: 3,640 files, 135 MiB.
const S5_SOURCE: &str = r"D:\site-export\wp-content\plugins";
/// The only zones and buckets touched.
const TEST_PREFIX: &str = "httporter-test";
/// The production zone, named so that no mistake in the prefix check can reach it.
const PRODUCTION_ZONE: &str = "exampleshop";
/// Every folder the harness makes starts with this, and it deletes nothing else.
const ACCEPT: &str = "accept-";
/// Set for the child process of the crash test: `<journal>|<prefix>|<site id>`.
const CHILD_ENV: &str = "HTTPORTER_ACCEPT_S5_CHILD";
/// What the child says once its batch is submitted: `<jobs> <files> <bytes>`.
const SUBMITTED: &str = "CHILD-SUBMITTED ";
/// What the child says every two seconds: `<files done> <active>`.
const PROGRESS: &str = "CHILD-PROGRESS ";
/// When the crash test kills the child, counted from its start.
const KILL_AFTER: Duration = Duration::from_secs(20);
/// The New Folder sheet's suggested name (`copy::NEW_FOLDER_NAME`).
const NEW_FOLDER_NAME: &str = "untitled folder";
/// "1 MB/s" as the Settings window means it (`settings_view::BYTES_PER_MB`).
const ONE_MB: u64 = 1_048_576;

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

/// Every secret read or made; [`scrub`] takes them out of whatever is printed.
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn keep_secret(secret: &str) {
    if !secret.is_empty() {
        SECRETS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(secret.to_owned());
    }
}

fn scrub(text: &str) -> String {
    let secrets = SECRETS.lock().unwrap_or_else(PoisonError::into_inner);
    let mut text = text.to_owned();
    for secret in secrets.iter() {
        text = text.replace(secret.as_str(), "<redacted>");
    }
    text
}

/// `println!`, scrubbed.
macro_rules! say {
    ($($arg:tt)*) => {
        println!("{}", scrub(&format!($($arg)*)))
    };
}

/// An error as its variant (and status), never its text: a network error can quote a URL, and
/// the R2 host holds the account ID.
fn kind(error: &ProviderError) -> String {
    match error {
        ProviderError::Auth { hint } => format!("Auth({hint:?})"),
        ProviderError::NotFound => "NotFound".to_owned(),
        ProviderError::RateLimited { retry_after } => format!("RateLimited({retry_after:?})"),
        ProviderError::ChecksumMismatch => "ChecksumMismatch".to_owned(),
        ProviderError::Refused(_) => "Refused".to_owned(),
        ProviderError::Network(_) => "Network".to_owned(),
        ProviderError::Server { status, .. } => format!("Server({status})"),
        ProviderError::ClockSkew => "ClockSkew".to_owned(),
        ProviderError::Permanent(_) => "Permanent".to_owned(),
    }
}

/// The engine's words for a failed job (the app passes `copy::describe_transfer_error`): the
/// variant only.
fn describe(error: &ProviderError) -> String {
    kind(error)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / ONE_MB as f64
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn per_s(count: f64, d: Duration) -> f64 {
    if d.is_zero() {
        0.0
    } else {
        count / d.as_secs_f64()
    }
}

fn verdict(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

// ---------------------------------------------------------------------------------------------
// Credentials and clients
// ---------------------------------------------------------------------------------------------

/// The `HTTPORTER_TEST_*` values. No `Debug`: it holds the secrets.
#[derive(Clone)]
struct TestEnv {
    zone: String,
    /// Lower case, as the region list has it.
    region: String,
    password: String,
    s3_preset: String,
    s3_account: String,
    s3_region: String,
    bucket: String,
    key_id: String,
    secret: String,
}

/// Parses [`ENV_FILE`] without loading it into the environment, notes its secrets for
/// [`scrub`], and checks the zone and the bucket names.
fn test_env() -> TestEnv {
    let entries = dotenvy::from_path_iter(ENV_FILE)
        .unwrap_or_else(|_| panic!(".env.test could not be opened"));
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
    let required = |name: &str| match values.get(name).map(|v| v.trim()) {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => panic!("{name} is not set in .env.test"),
    };
    let env = TestEnv {
        zone: required("HTTPORTER_TEST_ZONE"),
        region: required("HTTPORTER_TEST_REGION").to_ascii_lowercase(),
        password: required("HTTPORTER_TEST_PASSWORD"),
        s3_preset: required("HTTPORTER_TEST_S3_PRESET"),
        s3_account: required("HTTPORTER_TEST_S3_ACCOUNT_OR_ENDPOINT"),
        s3_region: values
            .get("HTTPORTER_TEST_S3_REGION")
            .map(|r| r.trim().to_owned())
            .unwrap_or_default(),
        bucket: required("HTTPORTER_TEST_S3_BUCKET"),
        key_id: required("HTTPORTER_TEST_S3_KEY_ID"),
        secret: required("HTTPORTER_TEST_S3_SECRET"),
    };
    for secret in [&env.password, &env.s3_account, &env.key_id, &env.secret] {
        keep_secret(secret);
    }
    assert_test_zone(&env.zone);
    assert_test_bucket(&env.bucket);
    env
}

/// Panics unless `zone` is a test zone. Called before any request to it.
fn assert_test_zone(zone: &str) {
    assert!(
        zone.starts_with(TEST_PREFIX),
        "refusing to touch zone {zone:?}: only zones starting with {TEST_PREFIX:?}"
    );
    assert_ne!(
        zone, PRODUCTION_ZONE,
        "refusing to touch the production zone"
    );
}

/// Panics unless `bucket` is a test bucket. Called before any request to it.
fn assert_test_bucket(bucket: &str) {
    assert!(
        bucket.starts_with(TEST_PREFIX),
        "refusing to touch bucket {bucket:?}: only buckets starting with {TEST_PREFIX:?}"
    );
}

/// The preset named in `.env.test` (as the S3 live tests read it).
fn s3_preset(name: &str) -> S3Preset {
    match name.to_ascii_lowercase().as_str() {
        "r2" | "cloudflare-r2" | "cloudflarer2" => S3Preset::CloudflareR2,
        "aws" | "s3" | "aws-s3" => S3Preset::AwsS3,
        "b2" | "backblaze" => S3Preset::BackblazeB2,
        "wasabi" => S3Preset::Wasabi,
        "spaces" | "digitalocean" => S3Preset::DigitalOceanSpaces,
        "minio" => S3Preset::Minio,
        "other" => S3Preset::Other,
        _ => panic!("HTTPORTER_TEST_S3_PRESET is not a known preset"),
    }
}

/// A saved site as the engine sees it: its id, name and host for the batch, and its client.
#[derive(Clone)]
struct Site {
    id: SiteId,
    name: String,
    host: String,
    provider: Arc<dyn StorageProvider>,
}

/// The test zone's client with `password`, as the hub makes it (`make_client`):
/// [`BunnyConfig::for_site`] with the Transfers settings, a single attempt per call.
fn bunny_provider(
    env: &TestEnv,
    password: &str,
    settings: &TransferSettings,
) -> Result<(BunnyStorage, String), ProviderError> {
    assert_test_zone(&env.zone);
    let host = storage_host(&env.region)
        .unwrap_or_else(|| panic!("HTTPORTER_TEST_REGION is not a Bunny region"));
    let cfg = BunnyConfig::for_site(env.zone.as_str(), host.as_str(), password, settings);
    Ok((BunnyStorage::new(cfg)?, host))
}

fn bunny_site(env: &TestEnv, settings: &TransferSettings, id: SiteId) -> Site {
    let (storage, host) = bunny_provider(env, &env.password, settings)
        .unwrap_or_else(|e| panic!("the Bunny client could not be built: {}", kind(&e)));
    Site {
        id,
        name: env.zone.clone(),
        host,
        provider: Arc::new(storage),
    }
}

/// Where the test bucket answers, as Add Site saves it (`record_of`: the address as text, without
/// its trailing slash) and the hub reads it back (`s3_endpoint`).
fn s3_endpoint(env: &TestEnv) -> (Endpoint, S3Preset) {
    let preset = s3_preset(&env.s3_preset);
    let found = endpoint(
        preset,
        &env.s3_region,
        &env.s3_account,
        &env.s3_account,
        true,
    )
    .unwrap_or_else(|_| panic!("the test bucket's address could not be made"));
    let saved = found.base.as_str().trim_end_matches('/').to_owned();
    let base = Url::parse(&saved).unwrap_or_else(|_| panic!("the saved S3 address does not parse"));
    (
        Endpoint {
            base,
            path_style: found.path_style,
            region: found.region,
        },
        preset,
    )
}

/// The test bucket's client with `key_id` and `secret`, as the hub makes it:
/// [`S3Config::for_site`] with the Transfers settings and the part size of its service.
fn s3_provider(
    env: &TestEnv,
    key_id: &str,
    secret: &str,
    settings: &TransferSettings,
) -> Result<S3Storage, ProviderError> {
    assert_test_bucket(&env.bucket);
    let (endpoint, preset) = s3_endpoint(env);
    let creds = Credentials {
        access_key_id: key_id.to_owned(),
        secret_access_key: secret.to_owned(),
        session_token: None,
    };
    let mut cfg = S3Config::for_site(env.bucket.as_str(), endpoint, creds, settings);
    cfg.part_size = part_size_for(preset);
    S3Storage::new(cfg)
}

/// The batch's host is the preset's name, not the bucket's address: the drawer shows it, and the
/// address holds the account ID. Nothing else uses it.
fn r2_site(env: &TestEnv, settings: &TransferSettings, id: SiteId) -> Site {
    let storage = s3_provider(env, &env.key_id, &env.secret, settings)
        .unwrap_or_else(|e| panic!("the S3 client could not be built: {}", kind(&e)));
    Site {
        id,
        name: env.bucket.clone(),
        host: s3_preset(&env.s3_preset).label().to_owned(),
        provider: Arc::new(storage),
    }
}

/// Lists as browsing does (`site_hub::list_interactive`): two quick retries.
async fn list_interactive(
    provider: &dyn StorageProvider,
    dir: &RemotePath,
) -> Result<Listing, ProviderError> {
    with_retry(&RetryPolicy::interactive(), |_| provider.list(dir)).await
}

/// `site_hub::Interactive`: the client the planners list through, so a passing failure doesn't
/// stop a plan.
struct Interactive(Arc<dyn StorageProvider>);

#[async_trait]
impl StorageProvider for Interactive {
    fn capabilities(&self) -> &Capabilities {
        self.0.capabilities()
    }

    fn endpoint(&self) -> String {
        self.0.endpoint()
    }

    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        list_interactive(&*self.0, dir).await
    }

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        self.0.upload(path, source, opts).await
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        self.0.download(path).await
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        self.0.delete_file(path).await
    }

    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.0.delete_folder(dir).await
    }

    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.0.create_folder(dir).await
    }

    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        self.0.delete_batch(paths).await
    }

    async fn reachable(&self) -> Result<(), ProviderError> {
        self.0.reachable().await
    }

    async fn abort_multipart(
        &self,
        path: &RemotePath,
        upload_id: &str,
    ) -> Result<(), ProviderError> {
        self.0.abort_multipart(path, upload_id).await
    }
}

/// Signed requests the provider doesn't make: a `HEAD` for the headers R2 serves an object with,
/// and a listing without a delimiter to count the keys left under a prefix. R2 refuses unsigned
/// requests (the bucket isn't public), so they are signed with this crate's own `sign`.
struct S3Raw {
    endpoint: Endpoint,
    creds: Credentials,
    bucket: String,
    http: reqwest::Client,
}

impl S3Raw {
    fn new(env: &TestEnv) -> Self {
        assert_test_bucket(&env.bucket);
        let (endpoint, _) = s3_endpoint(env);
        // reqwest brings no TLS crypto (`rustls-no-provider`): use ring's, as the providers do.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| panic!("the HTTP client could not be built"));
        Self {
            endpoint,
            creds: Credentials {
                access_key_id: env.key_id.clone(),
                secret_access_key: env.secret.clone(),
                session_token: None,
            },
            bucket: env.bucket.clone(),
            http,
        }
    }

    /// One signed request without a body; `None` when it got no answer.
    async fn send(&self, method: reqwest::Method, url: Url) -> Option<reqwest::Response> {
        let payload = sha256_hex(b"");
        let input = SignInput {
            method: method.as_str(),
            url: &url,
            headers: &[],
            payload_sha256: &payload,
        };
        let signed = sign(&input, &self.creds, &self.endpoint.region, "s3", Utc::now());
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &signed {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).ok()?;
            let mut value = reqwest::header::HeaderValue::from_str(value).ok()?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        self.http
            .request(method, url)
            .headers(headers)
            .send()
            .await
            .ok()
    }

    /// The status and `Content-Type` of a `HEAD` of `key`.
    async fn content_type(&self, key: &RemotePath) -> (Option<u16>, Option<String>) {
        let url = object_url(&self.endpoint, &self.bucket, key);
        match self.send(reqwest::Method::HEAD, url).await {
            Some(response) => {
                let status = response.status().as_u16();
                let served = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                (Some(status), served)
            }
            None => (None, None),
        }
    }

    /// How many keys start with `prefix` (one page: at most 1,000), or `None` if the listing
    /// failed.
    async fn keys_under(&self, prefix: &str) -> Option<usize> {
        let mut url = bucket_url(&self.endpoint, &self.bucket);
        url.set_query(Some(&format!(
            "list-type=2&prefix={}",
            encode_segment(prefix)
        )));
        let response = self.send(reqwest::Method::GET, url).await?;
        if !response.status().is_success() {
            say!("    raw key listing: HTTP {}", response.status().as_u16());
            return None;
        }
        let body = response.text().await.ok()?;
        Some(body.matches("<Key>").count())
    }
}

// ---------------------------------------------------------------------------------------------
// The engine, as the app runs it
// ---------------------------------------------------------------------------------------------

/// The app's runtime (`main.rs`): multi-threaded, with I/O and timers.
fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| panic!("the runtime could not be built"))
}

/// The journal as the app opens it at start (`open_journal`): SQLite on disk, finished jobs let go.
fn open_journal(path: &Path) -> Arc<dyn Journal> {
    let journal =
        SqliteJournal::open(path).unwrap_or_else(|_| panic!("the journal could not be opened"));
    let _ = journal.clear_finished();
    Arc::new(journal)
}

struct Harness {
    env: TestEnv,
    rt: Runtime,
    /// Journals and generated files.
    temp: tempfile::TempDir,
    journals: AtomicU64,
}

impl Harness {
    fn new() -> Self {
        Self {
            env: test_env(),
            rt: runtime(),
            temp: tempfile::tempdir().unwrap_or_else(|_| panic!("no temporary folder")),
            journals: AtomicU64::new(0),
        }
    }

    fn bunny(&self, settings: &TransferSettings) -> Site {
        bunny_site(&self.env, settings, SiteId::new())
    }

    fn r2(&self, settings: &TransferSettings) -> Site {
        r2_site(&self.env, settings, SiteId::new())
    }

    /// An engine over a journal of its own, as `main.rs` makes it.
    fn engine(&self, settings: &TransferSettings) -> Engine {
        let n = self.journals.fetch_add(1, Ordering::SeqCst);
        let journal = open_journal(&self.temp.path().join(format!("journal-{n}.db")));
        Engine::new(
            self.rt.handle().clone(),
            journal,
            settings.clone(),
            describe,
        )
    }
}

/// What Settings → Save does (`Transfers::apply_settings`): the hub rebuilds the site's client
/// with the new settings, the engine's lane takes it, then the engine takes the settings.
fn apply_settings(engine: &Engine, env: &TestEnv, site: &mut Site, settings: &TransferSettings) {
    let rebuilt = bunny_site(env, settings, site.id.clone());
    if engine.sites().contains(&site.id) {
        engine.replace_provider(&site.id, rebuilt.provider.clone());
    }
    engine.set_settings(settings.clone());
    *site = rebuilt;
}

/// `Transfers::submit`: the batch is journalled (a disk write, so on a blocking thread), then runs.
async fn submit(engine: &Engine, site: &Site, jobs: Vec<Job>) {
    let batch = Batch {
        site: site.id.clone(),
        site_name: site.name.clone(),
        host: site.host.clone(),
        provider: site.provider.clone(),
        jobs,
        checksums: HashMap::new(),
    };
    let engine = engine.clone();
    let submitted = tokio::task::spawn_blocking(move || engine.submit(batch).is_ok()).await;
    assert!(
        matches!(submitted, Ok(true)),
        "the journal refused the batch"
    );
}

/// An upload's jobs, planned as `Transfers::plan_upload` plans them.
struct Planned {
    jobs: Vec<Job>,
    files: u64,
    bytes: u64,
    skipped: usize,
    took: Duration,
}

async fn plan_upload_jobs(
    site: &Site,
    sources: &[PathBuf],
    dir: &RemotePath,
    settings: &TransferSettings,
) -> Planned {
    let started = Instant::now();
    let planner = Interactive(site.provider.clone());
    let plan = plan_upload(&planner, sources, dir)
        .await
        .unwrap_or_else(|e| panic!("planning the upload failed: {}", kind(&e)));
    let (files, bytes, skipped) = (plan.files, plan.bytes, plan.skipped.len());
    for (path, why) in plan.skipped.iter().take(5) {
        say!("    skipped {}: {why}", path.display());
    }
    let all = match policy_to_resolution(settings.upload_conflicts) {
        Some(all) => all,
        None if plan.conflicts.is_empty() => Resolution::Replace,
        None => panic!(
            "{} conflicts in a fresh folder: the conflict sheet would ask",
            plan.conflicts.len()
        ),
    };
    let jobs = resolve_uploads(plan, all, &HashMap::new());
    Planned {
        jobs,
        files,
        bytes,
        skipped,
        took: started.elapsed(),
    }
}

/// How a run of the engine ended.
struct Run {
    /// From the submit until nothing was queued or running.
    elapsed: Duration,
    snapshot: EngineSnapshot,
    /// The most jobs active in any snapshot taken.
    max_active: u64,
    /// The engine stopped for the user (sign-in refused, the clock, a full disk), or the time
    /// limit passed.
    stalled: Option<String>,
}

/// Waits until the engine has nothing queued or running, taking a snapshot every `poll`, then
/// (untimed) until the journal has caught up. Uploads waiting for their check (ruling Q55) are
/// not waited for: see [`wait_for_checks`].
async fn run_until_idle(
    engine: &Engine,
    started: Instant,
    limit: Duration,
    poll: Duration,
    label: &str,
) -> Run {
    let mut max_active = 0;
    let mut last_report = Instant::now();
    let stalled = loop {
        let snap = engine.snapshot();
        max_active = max_active.max(snap.active_count);
        if !engine.has_work() {
            break None;
        }
        if let EngineState::AuthStopped { .. }
        | EngineState::ClockSkew { .. }
        | EngineState::DiskFull { .. } = snap.state
        {
            break Some(format!("{:?}", snap.state));
        }
        if started.elapsed() > limit {
            break Some(format!("still busy after {} s", limit.as_secs()));
        }
        if last_report.elapsed() >= Duration::from_secs(15) {
            last_report = Instant::now();
            say!(
                "  [{label}] {:>5.0} s: {}/{} done, {} failed, {} active, {} queued, {:.0} files/s, {:?}",
                secs(started.elapsed()),
                snap.files_done,
                snap.files_total,
                snap.files_failed,
                snap.active_count,
                snap.queued_count,
                snap.files_per_s,
                snap.state
            );
        }
        tokio::time::sleep(poll).await;
    };
    let elapsed = started.elapsed();
    let settle = Instant::now();
    while stalled.is_none()
        && (engine.pending_writes() > 0 || engine.pending_aborts() > 0)
        && settle.elapsed() < Duration::from_secs(60)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Run {
        elapsed,
        snapshot: engine.snapshot(),
        max_active,
        stalled,
    }
}

/// How the checks of ruling Q55 went: how long they took to be done with, and whether they
/// were (`false`: still pending at the time limit).
struct Checks {
    took: Duration,
    finished: bool,
}

/// Waits until no upload waits for its check (ruling Q55) and whatever the checks sent again
/// has gone and been checked in turn, then until the journal has caught up: the app left open
/// after a resume. A report every 30 s.
async fn wait_for_checks(engine: &Engine, limit: Duration, label: &str) -> Checks {
    let started = Instant::now();
    let mut last_report = Instant::now();
    let finished = loop {
        if !engine.is_busy() {
            break true;
        }
        if started.elapsed() > limit {
            break false;
        }
        if last_report.elapsed() >= Duration::from_secs(30) {
            last_report = Instant::now();
            let snap = engine.snapshot();
            say!(
                "  [{label}] {:>5.0} s: {} uploads wait for their check, {} queued or running; checked {}, re-sent {}",
                secs(started.elapsed()),
                engine.pending_checks(),
                snap.active_count + snap.queued_count,
                snap.rechecked.checked,
                snap.rechecked.resent
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    Checks {
        took: started.elapsed(),
        finished,
    }
}

fn report_failures(snapshot: &EngineSnapshot) {
    for job in snapshot.failed.iter().take(10) {
        say!(
            "    failed: {} ({})",
            job.path,
            job.error.as_deref().unwrap_or("?")
        );
    }
}

/// The harness process's private bytes (`PROCESS_MEMORY_COUNTERS_EX.PrivateUsage`), 0 if
/// Windows won't say.
fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap_or(u32::MAX),
        ..PROCESS_MEMORY_COUNTERS_EX::default()
    };
    // SAFETY: `counters` is a live, writable PROCESS_MEMORY_COUNTERS_EX and `cb` is its size,
    // which GetProcessMemoryInfo accepts in place of the smaller PROCESS_MEMORY_COUNTERS it is
    // declared with. The current process's pseudo-handle needs no closing.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        0
    } else {
        counters.PrivateUsage as u64
    }
}

/// Samples [`private_bytes`] once a second on a thread of its own, keeping the peak.
struct Memory {
    start: u64,
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    samples: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Memory {
    fn start() -> Self {
        let start = private_bytes();
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(start));
        let samples = Arc::new(AtomicU64::new(1));
        let thread = {
            let (stop, peak, samples) = (stop.clone(), peak.clone(), samples.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(1));
                    peak.fetch_max(private_bytes(), Ordering::SeqCst);
                    samples.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        Self {
            start,
            stop,
            peak,
            samples,
            thread: Some(thread),
        }
    }

    /// (private bytes at the start, the peak, how many samples).
    fn stop(mut self) -> (u64, u64, u64) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let peak = self.peak.load(Ordering::SeqCst).max(private_bytes());
        (self.start, peak, self.samples.load(Ordering::SeqCst))
    }
}

/// A measured upload: planned, submitted and run to the end on an engine of its own, with the
/// process's private bytes sampled meanwhile.
struct Upload {
    files: u64,
    bytes: u64,
    jobs: usize,
    skipped: usize,
    plan_took: Duration,
    run: Run,
    /// Private bytes before planning, the peak, the samples taken.
    memory: (u64, u64, u64),
}

impl Upload {
    fn files_per_s(&self) -> f64 {
        per_s(self.run.snapshot.files_done as f64, self.run.elapsed)
    }

    fn report(&self, name: &str) {
        let s = &self.run.snapshot;
        let total = self.plan_took + self.run.elapsed;
        say!("RESULT {name}");
        say!(
            "  files: {} planned, {} jobs, {} done, {} failed, {} skipped",
            self.files,
            self.jobs,
            s.files_done,
            s.files_failed,
            self.skipped
        );
        say!("  bytes: {} ({:.1} MiB)", self.bytes, mib(self.bytes));
        say!(
            "  wall time: planning {:.1} s + transfer {:.1} s (submit to idle) = {:.1} s",
            secs(self.plan_took),
            secs(self.run.elapsed),
            secs(total)
        );
        say!(
            "  files/s: {:.1} (transfer), {:.1} (planning + transfer)",
            self.files_per_s(),
            per_s(s.files_done as f64, total)
        );
        say!(
            "  MiB/s: {:.2} (transfer), {:.2} (planning + transfer)",
            per_s(mib(self.bytes), self.run.elapsed),
            per_s(mib(self.bytes), total)
        );
        let (start, peak, samples) = self.memory;
        say!(
            "  peak private bytes, engine without UI: {:.1} MiB ({} one-second samples; {:.1} MiB before planning)",
            mib(peak),
            samples,
            mib(start)
        );
        if let Some(why) = &self.run.stalled {
            say!("  STALLED: {why}");
        }
        report_failures(s);
    }
}

async fn upload_measured(
    h: &Harness,
    site: &Site,
    sources: &[PathBuf],
    dir: &RemotePath,
    settings: &TransferSettings,
    label: &str,
) -> Upload {
    let memory = Memory::start();
    let engine = h.engine(settings);
    let planned = plan_upload_jobs(site, sources, dir, settings).await;
    let (files, bytes, jobs, skipped, plan_took) = (
        planned.files,
        planned.bytes,
        planned.jobs.len(),
        planned.skipped,
        planned.took,
    );
    say!(
        "  [{label}] planned {files} files ({:.1} MiB) in {:.1} s",
        mib(bytes),
        secs(plan_took)
    );
    let started = Instant::now();
    submit(&engine, site, planned.jobs).await;
    let run = run_until_idle(
        &engine,
        started,
        Duration::from_secs(3600),
        Duration::from_millis(250),
        label,
    )
    .await;
    Upload {
        files,
        bytes,
        jobs,
        skipped,
        plan_took,
        run,
        memory: memory.stop(),
    }
}

/// A delete: planned by `plan_delete` and run by the engine, as after the delete alert.
struct Removal {
    files: u64,
    bytes: u64,
    jobs: usize,
    folder_jobs: usize,
    /// Every folder job comes after every file job (ruling Q54).
    folders_last: bool,
    plan_took: Duration,
    run: Run,
}

/// Deletes `entries` (rows of a listing) as `Transfers::delete` and `delete_confirm` do. Refuses
/// anything outside an `accept-*` folder.
async fn delete_entries(
    engine: &Engine,
    site: &Site,
    entries: &[Entry],
    limit: Duration,
    label: &str,
) -> Removal {
    for entry in entries {
        assert!(
            entry
                .path
                .segments()
                .next()
                .is_some_and(|top| top.starts_with(ACCEPT)),
            "refusing to delete {} (outside accept-*)",
            entry.path.as_str()
        );
    }
    let planner = Interactive(site.provider.clone());
    let started = Instant::now();
    let plan = plan_delete(&planner, entries)
        .await
        .unwrap_or_else(|e| panic!("planning the delete failed: {}", kind(&e)));
    let plan_took = started.elapsed();
    let is_folder = |job: &Job| matches!(job, Job::Delete { folder: true, .. });
    let folder_jobs = plan.jobs.iter().filter(|job| is_folder(job)).count();
    let folders_last = plan
        .jobs
        .iter()
        .skip_while(|job| !is_folder(job))
        .all(is_folder);
    let (files, bytes, jobs) = (
        plan.summary.total_files,
        plan.summary.total_bytes,
        plan.jobs.len(),
    );
    say!(
        "  [{label}] delete plan: {files} files ({:.1} MiB), {jobs} jobs ({folder_jobs} folder jobs, all last: {folders_last}), planned in {:.1} s",
        mib(bytes),
        secs(plan_took)
    );
    let started = Instant::now();
    submit(engine, site, plan.jobs).await;
    let run = run_until_idle(engine, started, limit, Duration::from_millis(250), label).await;
    Removal {
        files,
        bytes,
        jobs,
        folder_jobs,
        folders_last,
        plan_took,
        run,
    }
}

/// The row named `path` in the listing of `dir`.
async fn find_in(site: &Site, dir: &RemotePath, path: &RemotePath) -> Option<Entry> {
    let listing = list_interactive(&*site.provider, dir).await.ok()?;
    listing
        .entries
        .into_iter()
        .find(|entry| entry.path == *path)
}

/// Whether `path` is gone from the listing of `dir` (a missing `dir` counts). Bunny's listings
/// can lag a delete (Task 7), so it is looked for up to six times, 2 s apart.
async fn gone_from(site: &Site, dir: &RemotePath, path: &RemotePath) -> bool {
    for attempt in 0..6 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        match list_interactive(&*site.provider, dir).await {
            Ok(listing) if !listing.entries.iter().any(|entry| entry.path == *path) => {
                return true;
            }
            Ok(_) => {}
            Err(ProviderError::NotFound) => return true,
            Err(e) => say!("    listing {}: {}", dir.as_str(), kind(&e)),
        }
    }
    false
}

/// Deletes `prefix` (an `accept-*` folder) through a fresh engine, as the app deletes, and says
/// whether it is gone: from the root listing, and for S3 with no key left under it.
async fn remove_prefix(h: &Harness, site: &Site, prefix: &RemotePath, raw: Option<&S3Raw>) -> bool {
    let root = RemotePath::root();
    match find_in(site, &root, prefix).await {
        Some(entry) => {
            let engine = h.engine(&TransferSettings::default());
            let removal = delete_entries(
                &engine,
                site,
                &[entry],
                Duration::from_secs(3600),
                "cleanup",
            )
            .await;
            say!(
                "  cleanup of {}/ on {}: {} deleted, {} failed, {:.1} s",
                prefix.as_str(),
                site.name,
                removal.run.snapshot.files_done,
                removal.run.snapshot.files_failed,
                secs(removal.run.elapsed)
            );
        }
        None => say!(
            "  cleanup: {}/ is not in the root of {}",
            prefix.as_str(),
            site.name
        ),
    }
    let gone = gone_from(site, &root, prefix).await;
    let keys = match raw {
        Some(raw) => raw.keys_under(&format!("{}/", prefix.as_str())).await,
        None => Some(0),
    };
    say!(
        "  {}/ gone from the root of {}: {gone}; keys left under it: {keys:?}",
        prefix.as_str(),
        site.name
    );
    gone && keys == Some(0)
}

/// Runs `body`; if it panics, runs `cleanup` (a panic there is only reported) and panics again.
fn guarded(body: impl FnOnce(), cleanup: impl FnOnce()) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(body)) {
        say!("  the test failed: cleaning up");
        if catch_unwind(AssertUnwindSafe(cleanup)).is_err() {
            say!("  the cleanup failed too: run sweep_accept_prefixes");
        }
        resume_unwind(panic);
    }
}

// ---------------------------------------------------------------------------------------------
// Local and remote files
// ---------------------------------------------------------------------------------------------

/// A folder for this run: `accept-<unix time>[-<tag>]`.
fn fresh_prefix(tag: &str) -> RemotePath {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = if tag.is_empty() {
        format!("{ACCEPT}{now}")
    } else {
        format!("{ACCEPT}{now}-{tag}")
    };
    RemotePath::parse(&name).unwrap_or_else(|_| panic!("bad folder name {name}"))
}

/// Every file under `root`, as (its path below `root` with `/`, its size), sorted. Read only.
fn local_files(root: &Path) -> Vec<(String, u64)> {
    let mut found = Vec::new();
    let mut waiting = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = waiting.pop() {
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let file_type = entry
                .file_type()
                .unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
            if file_type.is_dir() {
                waiting.push((entry.path(), path));
            } else if file_type.is_file() {
                let size = entry
                    .metadata()
                    .unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
                    .len();
                found.push((path, size));
            }
        }
    }
    found.sort();
    found
}

fn total_size(files: &[(String, u64)]) -> u64 {
    files.iter().map(|(_, size)| size).sum()
}

/// What is directly in `dir`, sorted: what Ctrl+A selects there.
fn children(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    found.sort();
    found
}

/// Files on a site, by path below the folder walked: (size, last change).
type RemoteFiles = HashMap<String, (u64, Option<DateTime<Utc>>)>;

/// Every file under `dir` on the site.
async fn remote_files(site: &Site, dir: &RemotePath) -> Result<RemoteFiles, ProviderError> {
    let planner = Interactive(site.provider.clone());
    let base = format!("{}/", dir.as_str());
    Ok(walk(&planner, dir)
        .await?
        .into_iter()
        .filter(|entry| !entry.is_folder)
        .filter_map(|entry| {
            let rel = entry.path.as_str().strip_prefix(&base)?.to_owned();
            Some((rel, (entry.size, entry.modified)))
        })
        .collect())
}

struct Verified {
    remote: RemoteFiles,
    missing: Vec<String>,
    wrong_size: Vec<String>,
}

/// Checks that every local file is under `dir` on the site, at `<under><its path>`, with its size.
/// Listings can lag, so a shortfall is looked at again (three walks at most, 10 s apart).
async fn verify(site: &Site, dir: &RemotePath, local: &[(String, u64)], under: &str) -> Verified {
    let mut walks = 0;
    loop {
        walks += 1;
        let remote = remote_files(site, dir).await.unwrap_or_else(|e| {
            say!("    walking {}: {}", dir.as_str(), kind(&e));
            HashMap::new()
        });
        let mut missing = Vec::new();
        let mut wrong_size = Vec::new();
        for (rel, size) in local {
            let key = format!("{under}{rel}");
            match remote.get(&key) {
                None => missing.push(key),
                Some((remote_size, _)) if remote_size != size => wrong_size.push(key),
                Some(_) => {}
            }
        }
        if (missing.is_empty() && wrong_size.is_empty()) || walks == 3 {
            return Verified {
                remote,
                missing,
                wrong_size,
            };
        }
        say!(
            "    {} missing, {} of another size: walking again in 10 s (listings can lag)",
            missing.len(),
            wrong_size.len()
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

fn report_verified(verified: &Verified, local: usize) {
    say!(
        "  remote check: {} of {local} local files there with their size; {} missing, {} of another size, {} files on the site in all",
        local - verified.missing.len() - verified.wrong_size.len(),
        verified.missing.len(),
        verified.wrong_size.len(),
        verified.remote.len()
    );
    for path in verified.missing.iter().take(5) {
        say!("    missing: {path}");
    }
    for path in verified.wrong_size.iter().take(5) {
        say!("    another size: {path}");
    }
}

/// Where `s1_bunny_upload_export` leaves the name of its folder for `bunny_delete_export`.
fn s1_state() -> PathBuf {
    std::env::temp_dir()
        .join("httporter-acceptance")
        .join("s1-prefix.txt")
}

fn save_s1_prefix(prefix: &RemotePath) {
    let path = s1_state();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(&path, prefix.as_str());
}

fn load_s1_prefix() -> Option<RemotePath> {
    let text = fs::read_to_string(s1_state()).ok()?;
    let text = text.trim();
    if !text.starts_with(ACCEPT) || text.contains('/') {
        return None;
    }
    RemotePath::parse(text).ok()
}

fn clear_s1_prefix() {
    let _ = fs::remove_file(s1_state());
}

/// Writes `count` files `file-NNN.dat` into `dir` (a temporary folder, never the export).
fn make_files(dir: &Path, count: usize, contents: impl Fn(usize) -> Vec<u8>) {
    fs::create_dir_all(dir).unwrap_or_else(|e| panic!("cannot make {}: {e}", dir.display()));
    for i in 0..count {
        let path = dir.join(format!("file-{i:03}.dat"));
        fs::write(&path, contents(i)).unwrap_or_else(|e| panic!("cannot write a test file: {e}"));
    }
}

/// `len` bytes that don't compress, from `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// 1. S1: the reference export to Bunny
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "uploads the reference export to the Bunny test zone; see the module docs"]
fn s1_bunny_upload_export() {
    let h = Harness::new();
    let settings = TransferSettings::default();
    let site = h.bunny(&settings);
    let prefix = fresh_prefix("");
    let export = Path::new(EXPORT);
    let local = local_files(export);
    say!(
        "S1: the reference export ({} files, {:.1} MiB) to Bunny zone {} under {}/, default settings",
        local.len(),
        mib(total_size(&local)),
        site.name,
        prefix.as_str()
    );
    guarded(
        || {
            h.rt.block_on(async {
                let upload =
                    upload_measured(&h, &site, &children(export), &prefix, &settings, "s1").await;
                upload.report("s1_bunny_upload_export");
                say!(
                    "  target S1 >= 94 files/s: {:.1} files/s (transfer) -> {}",
                    upload.files_per_s(),
                    verdict(upload.files_per_s() >= 94.0)
                );
                assert!(upload.run.stalled.is_none(), "the upload stalled");
                assert_eq!(upload.run.snapshot.files_failed, 0, "uploads failed");
                assert_eq!(
                    upload.files,
                    local.len() as u64,
                    "not every file was planned"
                );
                let verified = verify(&site, &prefix, &local, "").await;
                report_verified(&verified, local.len());
                assert!(
                    verified.missing.is_empty() && verified.wrong_size.is_empty(),
                    "the zone does not hold the export"
                );
            });
            save_s1_prefix(&prefix);
            say!(
                "  {}/ stays in the zone for bunny_delete_export",
                prefix.as_str()
            );
        },
        || {
            h.rt.block_on(remove_prefix(&h, &site, &prefix, None));
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 2. Deleting the export's folder on Bunny (ruling Q54)
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "deletes the export's folder from the Bunny test zone; see the module docs"]
fn bunny_delete_export() {
    let h = Harness::new();
    let settings = TransferSettings::default();
    let site = h.bunny(&settings);
    let root = RemotePath::root();
    let export = Path::new(EXPORT);
    let local = local_files(export);
    let (prefix, from_s1) = match load_s1_prefix() {
        Some(prefix) if h.rt.block_on(find_in(&site, &root, &prefix)).is_some() => (prefix, true),
        _ => (fresh_prefix(""), false),
    };
    guarded(
        || {
            h.rt.block_on(async {
                if from_s1 {
                    say!("delete: {}/ as s1_bunny_upload_export left it", prefix.as_str());
                } else {
                    say!(
                        "delete: no upload of s1_bunny_upload_export is left; uploading the export to {}/ first (not measured)",
                        prefix.as_str()
                    );
                    let upload =
                        upload_measured(&h, &site, &children(export), &prefix, &settings, "setup")
                            .await;
                    assert!(
                        upload.run.stalled.is_none() && upload.run.snapshot.files_failed == 0,
                        "the setup upload failed"
                    );
                }
                let engine = h.engine(&settings);
                let entry = find_in(&site, &root, &prefix)
                    .await
                    .unwrap_or_else(|| panic!("{}/ is not in the zone root", prefix.as_str()));
                // The user selects the folder's row in the root and confirms the delete alert.
                let removal = delete_entries(
                    &engine,
                    &site,
                    &[entry],
                    Duration::from_secs(3600),
                    "delete",
                )
                .await;
                let s = &removal.run.snapshot;
                let gone = gone_from(&site, &root, &prefix).await;
                let listing = list_interactive(&*site.provider, &root).await;
                let accept_left = listing.as_ref().map_or(usize::MAX, |listing| {
                    listing
                        .entries
                        .iter()
                        .filter(|entry| entry.name().starts_with(ACCEPT))
                        .count()
                });
                say!("RESULT bunny_delete_export");
                say!(
                    "  plan: {} files ({:.1} MiB) found by walking the folder in {:.1} s; {} jobs = {} file deletes + {} folder delete(s), folder last: {}",
                    removal.files,
                    mib(removal.bytes),
                    secs(removal.plan_took),
                    removal.jobs,
                    removal.jobs - removal.folder_jobs,
                    removal.folder_jobs,
                    removal.folders_last
                );
                say!(
                    "  delete: {:.1} s wall time (submit to idle), {:.1} files/s; {} done, {} failed",
                    secs(removal.run.elapsed),
                    per_s(removal.files as f64, removal.run.elapsed),
                    s.files_done,
                    s.files_failed
                );
                say!(
                    "  total with planning: {:.1} s, {:.1} files/s",
                    secs(removal.plan_took + removal.run.elapsed),
                    per_s(removal.files as f64, removal.plan_took + removal.run.elapsed)
                );
                say!(
                    "  zone root afterwards: {} entries, {}/ gone: {gone}, accept-* entries left: {accept_left}",
                    listing.as_ref().map_or(0, |listing| listing.entries.len()),
                    prefix.as_str()
                );
                if let Some(why) = &removal.run.stalled {
                    say!("  STALLED: {why}");
                }
                report_failures(s);
                assert!(removal.run.stalled.is_none(), "the delete stalled");
                assert_eq!(s.files_failed, 0, "deletes failed");
                assert_eq!(removal.files, local.len() as u64, "the walk missed files");
                assert!(removal.folders_last, "a folder job came before a file job");
                assert!(gone, "the folder is still listed");
            });
            clear_s1_prefix();
        },
        || {
            h.rt.block_on(remove_prefix(&h, &site, &prefix, None));
            clear_s1_prefix();
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 3. S5: killed mid-upload, resumed from the journal
// ---------------------------------------------------------------------------------------------

/// The child's batch: (jobs, files, bytes).
type ChildBatch = (u64, u64, u64);

/// What the parent saw of the child.
#[derive(Default)]
struct ChildSeen {
    /// The child's batch, and when the child said it was submitted.
    submitted: Mutex<Option<(ChildBatch, Instant)>>,
    /// The child's last count of files done.
    done: AtomicU64,
}

impl ChildSeen {
    /// The markers may come after other text on the line: the test runner prints
    /// `test s5_crash_resume ... ` without a line break before the child's first words.
    fn read(&self, line: &str) {
        if let Some((_, rest)) = line.split_once(SUBMITTED) {
            let numbers: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|n| n.parse().ok())
                .collect();
            if let [jobs, files, bytes] = numbers.as_slice() {
                *self
                    .submitted
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    Some(((*jobs, *files, *bytes), Instant::now()));
            }
        } else if let Some((_, rest)) = line.split_once(PROGRESS)
            && let Some(done) = rest.split_whitespace().next().and_then(|n| n.parse().ok())
        {
            self.done.store(done, Ordering::SeqCst);
        }
    }

    fn submitted(&self) -> Option<(ChildBatch, Instant)> {
        *self
            .submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Passes the child's lines on, scrubbed, and notes what they say.
fn forward<R: Read + Send + 'static>(
    stream: Option<R>,
    tag: &'static str,
    seen: Arc<ChildSeen>,
) -> Option<JoinHandle<()>> {
    let stream = stream?;
    Some(std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                break;
            };
            seen.read(&line);
            say!("  {tag}| {line}");
        }
    }))
}

/// Kills the child if it is still running when this goes (a failed test included).
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct ChildRun {
    /// (jobs, files, bytes) of the child's batch.
    submitted: ChildBatch,
    /// The child's own count of files done, at its last report before the kill.
    done_at_kill: u64,
    killed_after: Duration,
    killed_at: Instant,
    /// The remote paths of the child's jobs in flight, at most 250 ms before the kill.
    in_flight: Vec<String>,
}

/// Starts this test binary again as the child, which uploads [`S5_SOURCE`] into `prefix` with its
/// journal at `journal`, and kills it hard (`TerminateProcess`) about [`KILL_AFTER`] after its
/// start, once it has submitted its batch.
fn run_child_and_kill(journal: &Path, prefix: &RemotePath, site: &SiteId) -> ChildRun {
    let spec = format!("{}|{}|{}", journal.display(), prefix.as_str(), site.0);
    let exe = std::env::current_exe().unwrap_or_else(|_| panic!("the test binary is not found"));
    let child = Command::new(exe)
        .args([
            "s5_crash_resume",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &spec)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("the child did not start: {e}"));
    let spawned = Instant::now();
    let mut child = KillOnDrop(child);
    let seen = Arc::new(ChildSeen::default());
    let readers = [
        forward(child.0.stdout.take(), "child", seen.clone()),
        forward(child.0.stderr.take(), "child stderr", seen.clone()),
    ];
    let submitted = loop {
        if let Some((numbers, at)) = seen.submitted()
            && spawned.elapsed() >= KILL_AFTER
            && at.elapsed() >= Duration::from_secs(5)
        {
            break numbers;
        }
        if let Ok(Some(_)) = child.0.try_wait() {
            panic!("the child ended before it was killed");
        }
        assert!(
            spawned.elapsed() < Duration::from_secs(180),
            "the child never submitted its upload"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let done_at_kill = seen.done.load(Ordering::SeqCst);
    // TerminateProcess: no destructor runs, nothing is flushed.
    child
        .0
        .kill()
        .unwrap_or_else(|e| panic!("the child could not be killed: {e}"));
    let _ = child.0.wait();
    let killed_after = spawned.elapsed();
    let killed_at = Instant::now();
    drop(child);
    for reader in readers.into_iter().flatten() {
        let _ = reader.join();
    }
    let in_flight: Vec<String> = fs::read_to_string(in_flight_file(journal))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    say!(
        "  child killed {:.1} s after it started; it had reported {done_at_kill} files done, {} jobs in flight",
        secs(killed_after),
        in_flight.len()
    );
    ChildRun {
        submitted,
        done_at_kill,
        killed_after,
        killed_at,
        in_flight,
    }
}

/// The child: uploads [`S5_SOURCE`] as the app would, then reports progress until it is killed
/// (ten minutes at most, should the parent be gone).
fn s5_child(spec: &str) {
    let mut parts = spec.split('|');
    let (Some(journal), Some(prefix), Some(site), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        panic!("the child's spec is not <journal>|<prefix>|<site>");
    };
    let prefix = RemotePath::parse(prefix).unwrap_or_else(|_| panic!("bad prefix"));
    assert!(
        prefix.as_str().starts_with(ACCEPT),
        "the child only uploads into accept-*"
    );
    let env = test_env();
    let settings = TransferSettings::default();
    let site = bunny_site(&env, &settings, SiteId(site.to_owned()));
    let rt = runtime();
    rt.block_on(async {
        let engine = Engine::new(
            tokio::runtime::Handle::current(),
            open_journal(Path::new(journal)),
            settings.clone(),
            describe,
        );
        let planned =
            plan_upload_jobs(&site, &[PathBuf::from(S5_SOURCE)], &prefix, &settings).await;
        let (jobs, files, bytes) = (planned.jobs.len(), planned.files, planned.bytes);
        submit(&engine, &site, planned.jobs).await;
        say!("{SUBMITTED}{jobs} {files} {bytes}");
        // Diagnostics: the jobs in flight, written every 250 ms for the parent to read once
        // this process is killed.
        let in_flight = in_flight_file(Path::new(journal));
        let started = Instant::now();
        let mut ticks = 0u64;
        while started.elapsed() < Duration::from_secs(600) {
            let snapshot = engine.snapshot();
            let paths: Vec<&str> = snapshot
                .active
                .iter()
                .map(|job| job.path.as_str())
                .collect();
            let _ = fs::write(&in_flight, paths.join("\n"));
            if ticks.is_multiple_of(8) {
                say!(
                    "{PROGRESS}{} {}",
                    snapshot.files_done,
                    snapshot.active_count
                );
            }
            ticks += 1;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

/// Where the child writes the remote paths of its jobs in flight.
fn in_flight_file(journal: &Path) -> PathBuf {
    journal.with_extension("in-flight.txt")
}

/// The words of an upload's "possibly sent" mark in the journal's multipart column (ruling Q55).
const UPLOAD_SENT: &str = "httporter:upload-sent";

/// The app's next start and Resume on the banner: the journal opened (finished jobs let go), the
/// unfinished jobs counted, then `resume_from_journal` with the site's client
/// (`Transfers::resume_journal`). The app is left open until the uploads the kill cut off have
/// been checked on the zone (ruling Q55). Then every local file is checked on the zone.
async fn resume_and_verify(
    h: &Harness,
    site: &Site,
    prefix: &RemotePath,
    journal_path: &Path,
    local: &[(String, u64)],
    child: &ChildRun,
) {
    let journal = open_journal(journal_path);
    let records = journal
        .unfinished()
        .unwrap_or_else(|_| panic!("the journal cannot be read"));
    let left = records.len();
    // Diagnostics: which jobs the journal held unfinished, and every upload call of the resume.
    let unfinished: Vec<String> = records
        .iter()
        .filter_map(|record| match &record.job {
            Job::Upload { remote, .. } => Some(remote.as_str().to_owned()),
            _ => None,
        })
        .collect();
    let marked: Vec<String> = records
        .iter()
        .filter(|record| {
            record
                .multipart
                .as_ref()
                .is_some_and(|mark| mark.upload_id == UPLOAD_SENT)
        })
        .filter_map(|record| match &record.job {
            Job::Upload { remote, .. } => Some(remote.as_str().to_owned()),
            _ => None,
        })
        .collect();
    let probe = Arc::new(Probe::new(site.provider.clone()));
    let probed = Site {
        provider: probe.clone(),
        ..site.clone()
    };
    let engine = Engine::new(
        h.rt.handle().clone(),
        journal.clone(),
        TransferSettings::default(),
        describe,
    );
    let banner = engine.unfinished();
    let (jobs, files, bytes) = child.submitted;
    let jobs = usize::try_from(jobs).unwrap_or(usize::MAX);
    let before = remote_files(site, prefix).await.unwrap_or_default();
    say!(
        "  after the kill: {left} of {jobs} jobs unfinished in the journal; the resume banner offers {} ({} deletes); {} files on the zone",
        banner.total,
        banner.deletes,
        before.len()
    );
    let started = Instant::now();
    let resumed = {
        let (engine, site) = (engine.clone(), probed);
        tokio::task::spawn_blocking(move || {
            let lookup = move |id: &SiteId| -> Option<SiteInfo> {
                (*id == site.id)
                    .then(|| (site.name.clone(), site.host.clone(), site.provider.clone()))
            };
            engine.resume_from_journal(&lookup)
        })
        .await
        .unwrap_or_else(|_| panic!("resuming panicked"))
    };
    let run = run_until_idle(
        &engine,
        started,
        Duration::from_secs(1800),
        Duration::from_millis(250),
        "s5 resume",
    )
    .await;
    let waiting = engine.pending_checks();
    let checks = wait_for_checks(
        &engine,
        CHECK_AFTER * (u32::from(MAX_CHECKS) + 1) + Duration::from_secs(300),
        "s5 checks",
    )
    .await;
    let checked = engine.snapshot();
    let left_after = journal
        .unfinished()
        .map_or(usize::MAX, |records| records.len());
    let verified = verify(site, prefix, local, "plugins/").await;
    let mut reuploaded = 0;
    for (path, (_, modified)) in &before {
        if verified
            .remote
            .get(path)
            .is_some_and(|(_, now)| now != modified)
        {
            reuploaded += 1;
        }
    }
    let s = &run.snapshot;
    say!("RESULT s5_crash_resume");
    say!(
        "  source: {} files ({:.1} MiB); the child planned {files} files in {jobs} jobs ({:.1} MiB)",
        local.len(),
        mib(total_size(local)),
        mib(bytes)
    );
    say!(
        "  before the kill ({:.1} s after the child started): {} jobs done per the journal, {} per the child's last report",
        secs(child.killed_after),
        jobs.saturating_sub(left),
        child.done_at_kill
    );
    say!(
        "  resume: {resumed} jobs re-queued; {} done, {} failed in {:.1} s",
        s.files_done,
        s.files_failed,
        secs(run.elapsed)
    );
    // Ruling Q55: the uploads the kill cut off, and their checks.
    let resent_after: Vec<f64> = child
        .in_flight
        .iter()
        .filter_map(|path| probe.answered_for(path).first().copied())
        .map(|answered| secs(answered.saturating_duration_since(child.killed_at)))
        .collect();
    let earliest = resent_after.iter().copied().fold(f64::INFINITY, f64::min);
    let latest = resent_after.iter().copied().fold(0.0, f64::max);
    say!(
        "  cut off by the kill: {} in flight, {} marked in the journal ({} of the in-flight ones); sent again and answered {:.0}-{:.0} s after the kill",
        child.in_flight.len(),
        marked.len(),
        child
            .in_flight
            .iter()
            .filter(|path| marked.contains(path))
            .count(),
        earliest,
        latest
    );
    say!(
        "  checks (ruling Q55): {waiting} waited when the resume ended; done with {:.0} s later (finished: {}); checked {}, re-sent {}, {} listings; afterwards {} done, {} failed; {left_after} unfinished in the journal",
        secs(checks.took),
        checks.finished,
        checked.rechecked.checked,
        checked.rechecked.resent,
        probe.lists.load(Ordering::SeqCst),
        checked.files_done,
        checked.files_failed
    );
    for path in &child.in_flight {
        let answered = probe.answered_for(path);
        if answered.len() > 1 {
            let after: Vec<String> = answered
                .iter()
                .map(|at| {
                    format!(
                        "{:.0} s",
                        secs(at.saturating_duration_since(child.killed_at))
                    )
                })
                .collect();
            say!(
                "    re-sent after its check: {path}: answered {} after the kill",
                after.join(", then ")
            );
        }
    }
    say!(
        "  of the {} files already on the zone when resuming, {reuploaded} were uploaded again (jobs in flight at the kill, or whose 'done' had not reached the journal)",
        before.len()
    );
    report_verified(&verified, local.len());
    say!("  resume's upload calls: {}", probe.summary());
    for rel in &verified.missing {
        let remote = format!("{}/{rel}", prefix.as_str());
        let fetched = match RemotePath::parse(&remote) {
            Ok(path) => match site.provider.download(&path).await {
                Ok(_) => "Ok".to_owned(),
                Err(e) => kind(&e),
            },
            Err(_) => "bad path".to_owned(),
        };
        say!(
            "    {rel}: in flight at the kill: {}; unfinished in the journal: {}; on the zone before resuming: {}; the resume's upload calls: {:?}; a GET now: {fetched}",
            child.in_flight.contains(&remote),
            unfinished.contains(&remote),
            before.contains_key(rel),
            probe.calls_for(&remote)
        );
    }
    if let Some(why) = &run.stalled {
        say!("  STALLED: {why}");
    }
    report_failures(&checked);
    let pass = left > 0
        && banner.total == left
        && resumed == left
        && run.stalled.is_none()
        && checks.finished
        && checked.files_failed == 0
        && left_after == 0
        && verified.missing.is_empty()
        && verified.wrong_size.is_empty();
    say!(
        "  target S5 (offers to resume, finishes, nothing failed, completed files not sent again, every file on the zone): {}",
        verdict(pass)
    );
    assert!(
        left > 0,
        "the child finished before it was killed: nothing to resume"
    );
    assert_eq!(
        banner.total, left,
        "the banner does not offer every unfinished job"
    );
    assert_eq!(resumed, left, "not every unfinished job was resumed");
    assert!(run.stalled.is_none(), "the resumed upload stalled");
    assert!(checks.finished, "the checks did not finish");
    assert_eq!(checked.files_failed, 0, "resumed uploads failed");
    assert_eq!(left_after, 0, "the journal still holds unfinished jobs");
    assert!(
        verified.missing.is_empty() && verified.wrong_size.is_empty(),
        "the zone does not hold every file"
    );
}

#[test]
#[ignore = "uploads to the Bunny test zone from a child process it kills; see the module docs"]
fn s5_crash_resume() {
    if let Ok(spec) = std::env::var(CHILD_ENV) {
        s5_child(&spec);
        return;
    }
    let h = Harness::new();
    let settings = TransferSettings::default();
    let site = h.bunny(&settings);
    let prefix = fresh_prefix("s5");
    let source = Path::new(S5_SOURCE);
    let local = local_files(source);
    let journal_path = h.temp.path().join("s5-journal.db");
    say!(
        "S5: {} files ({:.1} MiB) of {} to {}/ from a child process, killed about {} s after it starts, then resumed from its journal",
        local.len(),
        mib(total_size(&local)),
        source.display(),
        prefix.as_str(),
        KILL_AFTER.as_secs()
    );
    guarded(
        || {
            let child = run_child_and_kill(&journal_path, &prefix, &site.id);
            h.rt.block_on(resume_and_verify(
                &h,
                &site,
                &prefix,
                &journal_path,
                &local,
                &child,
            ));
            assert!(
                h.rt.block_on(remove_prefix(&h, &site, &prefix, None)),
                "the folder was not removed"
            );
        },
        || {
            h.rt.block_on(remove_prefix(&h, &site, &prefix, None));
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 4. S10: the reference export to R2, and the types it is served with
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "uploads the reference export to the R2 test bucket; see the module docs"]
fn s10_r2_upload_and_types() {
    let h = Harness::new();
    let settings = TransferSettings::default();
    let site = h.r2(&settings);
    let raw = S3Raw::new(&h.env);
    let prefix = fresh_prefix("");
    let root = RemotePath::root();
    let export = Path::new(EXPORT);
    let local = local_files(export);
    say!(
        "S10: the reference export ({} files, {:.1} MiB) to R2 bucket {} under {}/, default settings",
        local.len(),
        mib(total_size(&local)),
        site.name,
        prefix.as_str()
    );
    guarded(
        || {
            h.rt.block_on(async {
                let upload =
                    upload_measured(&h, &site, &children(export), &prefix, &settings, "s10").await;
                upload.report("s10_r2_upload_and_types");
                assert!(upload.run.stalled.is_none(), "the upload stalled");
                assert_eq!(upload.run.snapshot.files_failed, 0, "uploads failed");
                let verified = verify(&site, &prefix, &local, "").await;
                report_verified(&verified, local.len());

                // The types R2 serves (a signed HEAD: the bucket is not public).
                let mut types_ok = true;
                for (ext, expected) in [
                    ("html", "text/html"),
                    ("css", "text/css"),
                    ("js", "javascript"),
                    ("webp", "image/webp"),
                ] {
                    let Some((rel, _)) = local.iter().find(|(rel, _)| {
                        Path::new(rel).extension().and_then(|e| e.to_str()) == Some(ext)
                    }) else {
                        say!("  .{ext}: no such file in the export");
                        types_ok = false;
                        continue;
                    };
                    let key = RemotePath::parse(&format!("{}/{rel}", prefix.as_str()))
                        .unwrap_or_else(|_| panic!("bad key"));
                    let (status, served) = raw.content_type(&key).await;
                    let good = status == Some(200)
                        && served.as_deref().is_some_and(|t| t.contains(expected));
                    types_ok &= good;
                    say!(
                        "  .{ext}: {rel} -> HTTP {status:?}, Content-Type {served:?} (expected {expected}): {}",
                        verdict(good)
                    );
                }

                // Deleting the folder through the planner and the engine.
                let engine = h.engine(&settings);
                let entry = find_in(&site, &root, &prefix)
                    .await
                    .unwrap_or_else(|| panic!("{}/ is not in the bucket root", prefix.as_str()));
                let removal =
                    delete_entries(&engine, &site, &[entry], Duration::from_secs(3600), "delete")
                        .await;
                let gone = gone_from(&site, &root, &prefix).await;
                let keys = raw.keys_under(&format!("{}/", prefix.as_str())).await;
                say!(
                    "  delete: {} files in {} jobs ({} folder), planned in {:.1} s, deleted in {:.1} s ({:.1} files/s), {} failed",
                    removal.files,
                    removal.jobs,
                    removal.folder_jobs,
                    secs(removal.plan_took),
                    secs(removal.run.elapsed),
                    per_s(removal.files as f64, removal.run.elapsed),
                    removal.run.snapshot.files_failed
                );
                say!(
                    "  bucket afterwards: {}/ gone from the root: {gone}; keys left under it: {keys:?}",
                    prefix.as_str()
                );
                report_failures(&removal.run.snapshot);
                say!(
                    "  target S10: types served correctly: {}; no upload failed: {}",
                    verdict(types_ok),
                    verdict(upload.run.snapshot.files_failed == 0)
                );
                assert!(
                    verified.missing.is_empty() && verified.wrong_size.is_empty(),
                    "the bucket does not hold the export"
                );
                assert!(types_ok, "a type is not served as expected");
                assert_eq!(removal.run.snapshot.files_failed, 0, "deletes failed");
                assert!(gone && keys == Some(0), "the folder is not gone");
            });
        },
        || {
            h.rt.block_on(remove_prefix(&h, &site, &prefix, Some(&raw)));
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 5. S9: wrong credentials
// ---------------------------------------------------------------------------------------------

/// `secret` with its last character changed: a digit to the next digit, a letter to the next
/// letter of the same case, anything else to `x`.
fn altered(secret: &str) -> String {
    let mut chars: Vec<char> = secret.chars().collect();
    if let Some(last) = chars.last_mut() {
        *last = match *last {
            '9' => '0',
            'z' => 'a',
            'Z' => 'A',
            c @ ('0'..='8' | 'a'..='y' | 'A'..='Y') => char::from(c as u8 + 1),
            _ => 'x',
        };
    }
    chars.into_iter().collect()
}

/// What Test Connection did (`Sites::test_prepared`): the client built on a blocking thread, then
/// the root listed with the browsing retries.
struct Tested {
    build: Duration,
    total: Duration,
    /// How many times the listing was tried: one request each (a root listing makes no other).
    attempts: u32,
    result: Result<usize, ProviderError>,
}

async fn test_connection(
    build: impl FnOnce() -> Result<Arc<dyn StorageProvider>, ProviderError> + Send + 'static,
) -> Tested {
    let started = Instant::now();
    let built = tokio::task::spawn_blocking(build)
        .await
        .unwrap_or_else(|_| panic!("building the client panicked"));
    let build = started.elapsed();
    let provider = match built {
        Ok(provider) => provider,
        Err(e) => {
            return Tested {
                build,
                total: build,
                attempts: 0,
                result: Err(e),
            };
        }
    };
    let attempts = AtomicU32::new(0);
    let root = RemotePath::root();
    let listed = with_retry(&RetryPolicy::interactive(), |_| {
        attempts.fetch_add(1, Ordering::SeqCst);
        provider.list(&root)
    })
    .await;
    Tested {
        build,
        total: started.elapsed(),
        attempts: attempts.load(Ordering::SeqCst),
        result: listed.map(|listing| listing.entries.len()),
    }
}

fn report_tested(label: &str, tested: &Tested) -> bool {
    let auth = matches!(tested.result, Err(ProviderError::Auth { .. }));
    let pass = auth && tested.total < Duration::from_secs(2) && tested.attempts == 1;
    let outcome = match &tested.result {
        Ok(entries) => format!("Ok ({entries} entries)"),
        Err(e) => kind(e),
    };
    say!(
        "  {label}: {outcome} after {} ms ({} ms building the client, {} ms listing), {} request(s) -> {}",
        tested.total.as_millis(),
        tested.build.as_millis(),
        (tested.total - tested.build).as_millis(),
        tested.attempts,
        verdict(pass)
    );
    pass
}

#[test]
#[ignore = "signs in to the Bunny test zone and the R2 test bucket with wrong credentials"]
fn s9_wrong_credentials() {
    let h = Harness::new();
    let settings = TransferSettings::default();
    let wrong_password = altered(&h.env.password);
    let wrong_secret = altered(&h.env.secret);
    keep_secret(&wrong_password);
    keep_secret(&wrong_secret);
    assert_ne!(wrong_password, h.env.password);
    assert_ne!(wrong_secret, h.env.secret);
    let (bunny, s3) = h.rt.block_on(async {
        let (env, s) = (h.env.clone(), settings.clone());
        let bunny = test_connection(move || {
            let (storage, _) = bunny_provider(&env, &wrong_password, &s)?;
            Ok(Arc::new(storage) as Arc<dyn StorageProvider>)
        })
        .await;
        let (env, s) = (h.env.clone(), settings.clone());
        let s3 = test_connection(move || {
            let storage = s3_provider(&env, &env.key_id, &wrong_secret, &s)?;
            Ok(Arc::new(storage) as Arc<dyn StorageProvider>)
        })
        .await;
        (bunny, s3)
    });
    say!("RESULT s9_wrong_credentials (Test Connection, one character of the secret changed)");
    let bunny_ok = report_tested("Bunny, wrong password", &bunny);
    let s3_ok = report_tested("R2, wrong secret", &s3);
    say!(
        "  target S9 (Auth within 2 s, one request): Bunny {}, R2 {}",
        verdict(bunny_ok),
        verdict(s3_ok)
    );
    assert!(
        bunny_ok && s3_ok,
        "a wrong secret was not told within 2 s and one request"
    );
}

// ---------------------------------------------------------------------------------------------
// 6. Settings: 8 transfers at once, and a 1 MB/s limit
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "uploads small test files to the Bunny test zone under limits"]
fn settings_limits() {
    let h = Harness::new();
    let defaults = TransferSettings::default();
    let mut site = h.bunny(&defaults);
    let cleanup_site = site.clone();
    let prefix = fresh_prefix("limits");
    let root = RemotePath::root();
    let small = h.temp.path().join("small");
    let big = h.temp.path().join("bandwidth");
    make_files(&small, 200, |i| {
        format!("small file {i:03}\n").repeat(128).into_bytes()
    });
    make_files(&big, 10, |i| noise(i as u64, 1 << 20));
    say!(
        "Settings: 200 small files with Simultaneous transfers 8, then 10 MiB with a 1 MB/s limit, to Bunny under {}/",
        prefix.as_str()
    );
    guarded(
        || {
            h.rt.block_on(async {
                let engine = h.engine(&defaults);

                let eight = TransferSettings {
                    simultaneous: 8,
                    ..TransferSettings::default()
                };
                apply_settings(&engine, &h.env, &mut site, &eight);
                let planned =
                    plan_upload_jobs(&site, std::slice::from_ref(&small), &prefix, &eight).await;
                let (files, jobs) = (planned.files, planned.jobs.len());
                let started = Instant::now();
                submit(&engine, &site, planned.jobs).await;
                let run = run_until_idle(
                    &engine,
                    started,
                    Duration::from_secs(600),
                    Duration::from_millis(5),
                    "8 at once",
                )
                .await;
                let limit = engine.site_transfer_limit(&site.id);
                let eight_ok = run.max_active > 0
                    && run.max_active <= 8
                    && run.stalled.is_none()
                    && run.snapshot.files_failed == 0;
                say!("RESULT settings_limits");
                say!(
                    "  simultaneous 8: {files} files ({jobs} jobs) in {:.1} s, {:.1} files/s, {} failed",
                    secs(run.elapsed),
                    per_s(run.snapshot.files_done as f64, run.elapsed),
                    run.snapshot.files_failed
                );
                say!(
                    "  most jobs active in any snapshot (taken every 5 ms): {}; the site's transfer limit: {limit:?} -> target <= 8: {}",
                    run.max_active,
                    verdict(eight_ok)
                );
                report_failures(&run.snapshot);

                let limited = TransferSettings {
                    bandwidth_limit: Some(ONE_MB),
                    ..TransferSettings::default()
                };
                apply_settings(&engine, &h.env, &mut site, &limited);
                let planned =
                    plan_upload_jobs(&site, std::slice::from_ref(&big), &prefix, &limited).await;
                let bytes = planned.bytes;
                let started = Instant::now();
                submit(&engine, &site, planned.jobs).await;
                let run = run_until_idle(
                    &engine,
                    started,
                    Duration::from_secs(600),
                    Duration::from_millis(100),
                    "1 MB/s",
                )
                .await;
                let rate = per_s(mib(bytes), run.elapsed);
                let rate_ok = (0.85..=1.10).contains(&rate)
                    && run.stalled.is_none()
                    && run.snapshot.files_failed == 0;
                say!(
                    "  bandwidth 1 MB/s (1,048,576 bytes/s): {bytes} bytes ({:.2} MiB) in {:.2} s = {rate:.3} MiB/s ({:.3} MB/s decimal), {} failed -> target ~1: {}",
                    mib(bytes),
                    secs(run.elapsed),
                    per_s(bytes as f64 / 1e6, run.elapsed),
                    run.snapshot.files_failed,
                    verdict(rate_ok)
                );
                report_failures(&run.snapshot);

                apply_settings(&engine, &h.env, &mut site, &defaults);
                let entry = find_in(&site, &root, &prefix)
                    .await
                    .unwrap_or_else(|| panic!("{}/ is not in the zone root", prefix.as_str()));
                let removal =
                    delete_entries(&engine, &site, &[entry], Duration::from_secs(600), "cleanup")
                        .await;
                let gone = gone_from(&site, &root, &prefix).await;
                say!(
                    "  cleanup: {} files deleted, {} failed; {}/ gone: {gone}",
                    removal.run.snapshot.files_done,
                    removal.run.snapshot.files_failed,
                    prefix.as_str()
                );
                assert!(eight_ok, "more than 8 transfers ran at once, or the upload failed");
                assert!(rate_ok, "the 1 MB/s limit was not kept");
                assert!(gone, "the folder is not gone");
            });
        },
        || {
            h.rt.block_on(remove_prefix(&h, &cleanup_site, &prefix, None));
        },
    );
}

// ---------------------------------------------------------------------------------------------
// 7. New Folder on Bunny and on R2
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "makes and deletes a folder on the Bunny test zone and the R2 test bucket"]
fn new_folder_both() {
    let h = Harness::new();
    let settings = TransferSettings::default();
    let bunny = h.bunny(&settings);
    let r2 = h.r2(&settings);
    let raw = S3Raw::new(&h.env);
    let prefix = fresh_prefix("folder");
    let folder = prefix
        .join(NEW_FOLDER_NAME)
        .unwrap_or_else(|_| panic!("bad folder name"));
    say!(
        "New Folder: {}/ on Bunny and on R2, then deleted",
        folder.as_str()
    );
    guarded(
        || {
            h.rt.block_on(async {
                say!("RESULT new_folder_both");
                let mut all_ok = true;
                for (label, site, raw) in [("Bunny", &bunny, None), ("R2", &r2, Some(&raw))] {
                    // `Transfers::create_remote_folder`: the browsing retries.
                    let started = Instant::now();
                    let made = with_retry(&RetryPolicy::interactive(), |_| {
                        site.provider.create_folder(&folder)
                    })
                    .await;
                    let took = started.elapsed();
                    let row = find_in(site, &prefix, &folder).await;
                    let listed = row.as_ref().is_some_and(|entry| entry.is_folder);
                    let inside = list_interactive(&*site.provider, &folder).await;
                    let inside_text = match &inside {
                        Ok(listing) => format!("Ok, {} entries", listing.entries.len()),
                        Err(e) => kind(e),
                    };
                    // Deleting it: its row, through the planner and the engine.
                    let removed = match row {
                        Some(entry) => {
                            let engine = h.engine(&settings);
                            let removal = delete_entries(
                                &engine,
                                site,
                                &[entry],
                                Duration::from_secs(300),
                                label,
                            )
                            .await;
                            removal.run.stalled.is_none() && removal.run.snapshot.files_failed == 0
                        }
                        None => false,
                    };
                    let folder_gone = gone_from(site, &prefix, &folder).await;
                    let prefix_clean = remove_prefix(&h, site, &prefix, raw).await;
                    let ok = made.is_ok()
                        && listed
                        && inside.as_ref().is_ok_and(|listing| listing.entries.is_empty())
                        && removed
                        && folder_gone
                        && prefix_clean;
                    all_ok &= ok;
                    say!(
                        "  {label}: create_folder {} in {} ms; listed in its parent as a folder: {listed}; its own listing: {inside_text}; deleted through the engine: {removed}; gone from its parent: {folder_gone}; parent folder removed and clean: {prefix_clean} -> {}",
                        made.as_ref().map_or_else(kind, |_| "Ok".to_owned()),
                        took.as_millis(),
                        verdict(ok)
                    );
                }
                assert!(all_ok, "New Folder did not work on both services");
            });
        },
        || {
            h.rt.block_on(async {
                remove_prefix(&h, &bunny, &prefix, None).await;
                remove_prefix(&h, &r2, &prefix, Some(&raw)).await;
            });
        },
    );
}

// ---------------------------------------------------------------------------------------------
// Diagnostics: where an upload's time goes
// ---------------------------------------------------------------------------------------------

/// One upload call a [`Probe`] saw: the remote path, when it was answered, how long it took,
/// and its error.
type UploadCall = (String, Instant, Duration, Option<String>);

/// A pass-through client that times every upload call (one attempt of a job: the engine retries
/// by calling again) and notes its path and how it ended, and counts the listings (during a
/// resume, the engine's checks of ruling Q55). Nothing else changes.
struct Probe {
    inner: Arc<dyn StorageProvider>,
    uploads: Mutex<Vec<UploadCall>>,
    lists: AtomicU64,
}

impl Probe {
    fn new(inner: Arc<dyn StorageProvider>) -> Self {
        Self {
            inner,
            uploads: Mutex::new(Vec::new()),
            lists: AtomicU64::new(0),
        }
    }

    /// The outcome of every upload call for `path` ("Ok" or the error's variant), in order.
    fn calls_for(&self, path: &str) -> Vec<String> {
        self.uploads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(called, _, _, _)| called == path)
            .map(|(_, _, _, error)| error.clone().unwrap_or_else(|| "Ok".to_owned()))
            .collect()
    }

    /// When each upload call for `path` that succeeded was answered, in order.
    fn answered_for(&self, path: &str) -> Vec<Instant> {
        self.uploads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(called, _, _, error)| called == path && error.is_none())
            .map(|(_, answered, _, _)| *answered)
            .collect()
    }

    fn summary(&self) -> String {
        let uploads = self.uploads.lock().unwrap_or_else(PoisonError::into_inner);
        let mut times: Vec<Duration> = uploads.iter().map(|(_, _, took, _)| *took).collect();
        times.sort();
        let at = |share: f64| {
            let last = times.len().saturating_sub(1) as f64;
            times
                .get((last * share).round() as usize)
                .map_or(0, Duration::as_millis)
        };
        let mut errors: Vec<(String, usize)> = Vec::new();
        for (_, _, _, error) in uploads.iter() {
            if let Some(error) = error {
                match errors.iter_mut().find(|(seen, _)| seen == error) {
                    Some((_, count)) => *count += 1,
                    None => errors.push((error.clone(), 1)),
                }
            }
        }
        format!(
            "{} upload calls; each took p50 {} ms, p90 {} ms, p99 {} ms, max {} ms; errors {errors:?}",
            times.len(),
            at(0.5),
            at(0.9),
            at(0.99),
            at(1.0)
        )
    }
}

#[async_trait]
impl StorageProvider for Probe {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    fn endpoint(&self) -> String {
        self.inner.endpoint()
    }

    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list(dir).await
    }

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        let started = Instant::now();
        let result = self.inner.upload(path, source, opts).await;
        let outcome = result.as_ref().err().map(kind);
        self.uploads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                path.as_str().to_owned(),
                Instant::now(),
                started.elapsed(),
                outcome,
            ));
        result
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        self.inner.download(path).await
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        self.inner.delete_file(path).await
    }

    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.inner.delete_folder(dir).await
    }

    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.inner.create_folder(dir).await
    }

    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        self.inner.delete_batch(paths).await
    }

    async fn reachable(&self) -> Result<(), ProviderError> {
        self.inner.reachable().await
    }

    async fn abort_multipart(
        &self,
        path: &RemotePath,
        upload_id: &str,
    ) -> Result<(), ProviderError> {
        self.inner.abort_multipart(path, upload_id).await
    }
}

/// The same upload ([`S5_SOURCE`], 3,640 files) to Bunny with the default "HTTP/2 when
/// available" and with the "HTTP/1.1 only" setting, every upload call timed: whether the protocol
/// explains the files/s.
#[test]
#[ignore = "uploads the same folder to the Bunny test zone over HTTP/2 and HTTP/1.1"]
fn bunny_protocol_comparison() {
    let h = Harness::new();
    let source = Path::new(S5_SOURCE);
    let local = local_files(source);
    say!(
        "Protocols: {} files ({:.1} MiB) of {} to Bunny, HTTP/2 when available, then HTTP/1.1 only",
        local.len(),
        mib(total_size(&local)),
        source.display()
    );
    for (tag, protocol) in [
        ("h2", HttpProtocol::Http2WhenAvailable),
        ("h1", HttpProtocol::Http1Only),
    ] {
        let settings = TransferSettings {
            protocol,
            ..TransferSettings::default()
        };
        let mut site = h.bunny(&settings);
        let plain = site.clone();
        let probe = Arc::new(Probe::new(site.provider.clone()));
        site.provider = probe.clone();
        let prefix = fresh_prefix(tag);
        guarded(
            || {
                h.rt.block_on(async {
                    let version = list_interactive(&*plain.provider, &RemotePath::root())
                        .await
                        .map_or_else(|e| kind(&e), |listing| listing.info.http_version);
                    let upload = upload_measured(
                        &h,
                        &site,
                        &[source.to_path_buf()],
                        &prefix,
                        &settings,
                        tag,
                    )
                    .await;
                    upload.report(&format!("bunny_protocol_comparison {protocol:?}"));
                    say!("  a listing went over {version}");
                    say!("  {}", probe.summary());
                    assert!(upload.run.stalled.is_none(), "the upload stalled");
                    assert_eq!(upload.run.snapshot.files_failed, 0, "uploads failed");
                });
                assert!(
                    h.rt.block_on(remove_prefix(&h, &plain, &prefix, None)),
                    "the folder was not removed"
                );
            },
            || {
                h.rt.block_on(remove_prefix(&h, &plain, &prefix, None));
            },
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Leftovers
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "deletes every root accept-* item from the Bunny test zone and the R2 test bucket"]
fn sweep_accept_prefixes() {
    let h = Harness::new();
    let settings = TransferSettings::default();
    let bunny = h.bunny(&settings);
    let r2 = h.r2(&settings);
    let raw = S3Raw::new(&h.env);
    let root = RemotePath::root();
    let clean = h.rt.block_on(async {
        let mut clean = true;
        for (label, site, raw) in [("Bunny", &bunny, None), ("R2", &r2, Some(&raw))] {
            let listing = list_interactive(&*site.provider, &root)
                .await
                .unwrap_or_else(|e| panic!("listing the {label} root failed: {}", kind(&e)));
            let leftovers: Vec<Entry> = listing
                .entries
                .iter()
                .filter(|entry| entry.name().starts_with(ACCEPT))
                .cloned()
                .collect();
            say!(
                "  {label} ({}): {} root entries, {} named accept-*",
                site.name,
                listing.entries.len(),
                leftovers.len()
            );
            for entry in leftovers {
                say!("  {label}: sweeping {}", entry.path.as_str());
                let engine = h.engine(&settings);
                let removal =
                    delete_entries(&engine, site, &[entry], Duration::from_secs(3600), "sweep")
                        .await;
                say!(
                    "    {} deleted, {} failed in {:.1} s",
                    removal.run.snapshot.files_done,
                    removal.run.snapshot.files_failed,
                    secs(removal.run.elapsed)
                );
            }
            let mut left = usize::MAX;
            for attempt in 0..6 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                if let Ok(listing) = list_interactive(&*site.provider, &root).await {
                    left = listing
                        .entries
                        .iter()
                        .filter(|entry| entry.name().starts_with(ACCEPT))
                        .count();
                    if left == 0 {
                        break;
                    }
                }
            }
            let keys = match raw {
                Some(raw) => raw.keys_under(ACCEPT).await,
                None => Some(0),
            };
            say!("  {label}: root entries named accept-* now: {left}; keys starting with accept-: {keys:?}");
            clean &= left == 0 && keys == Some(0);
        }
        clean
    });
    say!("RESULT sweep_accept_prefixes: zone and bucket clean of accept-*: {clean}");
    assert!(clean, "accept-* items are left");
}
