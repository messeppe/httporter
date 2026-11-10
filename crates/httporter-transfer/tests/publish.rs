// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compare, the publish plan, publishing and its CDN purge gate (I3), and moves, search, the
//! hash cache, multipart compares, Cache-Control rules and the job events (I4), against
//! `MockProvider` and temporary folders.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use httporter_core::{
    ByteStream, Capabilities, CdnPurger, CdnZone, ChecksumKind, CompareBy, CompareStatus,
    EmptyFolders, Entry, FileAction, FileOutcome, HistoryFile, Job, JobKind, Listing,
    MemoryJournal, MockOp, MockProvider, ObjectInfo, ProviderError, ProviderKind, PublishResult,
    PurgeMode, PurgeResult, RemotePath, SiteId, SkipRules, StorageProvider, TransferSettings,
    UploadOptions, UploadSource,
};
use httporter_transfer::{
    Batch, BatchTarget, CHECK_AFTER, CompareItem, CompareMode, CompareResult, Engine, FileHashes,
    HashCache, HashStore, JobEventOutcome, KindFilter, MovePlan, PublishPlan, SEARCH_LIMIT,
    SearchQuery, compare_trees, hash_file, messages, plan_move, run_move, run_publish, search,
};
use md5::{Digest, Md5};
use tokio::time::Instant;

fn p(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

fn describe(error: &ProviderError) -> String {
    format!("Failed: {error}")
}

fn engine() -> Engine {
    Engine::new(
        tokio::runtime::Handle::current(),
        Arc::new(MemoryJournal::new()),
        TransferSettings {
            simultaneous: 4,
            ..TransferSettings::default()
        },
        describe,
    )
}

fn site() -> SiteId {
    SiteId("site-1".into())
}

fn target(provider: Arc<dyn StorageProvider>) -> BatchTarget {
    BatchTarget {
        site: site(),
        site_name: "Test site".into(),
        host: "storage.example".into(),
        provider,
    }
}

fn bunny() -> Capabilities {
    Capabilities {
        checksum_in_listing: Some(ChecksumKind::Sha256),
        recursive_delete: true,
        empty_folders: EmptyFolders::Supported,
        ..Capabilities::default()
    }
}

fn s3() -> Capabilities {
    Capabilities {
        checksum_in_listing: Some(ChecksumKind::Md5),
        server_side_copy: true,
        empty_folders: EmptyFolders::Markers,
        batch_delete_max: 1000,
        ..Capabilities::default()
    }
}

/// Writes `files` (relative '/' paths and their contents) under `root`.
fn write_tree(root: &Path, files: &[(&str, &[u8])]) {
    for (rel, data) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
}

fn mock_with(caps: Capabilities, files: &[(&str, &[u8])]) -> MockProvider {
    files
        .iter()
        .fold(MockProvider::new(caps), |mock, (path, data)| {
            mock.with_file(path, data)
        })
}

async fn compare(
    provider: &dyn StorageProvider,
    local: &Path,
    remote: &str,
    mode: CompareMode,
    hashes: &HashCache,
) -> CompareResult {
    compare_trees(
        provider,
        local,
        &p(remote),
        &SkipRules::new(&SkipRules::defaults()),
        mode,
        hashes,
        &|_| {},
    )
    .await
    .unwrap()
}

fn status_of(r: &CompareResult) -> Vec<(&str, CompareStatus)> {
    r.items
        .iter()
        .map(|item| (item.rel.as_str(), item.status))
        .collect()
}

fn item<'a>(r: &'a CompareResult, rel: &str) -> &'a CompareItem {
    r.items.iter().find(|item| item.rel == rel).unwrap()
}

/// The local side most compares below use, and the site's side of it, under `site/`.
fn sample(dir: &Path, caps: Capabilities) -> MockProvider {
    write_tree(
        dir,
        &[
            ("new.txt", b"new"),
            ("size.txt", b"longer"),
            ("hash.txt", b"xyz"),
            ("same.txt", b"same"),
            ("sub/deep.txt", b"deep"),
            ("keys/id_rsa", b"secret"),
            (".env", b"A=1"),
            ("node_modules/x.js", b"x"),
        ],
    );
    mock_with(
        caps,
        &[
            ("site/size.txt", b"short"),
            ("site/hash.txt", b"abc"),
            ("site/same.txt", b"same"),
            ("site/sub/deep.txt", b"deep"),
            ("site/keys/id_rsa", b"secret"),
            ("site/gone.txt", b"gone"),
            ("site/.env", b"B=2"),
            ("site/node_modules/y.js", b"y"),
        ],
    )
}

// --- Compare ----------------------------------------------------------------------------------

