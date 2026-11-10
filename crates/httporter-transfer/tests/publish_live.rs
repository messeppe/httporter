// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live smoke of publishing (I3 plan T10) against the user's Bunny test zone. It is `#[ignore]`d
//! and runs only when asked for:
//!
//! ```text
//! cargo test --locked -p httporter-transfer --test publish_live -- --ignored --nocapture
//! ```
//!
//! **Steps.** 50 local files (one of them `sub/index.html`) are compared with
//! `publish-smoke-<unix time>/` on the zone and published there; 3 are changed, compared again
//! (exactly 3 Changed) and published again (3 done). The purge URLs of that second publish are
//! printed. They are sent only when `HTTPORTER_TEST_ACCOUNT_KEY` and `HTTPORTER_TEST_PULL_ZONE`
//! (a pull zone id, never 7654321) are both set and the account lists that pull zone in front
//! of the test zone; else the smoke only prints them.
//!
//! **Credentials.** `HTTPORTER_TEST_*` from the git-ignored `.env.test` above this crate, parsed
//! by this process alone (never loaded into the environment) and never printed: everything
//! printed goes through `say!`, which takes the secrets out, and errors are printed by variant.
//!
//! **Guards.** The zone must start with `httporter-test`; the production zone `exampleshop` and
//! its pull zone 7654321 are refused by name and id before the first request.
//!
//! **Cleanup.** Every root `publish-smoke-*` folder is deleted at the end, also when a step
//! fails, and nothing else.

use std::collections::HashMap;
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use httporter_bunny::{BunnyAccount, BunnyCdn, BunnyConfig, BunnyStorage, storage_host};
use httporter_core::{
    CdnPurger, CdnZone, CompareStatus, MemoryJournal, ProviderError, ProviderKind, PurgeMode,
    PurgeResult, RemotePath, SiteId, SkipRules, StorageProvider, TransferSettings, purge_urls,
};
use httporter_transfer::{
    BatchTarget, CompareMode, CompareResult, Engine, HashCache, PublishPlan, compare_trees,
    run_publish,
};

/// The only zones touched.
const TEST_PREFIX: &str = "httporter-test";
/// Never touched, whatever the prefix check says.
const PRODUCTION_ZONE: &str = "exampleshop";
const PRODUCTION_PULL_ZONE: u64 = 4_495_693;
/// Every folder the smoke makes starts with this, and it deletes nothing else.
const SMOKE: &str = "publish-smoke-";
/// The test pull zone's host when `.env.test` names none.
const DEFAULT_PULL_ZONE_HOST: &str = "httporter-test-sg.b-cdn.net";
const FILES: usize = 50;

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

/// An error as its variant (and status), never its text.
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

// ---------------------------------------------------------------------------------------------
// Credentials and guards
// ---------------------------------------------------------------------------------------------

/// The `HTTPORTER_TEST_*` values. No `Debug`: it holds the secrets.
struct TestEnv {
    zone: String,
    region: String,
    password: String,
    account_key: Option<String>,
    pull_zone: Option<u64>,
    pull_zone_host: String,
}

/// `HTTPORTER_TEST_*` of the nearest `.env.test` above this crate, parsed here and never loaded
/// into the environment; its errors never quote a line (which may hold a secret).
fn test_env() -> TestEnv {
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
                values.insert(key, value.trim().to_owned());
            }
            Ok(_) => {}
            Err(_) => panic!("an entry of .env.test could not be read"),
        }
    }
    let optional = |name: &str| values.get(name).filter(|value| !value.is_empty()).cloned();
    let required =
        |name: &str| optional(name).unwrap_or_else(|| panic!("{name} is not set in .env.test"));
    let env = TestEnv {
        zone: required("HTTPORTER_TEST_ZONE"),
        region: required("HTTPORTER_TEST_REGION").to_ascii_lowercase(),
        password: required("HTTPORTER_TEST_PASSWORD"),
        account_key: optional("HTTPORTER_TEST_ACCOUNT_KEY"),
        pull_zone: optional("HTTPORTER_TEST_PULL_ZONE").map(|id| {
            id.parse()
                .unwrap_or_else(|_| panic!("HTTPORTER_TEST_PULL_ZONE is not a pull zone id"))
        }),
        pull_zone_host: optional("HTTPORTER_TEST_PULL_ZONE_HOST")
            .unwrap_or_else(|| DEFAULT_PULL_ZONE_HOST.to_owned()),
    };
    keep_secret(&env.password);
    if let Some(key) = &env.account_key {
        keep_secret(key);
    }
    assert_test_zone(&env.zone);
    if let Some(id) = env.pull_zone {
        assert_test_pull_zone(id, "");
    }
    assert!(
        !env.pull_zone_host.contains(PRODUCTION_ZONE),
        "refusing the production pull zone's host"
    );
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

