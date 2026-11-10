// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live smoke of the manage tools (I4 plan T10) against the user's R2 test bucket and Bunny test
//! zone. Both tests are `#[ignore]`d and run only when asked for:
//!
//! ```text
//! cargo test --locked --release -p httporter-transfer --test manage_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **R2.** 3 text files and a 70 MB file (uploaded in parts) are published to
//! `manage-smoke-<unix time>/`; `stat` of the big one carries its SHA-256; a second compare says
//! Same for it; a text file is renamed (CopyObject, then a delete); a 60 s pre-signed GET of it
//! answers 200 and the same link with a tampered signature 403; a search for `*.txt` finds 3.
//!
//! **Bunny.** 20 files are published to `manage-smoke-<unix time>/src/`, the folder is moved
//! (renamed to `dst`: download, upload, then the originals deleted once the late-delete checks
//! have settled); `stat` of a moved file has its creation date; a search for `*.txt` finds 20.
//!
//! **Credentials.** `HTTPORTER_TEST_*` from the git-ignored `.env.test` above this crate, parsed
//! by this process alone and never printed: everything printed goes through `say!`, which takes
//! the secrets out, errors are printed by variant, and no URL is printed (the R2 host holds the
//! account ID, a pre-signed link a signature).
//!
//! **Guards.** The zone and the bucket must start with `httporter-test`; the production zone
//! `exampleshop` and its pull zone 7654321 are refused by name before the first request.
//!
//! **Cleanup.** Every root `manage-smoke-*` item is deleted at the end of each test, also when
//! a step fails, and nothing else.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use httporter_bunny::{BunnyConfig, BunnyStorage, storage_host};
use httporter_core::{
    CompareStatus, Entry, META_SHA256, MemoryJournal, ProviderError, ProviderKind, PurgeMode,
    RemotePath, S3Preset, SiteId, SkipRules, StorageProvider, TransferSettings, walk,
};
use httporter_s3::{Credentials, S3Config, S3Storage, Url, endpoint, part_size_for};
use httporter_transfer::{
    BatchTarget, CompareMode, CompareResult, Engine, HashCache, PublishPlan, SearchQuery,
    compare_trees, hash_file, plan_move, run_move, run_publish, search,
};

/// The only zones and buckets touched.
const TEST_PREFIX: &str = "httporter-test";
/// Never touched, whatever the prefix check says.
const PRODUCTION_ZONE: &str = "exampleshop";
const PRODUCTION_PULL_ZONE: &str = "7654321";
/// Every item the smoke makes starts with this, and it deletes nothing else.
const SMOKE: &str = "manage-smoke-";
/// The big file: over the 64 MiB multipart threshold.
const BIG: u64 = 70 * 1024 * 1024;
const BUNNY_FILES: usize = 20;

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

/// Every secret read; [`scrub`] takes them out of whatever is printed.
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

/// An error as its variant (and status), never its text: a network error can quote a URL.
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

fn describe(error: &ProviderError) -> String {
    kind(error)
}

// ---------------------------------------------------------------------------------------------
// Credentials and guards
// ---------------------------------------------------------------------------------------------

/// `HTTPORTER_TEST_*` of the nearest `.env.test` above this crate, parsed here and never loaded
/// into the environment; its errors never quote a line (which may hold a secret). Every value
/// but the zone, region, bucket, preset and S3 region is noted as a secret.
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
                let value = value.trim().to_owned();
                let public = [
                    "HTTPORTER_TEST_ZONE",
                    "HTTPORTER_TEST_REGION",
                    "HTTPORTER_TEST_S3_BUCKET",
                    "HTTPORTER_TEST_S3_PRESET",
                    "HTTPORTER_TEST_S3_REGION",
                ];
                if !public.contains(&key.as_str()) {
                    keep_secret(&value);
                }
                values.insert(key, value);
            }
            Ok(_) => {}
            Err(_) => panic!("an entry of .env.test could not be read"),
        }
    }
    values
}

fn required(env: &HashMap<String, String>, name: &str) -> String {
    match env.get(name) {
        Some(value) if !value.is_empty() => value.clone(),
        _ => panic!("{name} is not set in .env.test"),
    }
}