#[tokio::test]
async fn compare_finds_new_changed_same_and_remote_only_by_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let mock = sample(dir.path(), bunny());
    let r = compare(
        &mock,
        dir.path(),
        "site",
        CompareMode::for_site(ProviderKind::Bunny, true),
        &HashCache::default(),
    )
    .await;
    use CompareStatus::{Changed, New, RemoteOnly, Same};
    assert_eq!(
        status_of(&r),
        [
            ("gone.txt", RemoteOnly),
            ("hash.txt", Changed),
            ("keys/id_rsa", Same),
            ("new.txt", New),
            ("same.txt", Same),
            ("size.txt", Changed),
            ("sub/deep.txt", Same),
        ],
        "the skip rules leave .env and node_modules out on both sides"
    );
    assert_eq!(item(&r, "size.txt").by, CompareBy::Size);
    assert_eq!(item(&r, "hash.txt").by, CompareBy::Sha256);
    assert_eq!(item(&r, "same.txt").by, CompareBy::Sha256);
    assert_eq!(r.secrets, ["keys/id_rsa"]);
    // `.env` and the node_modules folder.
    assert_eq!(r.skipped_local, 2);
    assert_eq!(r.remote_folders, 2, "sub and keys");
    let counts = r.counts();
    assert_eq!(
        (counts.changed, counts.new, counts.remote_only, counts.same),
        (2, 1, 1, 3)
    );
    assert_eq!(counts.upload_bytes, 6 + 3 + 3);
    assert_eq!(r.weakest_method(), CompareBy::Sha256);
}

#[tokio::test]
async fn bunny_with_verify_off_compares_by_size_only() {
    let dir = tempfile::tempdir().unwrap();
    let mock = sample(dir.path(), bunny());
    let r = compare(
        &mock,
        dir.path(),
        "site",
        CompareMode::for_site(ProviderKind::Bunny, false),
        &HashCache::default(),
    )
    .await;
    let hash = item(&r, "hash.txt");
    assert_eq!(
        (hash.status, hash.by),
        (CompareStatus::Same, CompareBy::Size)
    );
    assert_eq!(item(&r, "size.txt").status, CompareStatus::Changed);
    assert_eq!(r.weakest_method(), CompareBy::Size);
}

#[tokio::test]
async fn s3_compares_by_md5_etag() {
    let dir = tempfile::tempdir().unwrap();
    let mock = sample(dir.path(), s3());
    let r = compare(
        &mock,
        dir.path(),
        "site",
        CompareMode::for_site(ProviderKind::S3, false),
        &HashCache::default(),
    )
    .await;
    let hash = item(&r, "hash.txt");
    assert_eq!(
        (hash.status, hash.by),
        (CompareStatus::Changed, CompareBy::Md5)
    );
    let same = item(&r, "same.txt");
    assert_eq!(
        (same.status, same.by),
        (CompareStatus::Same, CompareBy::Md5)
    );
    assert!(!same.by_date);
}

#[tokio::test]
async fn s3_multipart_without_metadata_compares_by_date() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &[("big.bin", b"0123456789")]);
    let mock = mock_with(s3(), &[("site/big.bin", b"9876543210")])
        .with_file_etag("site/big.bin", "0123456789abcdef0123456789abcdef-2");
    let mode = CompareMode::for_site(ProviderKind::S3, false);
    // The site's copy is newer than the local file: the same.
    let r = compare(&mock, dir.path(), "site", mode, &HashCache::default()).await;
    let big = item(&r, "big.bin");
    assert_eq!((big.status, big.by), (CompareStatus::Same, CompareBy::Size));
    assert!(big.by_date);

    // The local file changed after the upload.
    set_mtime(&dir.path().join("big.bin"), Duration::from_secs(3600));
    let r = compare(&mock, dir.path(), "site", mode, &HashCache::default()).await;
    assert_eq!(item(&r, "big.bin").status, CompareStatus::Changed);
}

fn set_mtime(path: &Path, ahead: Duration) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() + ahead).unwrap();
}

#[tokio::test]
async fn a_missing_remote_root_makes_everything_new() {
    let dir = tempfile::tempdir().unwrap();
    let mock = sample(dir.path(), bunny());
    let r = compare(
        &mock,
        dir.path(),
        "nothing/here",
        CompareMode::for_site(ProviderKind::Bunny, true),
        &HashCache::default(),
    )
    .await;
    assert!(!r.items.is_empty());
    assert!(r.items.iter().all(|i| i.status == CompareStatus::New));
    assert_eq!(r.remote_folders, 0);
}

fn compared(rel: &str, status: CompareStatus) -> CompareItem {
    let local = (status != CompareStatus::RemoteOnly).then_some(10);
    let remote = (status != CompareStatus::New).then_some(if status == CompareStatus::Changed {
        11
    } else {
        10
    });
    CompareItem {
        rel: rel.into(),
        status,
        local_size: local,
        remote_size: remote,
        by: CompareBy::Sha256,
        by_date: false,
    }
}

fn result_of(items: Vec<CompareItem>) -> CompareResult {
    CompareResult {
        local_root: PathBuf::from(r"D:\site"),
        remote_root: p("www"),
        items,
        remote_folders: 4,
        skipped_local: 0,
        secrets: Vec::new(),
        regions: 2,
    }
}

#[test]
fn child_statuses_sum_up_folders() {
    use CompareStatus::{Changed, New, RemoteOnly, Same};
    let r = result_of(vec![
        compared("top.txt", Same),
        compared("a/x.txt", Same),
        compared("a/y/z.txt", Changed),
        compared("b/n.txt", New),
        compared("c/r.txt", RemoteOnly),
        compared("d/s.txt", Same),
        compared("e/n.txt", New),
        compared("e/r.txt", RemoteOnly),
    ]);
    let root = r.child_statuses("");
    let want: HashMap<String, CompareStatus> = [
        ("top.txt", Same),
        ("a", Changed),
        ("b", New),
        ("c", RemoteOnly),
        ("d", Same),
        ("e", Changed),
    ]
    .into_iter()
    .map(|(name, status)| (name.to_owned(), status))
    .collect();
    assert_eq!(root, want);
    let a = r.child_statuses("/a/");
    assert_eq!(a.len(), 2);
    assert_eq!((a["x.txt"], a["y"]), (Same, Changed));
    assert!(r.child_statuses("nothing").is_empty());
}