/// Panics for the production pull zone, by id or by name.
fn assert_test_pull_zone(id: u64, name: &str) {
    assert_ne!(
        id, PRODUCTION_PULL_ZONE,
        "refusing to touch the production pull zone"
    );
    assert_ne!(
        name, PRODUCTION_ZONE,
        "refusing to touch the production pull zone"
    );
}

/// Panics unless `path` is inside a `publish-smoke-*` folder.
fn assert_smoke_path(path: &RemotePath) {
    assert!(
        path.segments()
            .next()
            .is_some_and(|top| top.starts_with(SMOKE)),
        "refusing to delete {} (outside {SMOKE}*)",
        path.as_str()
    );
}

fn bunny(env: &TestEnv) -> (Arc<dyn StorageProvider>, String) {
    assert_test_zone(&env.zone);
    let host = storage_host(&env.region)
        .unwrap_or_else(|| panic!("HTTPORTER_TEST_REGION is not a Bunny region"));
    let cfg = BunnyConfig::for_site(
        env.zone.as_str(),
        host.as_str(),
        env.password.as_str(),
        &TransferSettings::default(),
    );
    let storage = BunnyStorage::new(cfg)
        .unwrap_or_else(|e| panic!("the Bunny client could not be built: {}", kind(&e)));
    (Arc::new(storage), host)
}

/// The purger, when the account key and a pull zone of the test zone are both given. The pull
/// zone must be one the account lists in front of the test zone.
async fn purger(env: &TestEnv) -> Option<Arc<dyn CdnPurger>> {
    let (Some(key), Some(id)) = (&env.account_key, env.pull_zone) else {
        say!("  no account key and pull zone in .env.test: the purge URLs are only printed");
        return None;
    };
    assert_test_pull_zone(id, "");
    let account = BunnyAccount::new(key.clone())
        .unwrap_or_else(|e| panic!("the account client could not be built: {}", kind(&e)));
    let zones = account
        .cdn_zones_for(&env.zone)
        .await
        .unwrap_or_else(|e| panic!("the test zone's pull zones could not be read: {}", kind(&e)));
    let zone: CdnZone = zones
        .into_iter()
        .find(|zone| zone.id == id)
        .unwrap_or_else(|| panic!("pull zone {id} is not in front of the test zone"));
    assert_test_pull_zone(zone.id, &zone.name);
    assert!(
        zone.hostnames
            .iter()
            .all(|host| !host.contains(PRODUCTION_ZONE)),
        "refusing the production pull zone's host"
    );
    Some(Arc::new(BunnyCdn::new(account, vec![zone])))
}

// ---------------------------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------------------------

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn describe(error: &ProviderError) -> String {
    kind(error)
}

/// 50 small files, `sub/index.html` among them.
fn make_files(root: &Path) -> Vec<String> {
    let mut rels = vec!["index.html".to_owned(), "sub/index.html".to_owned()];
    for n in rels.len()..FILES {
        rels.push(format!("files/file-{n:02}.txt"));
    }
    for rel in &rels {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("publish smoke: {rel}\n")).unwrap();
    }
    rels
}

async fn compare(
    provider: &dyn StorageProvider,
    local: &Path,
    remote: &RemotePath,
) -> CompareResult {
    compare_trees(
        provider,
        local,
        remote,
        &SkipRules::new(&SkipRules::defaults()),
        CompareMode::for_site(ProviderKind::Bunny, true),
        &HashCache::default(),
        &|_| {},
    )
    .await
    .unwrap_or_else(|e| panic!("the compare failed: {e:?}"))
}