/// Panics unless `name` is a test zone or bucket. Called before any request to it.
fn assert_test_name(what: &str, name: &str) {
    assert!(
        name.starts_with(TEST_PREFIX),
        "refusing to touch {what} {name:?}: only names starting with {TEST_PREFIX:?}"
    );
    assert_ne!(
        name, PRODUCTION_ZONE,
        "refusing to touch the production zone"
    );
    assert!(
        !name.contains(PRODUCTION_PULL_ZONE),
        "refusing to touch the production pull zone"
    );
}

/// Panics unless `path` is inside a `manage-smoke-*` item.
fn assert_smoke_path(path: &RemotePath) {
    assert!(
        path.segments()
            .next()
            .is_some_and(|top| top.starts_with(SMOKE)),
        "refusing to delete {} (outside {SMOKE}*)",
        path.as_str()
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

/// The test bucket's client, as the app makes it, and the target its batches go to.
fn r2(env: &HashMap<String, String>) -> (Arc<dyn StorageProvider>, BatchTarget) {
    let bucket = required(env, "HTTPORTER_TEST_S3_BUCKET");
    assert_test_name("bucket", &bucket);
    let preset = s3_preset(&required(env, "HTTPORTER_TEST_S3_PRESET"));
    let account = required(env, "HTTPORTER_TEST_S3_ACCOUNT_OR_ENDPOINT");
    let region = env
        .get("HTTPORTER_TEST_S3_REGION")
        .cloned()
        .unwrap_or_default();
    let found = endpoint(preset, &region, &account, &account, true)
        .unwrap_or_else(|_| panic!("the test bucket's address could not be made"));
    let creds = Credentials {
        access_key_id: required(env, "HTTPORTER_TEST_S3_KEY_ID"),
        secret_access_key: required(env, "HTTPORTER_TEST_S3_SECRET"),
        session_token: None,
    };
    let mut cfg = S3Config::for_site(bucket.as_str(), found, creds, &TransferSettings::default());
    cfg.part_size = part_size_for(preset);
    let provider: Arc<dyn StorageProvider> = Arc::new(
        S3Storage::new(cfg)
            .unwrap_or_else(|e| panic!("the S3 client could not be built: {}", kind(&e))),
    );
    let target = BatchTarget {
        site: SiteId::new(),
        site_name: bucket,
        // The preset's name, not the address: the address holds the account ID.
        host: preset.label().to_owned(),
        provider: provider.clone(),
    };
    (provider, target)
}

/// The test zone's client, as the app makes it, and the target its batches go to.
fn bunny(env: &HashMap<String, String>) -> (Arc<dyn StorageProvider>, BatchTarget) {
    let zone = required(env, "HTTPORTER_TEST_ZONE");
    assert_test_name("zone", &zone);
    let region = required(env, "HTTPORTER_TEST_REGION").to_ascii_lowercase();
    let host = storage_host(&region)
        .unwrap_or_else(|| panic!("HTTPORTER_TEST_REGION is not a Bunny region"));
    let cfg = BunnyConfig::for_site(
        zone.as_str(),
        host.as_str(),
        required(env, "HTTPORTER_TEST_PASSWORD").as_str(),
        &TransferSettings::default(),
    );
    let provider: Arc<dyn StorageProvider> = Arc::new(
        BunnyStorage::new(cfg)
            .unwrap_or_else(|e| panic!("the Bunny client could not be built: {}", kind(&e))),
    );
    let target = BatchTarget {
        site: SiteId::new(),
        site_name: zone,
        host,
        provider: provider.clone(),
    };
    (provider, target)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn smoke_root() -> RemotePath {
    let root = RemotePath::parse(&format!("{SMOKE}{}", unix_now())).unwrap();
    assert_smoke_path(&root);
    root
}

fn child(dir: &RemotePath, name: &str) -> RemotePath {
    dir.join(name).unwrap()
}

fn engine() -> Engine {
    Engine::new(
        tokio::runtime::Handle::current(),
        Arc::new(MemoryJournal::new()),
        TransferSettings::default(),
        describe,
    )
}

/// A file of `len` bytes that no service could compress or dedupe away.
fn write_big(path: &Path, len: u64) {
    let mut file = fs::File::create(path).unwrap();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut block = vec![0u8; 1024 * 1024];
    let mut left = len;
    while left > 0 {
        for chunk in block.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
        }
        let n = left.min(block.len() as u64) as usize;
        file.write_all(&block[..n]).unwrap();
        left -= n as u64;
    }
}

async fn compare(
    provider: &dyn StorageProvider,
    kind: ProviderKind,
    local: &Path,
    remote: &RemotePath,
) -> CompareResult {
    compare_trees(
        provider,
        local,
        remote,
        &SkipRules::new(&SkipRules::defaults()),
        CompareMode::for_site(kind, true),
        &HashCache::default(),
        &|_| {},
    )
    .await
    .unwrap_or_else(|e| panic!("the compare failed: {e:?}"))
}

/// Compares `local` with `remote` and publishes everything not Same; returns the uploads done.
async fn publish(
    engine: &Engine,
    target: &BatchTarget,
    kind: ProviderKind,
    local: &Path,
    remote: &RemotePath,
) -> u64 {
    let result = compare(&*target.provider, kind, local, remote).await;
    let plan = PublishPlan::from_compare(&result, false);
    let outcome = run_publish(engine, target.clone(), &plan, PurgeMode::Off, None, &|_| {})
        .await
        .unwrap_or_else(|_| panic!("the journal refused the publish"));
    say!(
        "  published: uploaded {}, failed {}, {} bytes",
        outcome.uploaded,
        outcome.failed,
        outcome.bytes
    );
    assert_eq!(outcome.failed, 0);
    outcome.uploaded
}

/// The entry of `path` in its parent's listing.
async fn entry_of(provider: &dyn StorageProvider, path: &RemotePath) -> Entry {
    let parent = path.parent().unwrap();
    provider
        .list(&parent)
        .await
        .unwrap_or_else(|e| panic!("listing {} failed: {}", parent.as_str(), kind(&e)))
        .entries
        .into_iter()
        .find(|entry| entry.path == *path)
        .unwrap_or_else(|| panic!("{} is not listed", path.as_str()))
}

async fn search_names(
    provider: &dyn StorageProvider,
    root: &RemotePath,
    text: &str,
) -> Vec<String> {
    let found = Mutex::new(Vec::new());
    let query = SearchQuery {
        text: text.to_owned(),
        ..SearchQuery::default()
    };
    search(provider, root, &query, &|entries, _| {
        found.lock().unwrap_or_else(PoisonError::into_inner).extend(
            entries
                .into_iter()
                .map(|entry| entry.path.as_str().to_owned()),
        );
    })
    .await
    .unwrap_or_else(|e| panic!("the search failed: {}", kind(&e)));
    let mut names = found.into_inner().unwrap_or_else(PoisonError::into_inner);
    names.sort();
    names
}

/// Deletes every root `manage-smoke-*` item; says whether all went.
async fn clean_up(provider: &dyn StorageProvider) -> bool {
    let listing = match provider.list(&RemotePath::root()).await {
        Ok(listing) => listing,
        Err(e) => {
            say!("  listing the root for the cleanup failed: {}", kind(&e));
            return false;
        }
    };
    let mut clean = true;
    for entry in listing
        .entries
        .iter()
        .filter(|entry| entry.name().starts_with(SMOKE))
    {
        assert_smoke_path(&entry.path);
        let result = if entry.is_folder {
            provider.delete_folder(&entry.path).await
        } else {
            provider.delete_file(&entry.path).await
        };
        match result {
            Ok(()) | Err(ProviderError::NotFound) => say!("  deleted {}", entry.path.as_str()),
            Err(e) => {
                clean = false;
                say!("  deleting {} failed: {}", entry.path.as_str(), kind(&e));
            }
        }
    }
    clean
}

/// Runs the cleanup whatever `steps` did, then passes on its panic or the cleanup's failure.
async fn with_cleanup(provider: &dyn StorageProvider, steps: impl Future<Output = ()>) {
    let result = AssertUnwindSafe(steps).catch_unwind().await;
    let clean = clean_up(provider).await;
    say!("cleanup: {}", if clean { "done" } else { "INCOMPLETE" });
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(clean, "some manage-smoke-* items are left");
}

// ---------------------------------------------------------------------------------------------
// R2
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs .env.test; writes manage-smoke-* on the R2 test bucket"]
async fn manage_smoke_on_the_r2_test_bucket() {
    // reqwest brings no TLS crypto (`rustls-no-provider`): use ring's, as the providers do.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let env = test_env();
    let (provider, target) = r2(&env);
    let local = tempfile::tempdir().unwrap();
    let remote = smoke_root();
    say!(
        "R2 bucket {}, folder {}/",
        target.site_name,
        remote.as_str()
    );

    let steps = async {
        let engine = engine();

        // 1. 3 text files and a 70 MB one.
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(local.path().join(name), format!("manage smoke: {name}\n")).unwrap();
        }
        let big_local = local.path().join("big.bin");
        write_big(&big_local, BIG);
        let uploaded = publish(&engine, &target, ProviderKind::S3, local.path(), &remote).await;
        assert_eq!(uploaded, 4);

        // 2. stat has the SHA-256 meta.
        let big = child(&remote, "big.bin");
        let info = provider
            .stat(&big)
            .await
            .unwrap_or_else(|e| panic!("stat failed: {}", kind(&e)));
        let expected = hex_of(&hash_file(&big_local).unwrap().sha256);
        let recorded = info
            .metadata
            .iter()
            .find(|(name, _)| name == META_SHA256)
            .map(|(_, value)| value.clone());
        say!(
            "  stat big.bin: {} bytes, sha256 meta set: {}",
            info.entry.size,
            recorded.is_some()
        );
        assert_eq!(info.entry.size, BIG);
        assert_eq!(recorded.as_deref(), Some(expected.as_str()));

        // 3. A second compare says Same for the multipart file.
        let again = compare(&*provider, ProviderKind::S3, local.path(), &remote).await;
        let status = again
            .items
            .iter()
            .find(|item| item.rel == "big.bin")
            .map(|item| item.status);
        say!("  2nd compare: {:?}, big.bin {status:?}", again.counts());
        assert_eq!(status, Some(CompareStatus::Same));
        assert_eq!(again.counts().same, 4);

        // 4. Rename a.txt through CopyObject.
        let a = child(&remote, "a.txt");
        let renamed = child(&remote, "a-renamed.txt");
        let source = entry_of(&*provider, &a).await;
        let plan = plan_move(&*provider, &[source], &remote, Some("a-renamed.txt"))
            .await
            .unwrap_or_else(|e| panic!("planning the rename failed: {}", kind(&e)));
        assert!(plan.server_side, "an S3 rename copies on the server");
        let temp = tempfile::tempdir().unwrap();
        let moved = run_move(&engine, target.clone(), &plan, temp.path(), &|_| {})
            .await
            .unwrap_or_else(|_| panic!("the journal refused the rename"));
        say!(
            "  rename: copied {}, deleted {}, failed {}",
            moved.copied,
            moved.deleted,
            moved.failed.len()
        );
        assert_eq!((moved.copied, moved.deleted), (1, 1));
        assert!(moved.failed.is_empty());
        assert!(provider.stat(&renamed).await.is_ok());
        assert_eq!(
            provider.stat(&a).await.unwrap_err(),
            ProviderError::NotFound
        );

        // 5. A pre-signed GET (60 s) answers 200; a tampered signature 403.
        let link = provider
            .presign(&renamed, Duration::from_secs(60))
            .unwrap_or_else(|e| panic!("presign failed: {}", kind(&e)));
        keep_secret(&link);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| panic!("the HTTP client could not be built"));
        let answer = http
            .get(&link)
            .send()
            .await
            .unwrap_or_else(|_| panic!("the pre-signed GET got no answer"));
        let status = answer.status().as_u16();
        let body = answer.text().await.unwrap_or_default();
        say!("  pre-signed GET: {status}");
        assert_eq!(status, 200);
        assert_eq!(body, "manage smoke: a.txt\n");
        let tampered = tamper(&link);
        keep_secret(&tampered);
        let status = http
            .get(&tampered)
            .send()
            .await
            .unwrap_or_else(|_| panic!("the tampered GET got no answer"))
            .status()
            .as_u16();
        say!("  tampered GET: {status}");
        assert_eq!(status, 403);

        // 6. Search *.txt finds 3.
        let found = search_names(&*provider, &remote, "*.txt").await;
        say!("  search *.txt: {} found", found.len());
        assert_eq!(found.len(), 3, "{found:?}");
    };
    with_cleanup(&*provider, steps).await;
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The link with the last digit of its signature changed.
fn tamper(link: &str) -> String {
    let url = Url::parse(link).unwrap();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| {
            let value = if name == "X-Amz-Signature" {
                let mut value = value.into_owned();
                let last = value.pop().unwrap();
                value.push(if last == '0' { '1' } else { '0' });
                value
            } else {
                value.into_owned()
            };
            (name.into_owned(), value)
        })
        .collect();
    let mut out = url.clone();
    out.query_pairs_mut().clear().extend_pairs(pairs);
    out.to_string()
}