// --- The plan ---------------------------------------------------------------------------------

#[test]
fn the_plan_toggles_and_makes_jobs() {
    use CompareStatus::{Changed, New, RemoteOnly, Same};
    let r = result_of(vec![
        compared("z-new.txt", New),
        compared("old.txt", RemoteOnly),
        compared("a/changed.txt", Changed),
        compared("same.txt", Same),
    ]);
    let mut plan = PublishPlan::from_compare(&r, false);
    let rels: Vec<(&str, bool)> = plan
        .items
        .iter()
        .map(|i| (i.rel.as_str(), i.include))
        .collect();
    assert_eq!(
        rels,
        [
            ("a/changed.txt", true),
            ("z-new.txt", true),
            ("old.txt", false)
        ],
        "Changed, New, RemoteOnly; extras kept by default"
    );
    assert_eq!(plan.same, 1);
    assert_eq!(
        plan.remote_files, 3,
        "changed, old and same are on the site"
    );
    assert_eq!((plan.remote_folders, plan.regions), (4, 2));
    assert_eq!(plan.delete_count(), 0);
    assert!(plan.deletes().is_empty());
    assert_eq!(plan.upload_totals(), (2, 20));
    assert_eq!(
        plan.uploads(),
        [
            Job::Upload {
                local: PathBuf::from(r"D:\site\a\changed.txt"),
                remote: p("www/a/changed.txt"),
                size: 10,
            },
            Job::Upload {
                local: PathBuf::from(r"D:\site\z-new.txt"),
                remote: p("www/z-new.txt"),
                size: 10,
            },
        ]
    );

    plan.set_remove_extras(true);
    assert_eq!(plan.delete_count(), 1);
    assert_eq!(
        plan.deletes(),
        [Job::Delete {
            remote: p("www/old.txt"),
            folder: false,
        }]
    );
    assert_eq!(plan.touched_paths().len(), 3);
    assert_eq!(PublishPlan::from_compare(&r, true).delete_count(), 1);

    // An upload left out is no job and no touched path.
    plan.items[0].include = false;
    assert_eq!(plan.upload_totals(), (1, 10));
    assert!(!plan.touched_paths().contains(&p("www/a/changed.txt")));
    let text = plan.export_text("Publish", "Main");
    assert!(text.starts_with("Publish\n"));
    assert!(text.contains("\ta/changed.txt\t10\n"));
    assert!(text.contains("\told.txt\t10\n"));
}

#[test]
fn retry_of_tries_again_only_what_was_not_done() {
    let file = |path: &str, action, outcome| HistoryFile {
        path: p(path),
        action,
        outcome,
        size: 7,
    };
    let plan = PublishPlan::retry_of(
        PathBuf::from(r"D:\site"),
        p("www"),
        &[
            file("www/done.txt", FileAction::Upload, FileOutcome::Done),
            file(
                "www/failed.txt",
                FileAction::Upload,
                FileOutcome::Failed("x".into()),
            ),
            file("www/failed.txt", FileAction::Upload, FileOutcome::Cancelled),
            file(
                "www/kept.txt",
                FileAction::Delete,
                FileOutcome::NotRun(messages::NOT_DELETED_UPLOADS_FAILED.into()),
            ),
            file(
                "elsewhere/x.txt",
                FileAction::Upload,
                FileOutcome::Cancelled,
            ),
        ],
    );
    let items: Vec<(&str, CompareStatus, bool)> = plan
        .items
        .iter()
        .map(|i| (i.rel.as_str(), i.status, i.include))
        .collect();
    assert_eq!(
        items,
        [
            ("failed.txt", CompareStatus::New, true),
            ("kept.txt", CompareStatus::RemoteOnly, true),
        ]
    );
    assert_eq!(plan.upload_totals(), (1, 7));
    assert_eq!(plan.delete_count(), 1);
}

// --- A provider with what MockProvider lacks --------------------------------------------------

/// Forwards to a mock, and adds: uploads of some paths fail, `stat` answers with metadata, a
/// server-side copy, and a record of each upload's Cache-Control.
struct Site {
    mock: Arc<MockProvider>,
    fail_uploads: Vec<String>,
    metadata: HashMap<String, Vec<(String, String)>>,
    stat_fails: bool,
    cache_control: Mutex<HashMap<String, Option<String>>>,
    copies: AtomicUsize,
}