/// Deletes every root `publish-smoke-*` folder of the zone; says what is left.
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs .env.test; publishes into publish-smoke-* on the Bunny test zone"]
async fn publish_smoke_on_the_bunny_test_zone() {
    let env = test_env();
    let (provider, host) = bunny(&env);
    let local = tempfile::tempdir().unwrap();
    let remote = RemotePath::parse(&format!("{SMOKE}{}", unix_now())).unwrap();
    assert_smoke_path(&remote);
    say!("zone {}, folder {}/", env.zone, remote.as_str());

    let steps = AssertUnwindSafe(async {
        let engine = Engine::new(
            tokio::runtime::Handle::current(),
            Arc::new(MemoryJournal::new()),
            TransferSettings::default(),
            describe,
        );
        let target = BatchTarget {
            site: SiteId::new(),
            site_name: env.zone.clone(),
            host: host.clone(),
            provider: provider.clone(),
        };

        // 1. 50 files.
        let rels = make_files(local.path());
        assert_eq!(rels.len(), FILES);

        // 2. Compare (all New), then publish.
        let first = compare(&*provider, local.path(), &remote).await;
        let counts = first.counts();
        say!("1st compare: {counts:?}");
        assert_eq!(counts.new, FILES as u64);
        let plan = PublishPlan::from_compare(&first, false);
        let outcome = run_publish(
            &engine,
            target.clone(),
            &plan,
            PurgeMode::Off,
            None,
            &|_| {},
        )
        .await
        .unwrap_or_else(|_| panic!("the journal refused the publish"));
        say!(
            "1st publish: uploaded {}, failed {}, {} bytes",
            outcome.uploaded,
            outcome.failed,
            outcome.bytes
        );
        assert_eq!(outcome.uploaded, FILES as u64);
        assert_eq!(outcome.failed, 0);

        // 3. Change 3 files.
        let changed = ["index.html", "sub/index.html", "files/file-10.txt"];
        for rel in changed {
            fs::write(
                local.path().join(rel),
                format!("publish smoke, changed: {rel}\n"),
            )
            .unwrap();
        }

        // 4. Compare again: exactly 3 Changed.
        let second = compare(&*provider, local.path(), &remote).await;
        let counts = second.counts();
        say!("2nd compare: {counts:?}");
        let mut found: Vec<&str> = second
            .items
            .iter()
            .filter(|item| item.status == CompareStatus::Changed)
            .map(|item| item.rel.as_str())
            .collect();
        found.sort_unstable();
        let mut expected = changed.to_vec();
        expected.sort_unstable();
        assert_eq!(found, expected);
        assert_eq!((counts.changed, counts.new, counts.remote_only), (3, 0, 0));
        assert_eq!(counts.same, (FILES - 3) as u64);

        // 5 and 6. The purge URLs of the second publish, printed; sent only with a purger.
        let plan = PublishPlan::from_compare(&second, false);
        let zone = CdnZone {
            id: env.pull_zone.unwrap_or(0),
            name: "test pull zone".into(),
            hostnames: vec![env.pull_zone_host.clone()],
        };
        let urls = purge_urls(&[zone], &plan.touched_paths());
        say!("purge URLs ({}):", urls.len());
        for url in &urls {
            say!("  {url}");
        }
        // 7. Sent only with the account key and a test pull zone.
        let purger = purger(&env).await;
        let mode = if purger.is_some() {
            PurgeMode::Changed
        } else {
            PurgeMode::Off
        };
        let outcome = run_publish(&engine, target, &plan, mode, purger.clone(), &|_| {})
            .await
            .unwrap_or_else(|_| panic!("the journal refused the publish"));
        say!(
            "2nd publish: uploaded {}, failed {}, purge {:?}",
            outcome.uploaded,
            outcome.failed,
            outcome.purge
        );
        assert_eq!(outcome.uploaded, 3);
        assert_eq!(outcome.failed, 0);
        if purger.is_some() {
            assert!(
                matches!(outcome.purge, PurgeResult::Urls(_)),
                "{:?}",
                outcome.purge
            );
        } else {
            assert_eq!(outcome.purge, PurgeResult::Off);
        }
    })
    .catch_unwind()
    .await;

    // 8. Delete publish-smoke-*.
    let clean = clean_up(&*provider).await;
    say!("cleanup: {}", if clean { "done" } else { "INCOMPLETE" });
    if let Err(panic) = steps {
        std::panic::resume_unwind(panic);
    }
    assert!(clean, "some publish-smoke-* folders are left on the zone");
}