// ---------------------------------------------------------------------------------------------
// Bunny
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs .env.test; writes manage-smoke-* on the Bunny test zone"]
async fn manage_smoke_on_the_bunny_test_zone() {
    let env = test_env();
    let (provider, target) = bunny(&env);
    let local = tempfile::tempdir().unwrap();
    let remote = smoke_root();
    say!(
        "Bunny zone {}, folder {}/",
        target.site_name,
        remote.as_str()
    );

    let steps = async {
        let engine = engine();

        // A 20-file folder.
        let src = child(&remote, "src");
        for n in 1..=BUNNY_FILES {
            fs::write(
                local.path().join(format!("f{n:02}.txt")),
                format!("manage smoke: file {n}\n"),
            )
            .unwrap();
        }
        let uploaded = publish(&engine, &target, ProviderKind::Bunny, local.path(), &src).await;
        assert_eq!(uploaded, BUNNY_FILES as u64);

        // Move it (rename to dst): download, upload, then the originals deleted after the
        // late-delete checks have settled.
        let source = entry_of(&*provider, &src).await;
        assert!(source.is_folder);
        let plan = plan_move(&*provider, &[source], &remote, Some("dst"))
            .await
            .unwrap_or_else(|e| panic!("planning the move failed: {}", kind(&e)));
        assert!(!plan.server_side, "Bunny can't copy on the server");
        assert_eq!(plan.items.len(), BUNNY_FILES);
        let temp = tempfile::tempdir().unwrap();
        let moved = run_move(&engine, target.clone(), &plan, temp.path(), &|_| {})
            .await
            .unwrap_or_else(|_| panic!("the journal refused the move"));
        say!(
            "  move: copied {}, deleted {}, failed {}",
            moved.copied,
            moved.deleted,
            moved.failed.len()
        );
        assert_eq!(moved.copied, BUNNY_FILES as u64);
        assert_eq!(moved.deleted, BUNNY_FILES as u64);
        assert!(moved.failed.is_empty());
        assert!(moved.kept_copies.is_empty());
        let left = match walk(&*provider, &src).await {
            Ok(entries) => entries.iter().filter(|entry| !entry.is_folder).count(),
            Err(ProviderError::NotFound) => 0,
            Err(e) => panic!("listing the old folder failed: {}", kind(&e)),
        };
        say!("  files left in src: {left}");
        assert_eq!(left, 0);

        // stat has the creation date.
        let dst = child(&remote, "dst");
        let info = provider
            .stat(&child(&dst, "f01.txt"))
            .await
            .unwrap_or_else(|e| panic!("stat failed: {}", kind(&e)));
        say!("  stat dst/f01.txt: created {:?}", info.created);
        assert!(info.created.is_some());

        // Search finds the 20.
        let found = search_names(&*provider, &remote, "*.txt").await;
        say!("  search *.txt: {} found", found.len());
        assert_eq!(found.len(), BUNNY_FILES, "{found:?}");
        assert!(found.iter().all(|path| path.contains("/dst/")));
    };
    with_cleanup(&*provider, steps).await;
}