impl Site {
    fn new(mock: MockProvider) -> Self {
        Self {
            mock: Arc::new(mock),
            fail_uploads: Vec::new(),
            metadata: HashMap::new(),
            stat_fails: false,
            cache_control: Mutex::new(HashMap::new()),
            copies: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl StorageProvider for Site {
    fn capabilities(&self) -> &Capabilities {
        self.mock.capabilities()
    }

    fn endpoint(&self) -> String {
        self.mock.endpoint()
    }

    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        self.mock.list(dir).await
    }

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        self.cache_control
            .lock()
            .unwrap()
            .insert(path.as_str().to_owned(), opts.cache_control.clone());
        if self.fail_uploads.iter().any(|f| f == path.as_str()) {
            return Err(ProviderError::Permanent("this file is refused".into()));
        }
        self.mock.upload(path, source, opts).await
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        self.mock.download(path).await
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        self.mock.delete_file(path).await
    }

    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.mock.delete_folder(dir).await
    }

    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.mock.create_folder(dir).await
    }

    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        self.mock.delete_batch(paths).await
    }

    async fn reachable(&self) -> Result<(), ProviderError> {
        self.mock.reachable().await
    }

    async fn stat(&self, path: &RemotePath) -> Result<ObjectInfo, ProviderError> {
        if self.stat_fails {
            return Err(ProviderError::Network("no answer".into()));
        }
        let mut info = self.mock.stat(path).await?;
        info.metadata = self
            .metadata
            .get(path.as_str())
            .cloned()
            .unwrap_or_default();
        Ok(info)
    }

    async fn copy(&self, from: &RemotePath, to: &RemotePath) -> Result<(), ProviderError> {
        self.copies.fetch_add(1, Ordering::SeqCst);
        let data = self.mock.contents(from).ok_or(ProviderError::NotFound)?;
        self.mock
            .upload(
                to,
                UploadSource::Bytes(data),
                &UploadOptions::new("application/octet-stream"),
            )
            .await
    }
}

// --- Multipart compares -----------------------------------------------------------------------

/// The ETag S3 gives `data` uploaded in parts of `part` bytes.
fn multipart_etag(data: &[u8], part: usize) -> String {
    let mut digests = Vec::new();
    let chunks: Vec<&[u8]> = data.chunks(part).collect();
    for chunk in &chunks {
        digests.extend_from_slice(&Md5::digest(chunk));
    }
    format!("{}-{}", hex::encode(Md5::digest(&digests)), chunks.len())
}

async fn compare_multipart(site: &Site, local: &Path) -> CompareItem {
    let r = compare(
        site,
        local,
        "site",
        CompareMode::for_site(ProviderKind::S3, false),
        &HashCache::default(),
    )
    .await;
    item(&r, "big.bin").clone()
}

#[tokio::test]
async fn multipart_compares_by_recorded_sha256_then_part_size_then_date() {
    let dir = tempfile::tempdir().unwrap();
    let data = b"0123456789";
    write_tree(dir.path(), &[("big.bin", data)]);
    let sha = hex::encode(hash_file(&dir.path().join("big.bin")).unwrap().sha256);
    let mock = || {
        mock_with(s3(), &[("site/big.bin", b"----------")])
            .with_file_etag("site/big.bin", &multipart_etag(data, 4))
    };
    let meta = |name: &str, value: &str| {
        HashMap::from([(
            "site/big.bin".to_owned(),
            vec![(name.to_owned(), value.to_owned())],
        )])
    };

    // The SHA-256 recorded at upload.
    let mut site = Site::new(mock());
    site.metadata = meta(httporter_core::META_SHA256, &sha);
    let told = compare_multipart(&site, dir.path()).await;
    assert_eq!(
        (told.status, told.by),
        (CompareStatus::Same, CompareBy::Sha256)
    );
    site.metadata = meta(httporter_core::META_SHA256, &"0".repeat(64));
    let told = compare_multipart(&site, dir.path()).await;
    assert_eq!(
        (told.status, told.by),
        (CompareStatus::Changed, CompareBy::Sha256)
    );

    // The part size recorded: the ETag recomputed from the local file.
    site.metadata = meta(httporter_core::META_PART_SIZE, "4");
    let told = compare_multipart(&site, dir.path()).await;
    assert_eq!(
        (told.status, told.by),
        (CompareStatus::Same, CompareBy::Md5)
    );
    site.metadata = meta(httporter_core::META_PART_SIZE, "5");
    let told = compare_multipart(&site, dir.path()).await;
    assert_eq!(
        (told.status, told.by),
        (CompareStatus::Changed, CompareBy::Md5)
    );

    // Nothing recorded, or no answer: size and date.
    site.metadata = HashMap::new();
    let told = compare_multipart(&site, dir.path()).await;
    assert_eq!((told.by, told.by_date), (CompareBy::Size, true));
    site.stat_fails = true;
    let told = compare_multipart(&site, dir.path()).await;
    assert_eq!(
        (told.status, told.by, told.by_date),
        (CompareStatus::Same, CompareBy::Size, true)
    );
}

// --- Hash cache with a store ------------------------------------------------------------------

#[derive(Default)]
struct Kept {
    hashes: Mutex<HashMap<PathBuf, FileHashes>>,
    loads: AtomicUsize,
    hits: AtomicUsize,
    saves: AtomicUsize,
}

impl HashStore for Kept {
    fn load(&self, path: &Path, size: u64, modified: SystemTime) -> Option<FileHashes> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        let found = self
            .hashes
            .lock()
            .unwrap()
            .get(path)
            .filter(|h| h.size == size && h.modified == modified)
            .cloned();
        if found.is_some() {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }
        found
    }

    fn save(&self, path: &Path, h: &FileHashes) {
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.hashes
            .lock()
            .unwrap()
            .insert(path.to_owned(), h.clone());
    }
}

#[tokio::test]
async fn the_hash_cache_keeps_digests_in_its_store_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mock = sample(dir.path(), bunny());
    let store = Arc::new(Kept::default());
    let mode = CompareMode::for_site(ProviderKind::Bunny, true);

    let first = HashCache::with_store(store.clone());
    compare(&mock, dir.path(), "site", mode, &first).await;
    // hash.txt, same.txt, sub/deep.txt and keys/id_rsa: equal sizes with a digest listed.
    assert_eq!(store.saves.load(Ordering::SeqCst), 4);
    // In memory now: the same cache asks the store nothing more.
    let loads = store.loads.load(Ordering::SeqCst);
    compare(&mock, dir.path(), "site", mode, &first).await;
    assert_eq!(store.loads.load(Ordering::SeqCst), loads);

    // A new cache (a restart) finds them in the store and hashes nothing.
    let second = HashCache::with_store(store.clone());
    let r = compare(&mock, dir.path(), "site", mode, &second).await;
    assert_eq!(store.saves.load(Ordering::SeqCst), 4);
    assert_eq!(store.hits.load(Ordering::SeqCst), 4);
    assert_eq!(item(&r, "same.txt").status, CompareStatus::Same);

    // A file that changed is hashed again.
    std::fs::write(dir.path().join("same.txt"), b"SAME").unwrap();
    set_mtime(&dir.path().join("same.txt"), Duration::from_secs(60));
    let r = compare(&mock, dir.path(), "site", mode, &second).await;
    assert_eq!(item(&r, "same.txt").status, CompareStatus::Changed);
    assert_eq!(store.saves.load(Ordering::SeqCst), 5);

    // get_or_hash: the store's digests, else the file's.
    let third = HashCache::with_store(store.clone());
    let path = dir.path().join("hash.txt");
    assert_eq!(third.get_or_hash(&path).unwrap(), hash_file(&path).unwrap());
    assert_eq!(store.saves.load(Ordering::SeqCst), 5);
}

// --- Publishing -------------------------------------------------------------------------------

/// A purger that records each URL with how many late checks were still waiting then.
struct Recording {
    zones: Vec<CdnZone>,
    engine: Engine,
    calls: Mutex<Vec<(String, usize, Instant)>>,
}

impl Recording {
    fn new(engine: &Engine) -> Self {
        Self {
            zones: vec![CdnZone {
                id: 1,
                name: "pull".into(),
                hostnames: vec!["cdn.example".into()],
            }],
            engine: engine.clone(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.0.clone())
            .collect();
        urls.sort();
        urls
    }
}

#[async_trait]
impl CdnPurger for Recording {
    fn zones(&self) -> &[CdnZone] {
        &self.zones
    }

    async fn purge_url(&self, url: &str) -> Result<(), ProviderError> {
        let pending = self.engine.pending_checks_for(&site());
        self.calls
            .lock()
            .unwrap()
            .push((url.to_owned(), pending, Instant::now()));
        Ok(())
    }

    async fn purge_zone(&self, _id: u64) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Ruling Q55: the purge waits for the late checks of the uploads a pause cut off.
#[tokio::test(start_paused = true)]
async fn the_purge_waits_for_the_late_checks() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(
        dir.path(),
        &[("a.txt", b"a"), ("b.txt", b"b"), ("c/index.html", b"c")],
    );
    let mock = Arc::new(MockProvider::new(Capabilities {
        cut_uploads_delete_late: true,
        ..Capabilities::default()
    }));
    mock.set_latency(MockOp::Upload, Duration::from_secs(10));
    let r = compare(
        &*mock,
        dir.path(),
        "",
        CompareMode::for_site(ProviderKind::Bunny, false),
        &HashCache::default(),
    )
    .await;
    let plan = PublishPlan::from_compare(&r, false);
    let e = engine();
    let purger = Arc::new(Recording::new(&e));
    let started = Instant::now();

    let no_phase = |_: httporter_transfer::PublishPhase| {};
    let publish = run_publish(
        &e,
        target(mock.clone()),
        &plan,
        PurgeMode::Changed,
        Some(purger.clone()),
        &no_phase,
    );
    let drive = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(e.snapshot().active_count, 3);
        e.pause_all();
        tokio::time::sleep(Duration::from_secs(1)).await;
        e.resume_all();
        let mut saw_pending = false;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if e.pending_checks_for(&site()) > 0 {
                saw_pending = true;
                assert!(purger.calls.lock().unwrap().is_empty(), "purged too early");
                break;
            }
        }
        assert!(saw_pending, "the cut-off uploads were never checked");
    };
    let (outcome, ()) = tokio::join!(publish, drive);
    let outcome = outcome.unwrap();
    assert_eq!(outcome.result, PublishResult::Ok);
    assert_eq!(outcome.uploaded, 3);
    assert_eq!(outcome.purge, PurgeResult::Urls(5));
    let calls = purger.calls.lock().unwrap();
    assert_eq!(calls.len(), 5);
    for (url, pending, at) in calls.iter() {
        assert_eq!(*pending, 0, "{url} purged with checks waiting");
        assert!(
            *at - started >= CHECK_AFTER,
            "{url} purged before the checks"
        );
    }
}

#[tokio::test]
async fn a_failed_upload_holds_the_deletes_and_purges_what_was_done() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &[("good.txt", b"good"), ("bad.txt", b"bad")]);
    let mut site_ = Site::new(mock_with(bunny(), &[("www/extra.txt", b"extra")]));
    site_.fail_uploads = vec!["www/bad.txt".into()];
    let site_ = Arc::new(site_);
    let r = compare(
        &*site_,
        dir.path(),
        "www",
        CompareMode::for_site(ProviderKind::Bunny, true),
        &HashCache::default(),
    )
    .await;
    let plan = PublishPlan::from_compare(&r, true);
    assert_eq!(plan.delete_count(), 1);
    let e = engine();
    let purger = Arc::new(Recording::new(&e));
    let phases = Mutex::new(Vec::new());
    let outcome = run_publish(
        &e,
        target(site_.clone()),
        &plan,
        PurgeMode::Changed,
        Some(purger.clone()),
        &|phase| phases.lock().unwrap().push(phase),
    )
    .await
    .unwrap();

    assert_eq!(outcome.result, PublishResult::Partial);
    assert_eq!(
        (outcome.uploaded, outcome.deleted, outcome.failed),
        (1, 0, 1)
    );
    let of = |path: &str| {
        outcome
            .files
            .iter()
            .find(|f| f.path == p(path))
            .unwrap()
            .clone()
    };
    assert_eq!(of("www/good.txt").outcome, FileOutcome::Done);
    assert!(matches!(of("www/bad.txt").outcome, FileOutcome::Failed(_)));
    let extra = of("www/extra.txt");
    assert_eq!(extra.action, FileAction::Delete);
    assert_eq!(
        extra.outcome,
        FileOutcome::NotRun(messages::NOT_DELETED_UPLOADS_FAILED.into())
    );
    assert_eq!(extra.size, 5);
    assert!(
        site_.mock.contents(&p("www/extra.txt")).is_some(),
        "nothing deleted"
    );
    assert_eq!(outcome.purge, PurgeResult::Urls(1));
    assert_eq!(purger.urls(), ["https://cdn.example/www/good.txt"]);
    assert!(
        !phases
            .lock()
            .unwrap()
            .contains(&httporter_transfer::PublishPhase::Deleting)
    );

    let record = outcome.into_record(httporter_core::ProfileId::new(), &plan, true);
    assert_eq!((record.uploaded, record.failed, record.skipped), (1, 1, 0));
    // The site had extra.txt; good.txt is new there now.
    assert_eq!(record.remote_files, 2);
}

#[tokio::test]
async fn a_clean_publish_uploads_deletes_and_purges_without_a_purger_says_why() {
    let dir = tempfile::tempdir().unwrap();
    write_tree(dir.path(), &[("a.txt", b"a")]);
    let mock = Arc::new(mock_with(bunny(), &[("old.txt", b"old")]));
    let r = compare(
        &*mock,
        dir.path(),
        "",
        CompareMode::for_site(ProviderKind::Bunny, true),
        &HashCache::default(),
    )
    .await;
    let plan = PublishPlan::from_compare(&r, true);
    let e = engine();
    let outcome = run_publish(
        &e,
        target(mock.clone()),
        &plan,
        PurgeMode::Changed,
        None,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.result, PublishResult::Ok);
    assert_eq!((outcome.uploaded, outcome.deleted), (1, 1));
    assert_eq!(
        outcome.purge,
        PurgeResult::Unavailable(messages::NO_PURGER.into())
    );
    assert!(mock.contents(&p("old.txt")).is_none());
    assert_eq!(mock.contents(&p("a.txt")).unwrap().as_ref(), b"a");

    let off = run_publish(
        &e,
        target(mock.clone()),
        &plan,
        PurgeMode::Off,
        None,
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(off.purge, PurgeResult::Off);
}

// --- Moves ------------------------------------------------------------------------------------

async fn entry(provider: &dyn StorageProvider, path: &str) -> Entry {
    provider.stat(&p(path)).await.unwrap().entry
}

async fn folder_entry(provider: &dyn StorageProvider, path: &str) -> Entry {
    let path = p(path);
    provider
        .list(&path.parent().unwrap())
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.path == path)
        .unwrap()
}

#[tokio::test]
async fn plan_move_renames_expands_and_refuses() {
    let mock = mock_with(
        bunny(),
        &[
            ("a/one.txt", b"1"),
            ("a/sub/two.txt", b"22"),
            ("b/one.txt", b"x"),
            ("c.txt", b"333"),
        ],
    )
    .with_folder("a/empty");

    let file = entry(&mock, "c.txt").await;
    let renamed = plan_move(
        &mock,
        std::slice::from_ref(&file),
        &p("b"),
        Some(" new.txt "),
    )
    .await
    .unwrap();
    assert_eq!(renamed.items.len(), 1);
    assert_eq!(
        (renamed.items[0].to.clone(), renamed.bytes),
        (p("b/new.txt"), 3)
    );
    assert!(!renamed.server_side);

    let folder = folder_entry(&mock, "a").await;
    let expanded = plan_move(&mock, std::slice::from_ref(&folder), &p("d"), None)
        .await
        .unwrap();
    let moves: Vec<(&str, &str)> = expanded
        .items
        .iter()
        .map(|i| (i.from.as_str(), i.to.as_str()))
        .collect();
    assert_eq!(
        moves,
        [
            ("a/one.txt", "d/a/one.txt"),
            ("a/sub/two.txt", "d/a/sub/two.txt")
        ]
    );
    let folders: Vec<&str> = expanded.folders.iter().map(|f| f.1.as_str()).collect();
    assert_eq!(folders, ["d/a", "d/a/empty", "d/a/sub"]);
    assert_eq!(expanded.bytes, 3);

    let refused = |e: ProviderError| match e {
        ProviderError::Refused(text) => text,
        other => panic!("{other:?}"),
    };
    let into_itself = plan_move(&mock, std::slice::from_ref(&folder), &p("a/sub"), None)
        .await
        .unwrap_err();
    assert_eq!(refused(into_itself), messages::MOVE_INTO_ITSELF);
    let exists = plan_move(&mock, &[entry(&mock, "a/one.txt").await], &p("b"), None)
        .await
        .unwrap_err();
    assert_eq!(refused(exists), messages::move_exists("one.txt"));
    let two = plan_move(&mock, &[file.clone(), folder], &p("b"), Some("x"))
        .await
        .unwrap_err();
    assert_eq!(refused(two), messages::MOVE_RENAME_ONE);
    let same = plan_move(&mock, &[file], &RemotePath::root(), None)
        .await
        .unwrap_err();
    assert_eq!(refused(same), messages::MOVE_SAME_PLACE);
}

#[tokio::test]
async fn a_bunny_move_whose_upload_fails_deletes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let mut site_ = Site::new(mock_with(
        bunny(),
        &[("src/a.txt", b"aa"), ("src/b.txt", b"bb")],
    ));
    site_.fail_uploads = vec!["dst/src/b.txt".into()];
    let site_ = Arc::new(site_);
    let plan = plan_move(
        &*site_,
        &[folder_entry(&*site_, "src").await],
        &p("dst"),
        None,
    )
    .await
    .unwrap();
    assert!(!plan.server_side);
    let e = engine();
    let outcome = run_move(&e, target(site_.clone()), &plan, temp.path(), &|_| {})
        .await
        .unwrap();
    assert_eq!(outcome.copied, 1);
    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].0, p("src/b.txt"), "named by the original");
    assert_eq!(outcome.kept_copies, [p("dst/src/a.txt")]);
    for kept in ["src/a.txt", "src/b.txt", "dst/src/a.txt"] {
        assert!(site_.mock.contents(&p(kept)).is_some(), "{kept}");
    }
    assert_eq!(
        std::fs::read_dir(temp.path()).unwrap().count(),
        0,
        "temp emptied"
    );
}

#[tokio::test]
async fn a_bunny_move_that_works_removes_the_originals() {
    let temp = tempfile::tempdir().unwrap();
    let mock = Arc::new(
        mock_with(bunny(), &[("src/a.txt", b"aa"), ("src/b.txt", b"bb")]).with_folder("src/e"),
    );
    let plan = plan_move(
        &*mock,
        &[folder_entry(&*mock, "src").await],
        &p("dst"),
        None,
    )
    .await
    .unwrap();
    let outcome = run_move(&engine(), target(mock.clone()), &plan, temp.path(), &|_| {})
        .await
        .unwrap();
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
    assert_eq!((outcome.copied, outcome.deleted), (2, 2));
    assert_eq!(mock.contents(&p("dst/src/b.txt")).unwrap().as_ref(), b"bb");
    assert!(
        mock.list(&p("dst/src/e")).await.is_ok(),
        "the empty folder made"
    );
    assert_eq!(
        mock.list(&p("src")).await.unwrap_err(),
        ProviderError::NotFound
    );
}

#[tokio::test]
async fn an_s3_move_copies_on_the_server() {
    let temp = tempfile::tempdir().unwrap();
    let site_ = Arc::new(Site::new(mock_with(
        s3(),
        &[("x/a.txt", b"a"), ("x/b.txt", b"b"), ("y.txt", b"y")],
    )));
    let sources = [
        folder_entry(&*site_, "x").await,
        entry(&*site_, "y.txt").await,
    ];
    let plan: MovePlan = plan_move(&*site_, &sources, &p("z"), None).await.unwrap();
    assert!(plan.server_side);
    let outcome = run_move(
        &engine(),
        target(site_.clone()),
        &plan,
        temp.path(),
        &|_| {},
    )
    .await
    .unwrap();
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
    assert_eq!((outcome.copied, outcome.deleted), (3, 3));
    assert_eq!(site_.copies.load(Ordering::SeqCst), 3);
    assert_eq!(site_.mock.calls(MockOp::Download), 0, "nothing came down");
    for (gone, there) in [("x/a.txt", "z/x/a.txt"), ("y.txt", "z/y.txt")] {
        assert!(site_.mock.contents(&p(gone)).is_none());
        assert!(site_.mock.contents(&p(there)).is_some());
    }
}

// --- Search -----------------------------------------------------------------------------------

async fn found(provider: &dyn StorageProvider, q: &SearchQuery) -> Vec<String> {
    let all = Mutex::new(Vec::new());
    let progress = search(provider, &RemotePath::root(), q, &|entries, _| {
        all.lock()
            .unwrap()
            .extend(entries.into_iter().map(|e| e.path.as_str().to_owned()));
    })
    .await
    .unwrap();
    assert!(progress.done);
    let mut all = all.into_inner().unwrap();
    all.sort();
    all
}

#[tokio::test]
async fn search_by_glob_kind_and_size() {
    let mock = mock_with(
        bunny(),
        &[
            ("notes.txt", b"1"),
            ("docs/Readme.TXT", b"12345"),
            ("docs/txt/photo.png", b"123456789"),
            ("css/site.css", b"12"),
        ],
    );
    let q = |text: &str| SearchQuery {
        text: text.into(),
        ..SearchQuery::default()
    };
    assert_eq!(
        found(&mock, &q("*.txt")).await,
        ["docs/Readme.TXT", "notes.txt"]
    );
    assert_eq!(
        found(&mock, &q("txt")).await,
        ["docs/Readme.TXT", "docs/txt", "notes.txt"],
        "a substring matches folders too"
    );
    let images = SearchQuery {
        kind: Some(KindFilter::Image),
        ..SearchQuery::default()
    };
    assert_eq!(found(&mock, &images).await, ["docs/txt/photo.png"]);
    let code = SearchQuery {
        kind: Some(KindFilter::Code),
        ..SearchQuery::default()
    };
    assert_eq!(found(&mock, &code).await, ["css/site.css"]);
    let big = SearchQuery {
        min_size: Some(5),
        ..SearchQuery::default()
    };
    assert_eq!(
        found(&mock, &big).await,
        ["docs/Readme.TXT", "docs/txt/photo.png"]
    );
}

#[tokio::test]
async fn search_stops_at_the_limit() {
    let mock = (0..SEARCH_LIMIT + 5).fold(MockProvider::new(bunny()), |mock, i| {
        mock.with_file(&format!("f{i}.txt"), b"x")
    });
    let calls = AtomicUsize::new(0);
    let total = AtomicUsize::new(0);
    let progress = search(
        &mock,
        &RemotePath::root(),
        &SearchQuery::default(),
        &|e, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            total.fetch_add(e.len(), Ordering::SeqCst);
        },
    )
    .await
    .unwrap();
    assert!(progress.done);
    assert_eq!(progress.found, SEARCH_LIMIT as u64);
    assert_eq!(total.load(Ordering::SeqCst), SEARCH_LIMIT);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// --- Cache-Control rules and job events -------------------------------------------------------

fn batch(provider: Arc<dyn StorageProvider>, jobs: Vec<Job>) -> Batch {
    Batch {
        site: site(),
        site_name: "Test site".into(),
        host: "storage.example".into(),
        provider,
        jobs,
        checksums: HashMap::new(),
    }
}

fn uploads(dir: &Path, remotes: &[&str]) -> Vec<Job> {
    remotes
        .iter()
        .enumerate()
        .map(|(i, remote)| {
            let local = dir.join(format!("{i}.txt"));
            std::fs::write(&local, b"data").unwrap();
            Job::Upload {
                local,
                remote: p(remote),
                size: 4,
            }
        })
        .collect()
}

#[tokio::test]
async fn cache_control_follows_the_longest_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let site_ = Arc::new(Site::new(MockProvider::new(bunny())));
    let e = engine();
    e.set_cache_control(&site(), &RemotePath::root(), Some("max-age=60".into()));
    e.set_cache_control(&site(), &p("blog"), Some(" max-age=600 ".into()));
    e.set_cache_control(&site(), &p("blog/img"), Some("max-age=86400".into()));
    e.set_cache_control(&site(), &p("other"), Some("   ".into()));
    let paths = ["x.txt", "blog/p.html", "blog/img/i.png", "blogger/q.txt"];
    let id = e
        .submit_tracked(batch(site_.clone(), uploads(dir.path(), &paths)))
        .unwrap();
    assert!(e.wait_batch(&id).await.failed.is_empty());
    let seen = site_.cache_control.lock().unwrap().clone();
    let of = |path: &str| seen[path].as_deref();
    assert_eq!(of("x.txt"), Some("max-age=60"));
    assert_eq!(of("blog/p.html"), Some("max-age=600"), "trimmed");
    assert_eq!(of("blog/img/i.png"), Some("max-age=86400"));
    assert_eq!(
        of("blogger/q.txt"),
        Some("max-age=60"),
        "whole segments only"
    );

    e.set_cache_control(&site(), &p("blog"), None);
    e.set_cache_control(&site(), &RemotePath::root(), None);
    let id = e
        .submit_tracked(batch(site_.clone(), uploads(dir.path(), &paths[1..3])))
        .unwrap();
    e.wait_batch(&id).await;
    let seen = site_.cache_control.lock().unwrap().clone();
    assert_eq!(seen["blog/p.html"], None);
    assert_eq!(seen["blog/img/i.png"].as_deref(), Some("max-age=86400"));
}

#[tokio::test]
async fn events_tell_each_job_that_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut site_ = Site::new(MockProvider::new(bunny()));
    site_.fail_uploads = vec!["bad.txt".into()];
    let site_ = Arc::new(site_);
    let e = engine();
    let mut events = e.events();
    let id = e
        .submit_tracked(batch(
            site_.clone(),
            uploads(dir.path(), &["ok.txt", "bad.txt"]),
        ))
        .unwrap();
    e.wait_batch(&id).await;
    let mut got = Vec::new();
    for _ in 0..2 {
        got.push(
            tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    got.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(got[0].path, "bad.txt");
    assert!(matches!(got[0].outcome, JobEventOutcome::Failed(_)));
    assert_eq!(got[1].path, "ok.txt");
    assert_eq!(got[1].outcome, JobEventOutcome::Done);
    assert_eq!(got[1].bytes, 4);
    for event in &got {
        assert_eq!((event.kind, event.site.clone()), (JobKind::Upload, site()));
        assert_eq!(event.site_name, "Test site");
    }
}
