//! Resume after a crash, offline pauses, damaged or changing files and a full disk (Task 14),
//! against `MockProvider` on a paused tokio clock: every wait below is virtual time.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::StreamExt;
use httporter_core::{
    ByteStream, Capabilities, Checksum, ChecksumKind, EngineSnapshot, EngineState, Job, JobId,
    JobRecord, JobState, Journal, JournalError, Listing, MemoryJournal, MockOp, MockProvider,
    MultipartProgress, ProviderError, RemotePath, SiteId, StorageProvider, TransferSettings,
    UploadOptions, UploadSource,
};
use httporter_transfer::{Batch, Engine, FileHashes, LocalFiles, SiteInfo, hash_file, messages};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

const SITE_NAME: &str = "Test site";
const HOST: &str = "storage.example";

/// The plain words the app would show; the engine only passes errors through this.
fn describe(err: &ProviderError) -> String {
    match err {
        ProviderError::Network(_) => "No connection".into(),
        other => format!("Failed: {other}"),
    }
}

fn p(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

fn with(simultaneous: u32) -> TransferSettings {
    TransferSettings {
        simultaneous,
        ..TransferSettings::default()
    }
}

fn engine_on(journal: Arc<dyn Journal>, settings: TransferSettings) -> Engine {
    Engine::new(
        tokio::runtime::Handle::current(),
        journal,
        settings,
        describe,
    )
}

fn engine(settings: TransferSettings) -> (Engine, Arc<MemoryJournal>) {
    let journal = Arc::new(MemoryJournal::new());
    (engine_on(journal.clone(), settings), journal)
}

fn site() -> SiteId {
    SiteId("site-1".into())
}

fn batch(provider: Arc<dyn StorageProvider>, jobs: Vec<Job>) -> Batch {
    Batch {
        site: site(),
        site_name: SITE_NAME.into(),
        host: HOST.into(),
        provider,
        jobs,
        checksums: HashMap::new(),
    }
}

/// What the app's lookup answers for a journal's sites: `site()` is still there, with
/// `provider` as its client; no other site exists.
fn only_site(provider: Arc<dyn StorageProvider>) -> impl Fn(&SiteId) -> Option<SiteInfo> {
    move |id: &SiteId| {
        (*id == site()).then(|| (SITE_NAME.to_owned(), HOST.to_owned(), provider.clone()))
    }
}

/// The bytes of local file number `i`: `size` bytes that differ from file to file.
fn content(i: usize, size: usize) -> Vec<u8> {
    (0..size).map(|b| ((b + i * 7) % 251) as u8).collect()
}

/// Writes `n` local files of `size` bytes (`f0.txt`, …) and returns upload jobs sending them to
/// `up/f0.txt`, ….
fn upload_jobs(dir: &Path, n: usize, size: usize) -> Vec<Job> {
    std::fs::create_dir_all(dir).unwrap();
    (0..n)
        .map(|i| {
            let local = dir.join(format!("f{i}.txt"));
            std::fs::write(&local, content(i, size)).unwrap();
            Job::Upload {
                local,
                remote: p(&format!("up/f{i}.txt")),
                size: size as u64,
            }
        })
        .collect()
}

fn download(remote: &str, local: PathBuf, size: u64) -> Job {
    Job::Download {
        remote: p(remote),
        local,
        size,
    }
}

fn delete(remote: &str) -> Job {
    Job::Delete {
        remote: p(remote),
        folder: false,
    }
}

/// The remote path a job works on, as text.
fn remote_of(job: &Job) -> String {
    match job {
        Job::Upload { remote, .. }
        | Job::Download { remote, .. }
        | Job::Delete { remote, .. }
        | Job::CreateFolder { remote } => remote.as_str().to_owned(),
    }
}

/// Journal records for `jobs` on `site`, as a crash would leave them: queued, ids unset.
fn queued_records(site: &SiteId, jobs: Vec<Job>) -> Vec<JobRecord> {
    jobs.into_iter()
        .map(|job| JobRecord {
            id: 0,
            batch: "left-by-a-crash".into(),
            site: site.clone(),
            job,
            state: JobState::Queued,
            error: None,
            multipart: None,
        })
        .collect()
}

/// A short account of a snapshot, for failure messages (the full one can be huge).
fn brief(s: &EngineSnapshot) -> String {
    format!(
        "state {:?}, done {}/{}, failed {}, active {}, queued {}",
        s.state,
        s.files_done,
        s.files_total,
        s.files_failed,
        s.active.len(),
        s.queued_count
    )
}

/// Waits, in virtual time, until `done` holds for the engine's snapshot. Gives up after an hour
/// of virtual time, which the paused clock reaches at once when nothing else is pending.
async fn wait_for(
    engine: &Engine,
    what: &str,
    done: impl Fn(&EngineSnapshot) -> bool,
) -> EngineSnapshot {
    let mut changes = engine.changes();
    let deadline = Instant::now() + Duration::from_secs(3600);
    loop {
        let snapshot = engine.snapshot();
        if done(&snapshot) {
            return snapshot;
        }
        match tokio::time::timeout_at(deadline, changes.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => panic!("the engine went away waiting for {what}"),
            Err(_) => panic!("gave up waiting for {what}: {}", brief(&snapshot)),
        }
    }
}

/// Waits until nothing is queued, running or waiting to be journalled.
async fn wait_idle(engine: &Engine) -> EngineSnapshot {
    wait_for(engine, "the engine to go idle", |_| !engine.is_busy()).await
}

fn active_id(engine: &Engine, path: &str) -> JobId {
    engine
        .snapshot()
        .active
        .iter()
        .find(|view| view.path == path)
        .unwrap_or_else(|| panic!("{path} is not active"))
        .id
}

fn count(journal: &MemoryJournal, state: JobState) -> usize {
    journal
        .records()
        .iter()
        .filter(|r| r.state == state)
        .count()
}

fn offline() -> EngineState {
    EngineState::Offline {
        site: SITE_NAME.into(),
    }
}

/// What a [`Faulty`] site does wrong, beyond what its mock can be told to.
#[derive(Default)]
struct Faults {
    /// Every call fails with a network error before this (virtual) time.
    down_until: Option<Instant>,
    /// The next upload writes these bytes over its local file, then fails with a 502: the file
    /// changed between two tries. With `true` the file keeps its old time; otherwise it is
    /// dated an hour on.
    change_file: Option<(Vec<u8>, bool)>,
    /// This many downloads arrive one byte short.
    damaged: usize,
    /// A file delete takes effect, then its answer never comes.
    hang_after_delete: bool,
}

/// Forwards to a mock, with faults a `MockProvider` can't make on its own, and counts the
/// uploads of each path and the probes.
struct Faulty {
    mock: Arc<MockProvider>,
    faults: Mutex<Faults>,
    /// Upload calls by remote path, counted as they start, whatever they answer.
    uploads: Mutex<HashMap<String, usize>>,
    /// `reachable` calls, whatever they answer.
    probes: AtomicUsize,
}

impl Faulty {
    fn new(mock: Arc<MockProvider>, faults: Faults) -> Self {
        Self {
            mock,
            faults: Mutex::new(faults),
            uploads: Mutex::new(HashMap::new()),
            probes: AtomicUsize::new(0),
        }
    }

    fn down(&self) -> Result<(), ProviderError> {
        let until = self.faults.lock().unwrap().down_until;
        match until {
            Some(until) if Instant::now() < until => {
                Err(ProviderError::Network("connection refused".into()))
            }
            _ => Ok(()),
        }
    }

    fn uploads_of(&self, path: &str) -> usize {
        self.uploads.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn total_uploads(&self) -> usize {
        self.uploads.lock().unwrap().values().sum()
    }

    fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }
}

/// Overwrites `local` with `bytes`, keeping its old time or dating it an hour on.
fn change(local: &Path, bytes: &[u8], keep_time: bool) {
    let old = std::fs::metadata(local).unwrap().modified().unwrap();
    std::fs::write(local, bytes).unwrap();
    let time = if keep_time {
        old
    } else {
        SystemTime::now() + Duration::from_secs(3600)
    };
    std::fs::File::options()
        .write(true)
        .open(local)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

#[async_trait]
impl StorageProvider for Faulty {
    fn capabilities(&self) -> &Capabilities {
        self.mock.capabilities()
    }
    fn endpoint(&self) -> String {
        self.mock.endpoint()
    }
    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        self.down()?;
        self.mock.list(dir).await
    }
    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        *self
            .uploads
            .lock()
            .unwrap()
            .entry(path.as_str().to_owned())
            .or_default() += 1;
        self.down()?;
        let changed = self.faults.lock().unwrap().change_file.take();
        if let Some((bytes, keep_time)) = changed {
            if let UploadSource::File { path: local, .. } = &source {
                change(local, &bytes, keep_time);
            }
            return Err(ProviderError::Server {
                status: 502,
                detail: "bad gateway".into(),
            });
        }
        self.mock.upload(path, source, opts).await
    }
    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        self.down()?;
        let body = self.mock.download(path).await?;
        let damaged = {
            let mut faults = self.faults.lock().unwrap();
            let damaged = faults.damaged > 0;
            faults.damaged = faults.damaged.saturating_sub(1);
            damaged
        };
        if !damaged {
            return Ok(body);
        }
        Ok(Box::pin(body.map(|chunk| {
            chunk.map(|bytes| bytes.slice(..bytes.len().saturating_sub(1)))
        })))
    }
    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        self.down()?;
        let result = self.mock.delete_file(path).await;
        let hang = self.faults.lock().unwrap().hang_after_delete;
        if hang {
            std::future::pending::<()>().await;
        }
        result
    }
    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.down()?;
        self.mock.delete_folder(dir).await
    }
    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.down()?;
        self.mock.create_folder(dir).await
    }
    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        match self.down() {
            Ok(()) => self.mock.delete_batch(paths).await,
            Err(error) => vec![Err(error); paths.len()],
        }
    }
    async fn reachable(&self) -> Result<(), ProviderError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        self.down()?;
        self.mock.reachable().await
    }
}

/// The disk, counting how often a file is hashed.
#[derive(Default)]
struct CountingFiles {
    hashes: AtomicUsize,
}

impl CountingFiles {
    fn hashes(&self) -> usize {
        self.hashes.load(Ordering::SeqCst)
    }
}

impl LocalFiles for CountingFiles {
    fn hash(&self, path: &Path) -> io::Result<FileHashes> {
        self.hashes.fetch_add(1, Ordering::SeqCst);
        hash_file(path)
    }
}

/// A disk in memory that can be full: then writing a download fails with `StorageFull`.
/// Otherwise what is written goes nowhere.
struct FullDisk {
    full: AtomicBool,
}

impl LocalFiles for FullDisk {
    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
    fn create(&self, _path: &Path) -> io::Result<Box<dyn Write + Send>> {
        if self.full.load(Ordering::SeqCst) {
            Ok(Box::new(NoSpace))
        } else {
            Ok(Box::new(io::sink()))
        }
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Ok(())
    }
    fn remove(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// A file on a full disk.
struct NoSpace;

impl Write for NoSpace {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::StorageFull.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A journal in memory whose writes, or reads, can be made to fail. Inserting always works.
#[derive(Default)]
struct Unreliable {
    records: MemoryJournal,
    fail_writes: AtomicBool,
    fail_reads: AtomicBool,
}

impl Unreliable {
    fn check(&self, flag: &AtomicBool) -> Result<(), JournalError> {
        if flag.load(Ordering::SeqCst) {
            Err(JournalError("the disk is read-only".into()))
        } else {
            Ok(())
        }
    }
}

impl Journal for Unreliable {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError> {
        self.records.insert(records)
    }
    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError> {
        self.check(&self.fail_writes)?;
        self.records.set_state(id, state, error)
    }
    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError> {
        self.check(&self.fail_writes)?;
        self.records.set_multipart(id, mp)
    }
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError> {
        self.check(&self.fail_reads)?;
        self.records.unfinished()
    }
    fn clear_finished(&self) -> Result<(), JournalError> {
        self.check(&self.fail_writes)?;
        self.records.clear_finished()
    }
    fn discard_unfinished(&self) -> Result<(), JournalError> {
        self.check(&self.fail_writes)?;
        self.records.discard_unfinished()
    }
}

/// The site can't be reached for 30 s: its lane goes offline, nothing is retried or fails
/// meanwhile, the site is probed every 5 s, and everything goes once a probe gets an answer.
#[tokio::test(start_paused = true)]
async fn offline_pauses_and_resumes_without_failures() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(4));
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.set_latency(MockOp::Upload, Duration::from_millis(100));
    let start = Instant::now();
    let site = Arc::new(Faulty::new(
        mock.clone(),
        Faults {
            down_until: Some(start + Duration::from_secs(30)),
            ..Faults::default()
        },
    ));
    engine
        .submit(batch(site.clone(), upload_jobs(dir.path(), 20, 10)))
        .unwrap();

    // The first four tries fail at once: three errors within 10 s take the lane offline.
    wait_for(&engine, "the lane to go offline", |s| s.state == offline()).await;
    assert_eq!(start.elapsed(), Duration::ZERO);

    tokio::time::sleep_until(start + secs(22.5)).await;
    let s = engine.snapshot();
    assert_eq!(s.state, offline());
    assert_eq!(
        (s.files_done, s.files_failed, s.queued_count),
        (0, 0, 20),
        "{}",
        brief(&s)
    );
    assert!(s.active.is_empty() && s.failed.is_empty());
    assert_eq!(s.eta, None);
    assert_eq!(site.total_uploads(), 4, "no tries while offline");
    assert_eq!(site.probes(), 4, "a probe every 5 s");
    assert_eq!(count(&journal, JobState::Failed), 0);

    let s = wait_idle(&engine).await;
    let took = start.elapsed();
    assert!(took >= secs(30.0) && took <= secs(31.0), "{took:?}");
    assert_eq!((s.files_done, s.files_failed), (20, 0), "{}", brief(&s));
    assert_eq!(s.state, EngineState::Idle);
    assert_eq!(site.probes(), 6, "the sixth, at 30 s, got an answer");
    assert_eq!(
        mock.calls(MockOp::Upload),
        20,
        "each once, after the outage"
    );
    assert!(journal.unfinished().unwrap().is_empty());
}

/// Engine A is dropped halfway through 200 uploads (the app crashed); engine B, on the same
/// journal, resumes what A left: nothing done is sent again.
#[tokio::test(start_paused = true)]
async fn crash_then_resume_sends_only_unfinished() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.set_latency(MockOp::Upload, Duration::from_secs(1));
    let site = Arc::new(Faulty::new(mock.clone(), Faults::default()));
    let jobs = upload_jobs(dir.path(), 200, 10);
    let paths: Vec<String> = jobs.iter().map(remote_of).collect();

    let a = engine_on(journal.clone(), with(8));
    let start = Instant::now();
    a.submit(batch(site.clone(), jobs)).unwrap();
    // 12 rounds of 8 are done and the 13th is under way.
    tokio::time::sleep_until(start + secs(12.5)).await;
    assert_eq!(a.snapshot().files_done, 96);
    drop(a);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let done_before: HashSet<String> = journal
        .records()
        .iter()
        .filter(|r| r.state == JobState::Done)
        .map(|r| remote_of(&r.job))
        .collect();
    assert_eq!(done_before.len(), 96);
    assert_eq!(journal.unfinished().unwrap().len(), 104);
    assert_eq!(site.total_uploads(), 104, "96 done and 8 cut short");

    let b = engine_on(journal.clone(), with(8));
    assert_eq!(b.unfinished_count(), 104);
    assert_eq!(b.resume_from_journal(&only_site(site.clone())), 104);
    assert_eq!(b.unfinished_count(), 0, "B holds them all now");
    let s = wait_idle(&b).await;
    assert_eq!((s.files_done, s.files_failed), (104, 0), "{}", brief(&s));
    assert_eq!(s.site, SITE_NAME);

    for path in &paths {
        let sent = site.uploads_of(path);
        assert!(sent >= 1, "{path} was never sent");
        if done_before.contains(path) {
            assert_eq!(sent, 1, "{path} was done before the crash");
        }
        assert!(mock.contents(&p(path)).is_some(), "{path}");
    }
    assert!(site.total_uploads() <= 200 + 8, "{}", site.total_uploads());
    assert!(journal.unfinished().unwrap().is_empty());
    assert_eq!(count(&journal, JobState::Done), 200);
}

/// Ruling Q47: on resume, the records of a site that no longer exists are marked Cancelled, so
/// they aren't offered again; the rest, failed ones too, are sent again.
#[tokio::test(start_paused = true)]
async fn resume_skips_sites_that_no_longer_exist() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    let removed = SiteId("site-removed".into());
    let mut jobs = upload_jobs(dir.path(), 5, 10);
    let theirs = jobs.split_off(3);
    let mut records = queued_records(&site(), jobs);
    records.extend(queued_records(&removed, theirs));
    journal.insert(&mut records).unwrap();
    // One of the site's jobs had failed before the crash.
    journal
        .set_state(2, JobState::Failed, Some("No connection"))
        .unwrap();

    let engine = engine_on(journal.clone(), TransferSettings::default());
    assert_eq!(engine.unfinished_count(), 5);
    let mock: Arc<dyn StorageProvider> = Arc::new(MockProvider::new(Capabilities::default()));
    let asked = Mutex::new(Vec::new());
    let lookup = |id: &SiteId| -> Option<SiteInfo> {
        asked.lock().unwrap().push(id.clone());
        only_site(mock.clone())(id)
    };
    assert_eq!(engine.resume_from_journal(&lookup), 3);
    assert_eq!(
        *asked.lock().unwrap(),
        [site(), removed.clone()],
        "each site is looked up once"
    );
    // What the engine holds isn't offered again.
    assert_eq!(engine.resume_from_journal(&lookup), 0);

    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (3, 0), "{}", brief(&s));
    let outcome: Vec<(SiteId, JobState, Option<String>)> = journal
        .records()
        .into_iter()
        .map(|r| (r.site, r.state, r.error))
        .collect();
    assert_eq!(
        outcome,
        [
            (site(), JobState::Done, None),
            (site(), JobState::Done, None),
            (site(), JobState::Done, None),
            (removed.clone(), JobState::Cancelled, None),
            (removed, JobState::Cancelled, None),
        ]
    );
    assert_eq!(engine.unfinished_count(), 0);
    assert!(journal.unfinished().unwrap().is_empty());
}

/// A checksum mismatch hashes the file again and sends it once more; a second one fails the
/// job as damaged twice. A download that arrives damaged is fetched once more too (Q27).
#[tokio::test(start_paused = true)]
async fn checksum_mismatch_rehashes_and_retries_once() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(1));
    let files = Arc::new(CountingFiles::default());
    engine.set_local_files(files.clone());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));

    mock.fail_next(MockOp::Upload, ProviderError::ChecksumMismatch, 1);
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 1, 100)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (1, 0), "{}", brief(&s));
    assert_eq!(mock.calls(MockOp::Upload), 2);
    assert_eq!(files.hashes(), 2, "hashed again before the second try");
    assert_eq!(
        mock.contents(&p("up/f0.txt")).as_deref(),
        Some(content(0, 100).as_slice())
    );

    mock.fail_next(MockOp::Upload, ProviderError::ChecksumMismatch, 2);
    let twice = dir.path().join("twice.txt");
    std::fs::write(&twice, b"twice").unwrap();
    engine
        .submit(batch(
            mock.clone(),
            vec![Job::Upload {
                local: twice,
                remote: p("up/twice.txt"),
                size: 5,
            }],
        ))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (0, 1), "{}", brief(&s));
    assert_eq!(s.failed[0].error.as_deref(), Some(messages::DAMAGED_TWICE));
    assert_eq!(
        messages::DAMAGED_TWICE,
        "The file arrived damaged twice (checksum mismatch)."
    );
    assert_eq!(mock.calls(MockOp::Upload), 4, "no third try");
    assert_eq!(files.hashes(), 4);
    assert_eq!(mock.contents(&p("up/twice.txt")), None);
    assert_eq!(journal.records()[1].state, JobState::Failed);

    let photo = content(9, 5000);
    let site = Arc::new(Faulty::new(
        Arc::new(MockProvider::new(Capabilities::default()).with_file("photo.jpg", &photo)),
        Faults {
            damaged: 1,
            ..Faults::default()
        },
    ));
    let target = dir.path().join("photo.jpg");
    let checksums = HashMap::from([(
        p("photo.jpg"),
        Checksum {
            kind: ChecksumKind::Sha256,
            hex: hex::encode(Sha256::digest(&photo)),
        },
    )]);
    engine
        .submit(Batch {
            checksums,
            ..batch(
                site.clone(),
                vec![download("photo.jpg", target.clone(), 5000)],
            )
        })
        .unwrap();
    let s = wait_idle(&engine).await;
    // The upload damaged twice is still failed; the download isn't.
    assert_eq!((s.files_done, s.files_failed), (1, 1), "{}", brief(&s));
    assert_eq!(s.failed[0].path, "up/twice.txt");
    assert_eq!(site.mock.calls(MockOp::Download), 2);
    assert_eq!(std::fs::read(&target).unwrap(), photo);
}

/// A file that changed between two tries (its size, or only its time) is hashed again before
/// it is sent, so what is sent and its checksum agree: no mismatch on the way.
#[tokio::test(start_paused = true)]
async fn changed_file_is_rehashed_before_sending() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings::default());
    let files = Arc::new(CountingFiles::default());
    engine.set_local_files(files.clone());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    let site = Arc::new(Faulty::new(mock.clone(), Faults::default()));

    // Grown but dated as before; then the same size, but dated later.
    for (name, draft, edited, keep_time) in [
        (
            "grown.html",
            &b"first draft"[..],
            &b"edited while it waited"[..],
            true,
        ),
        ("same.html", &b"first draft"[..], &b"final draft"[..], false),
    ] {
        let local = dir.path().join(name);
        std::fs::write(&local, draft).unwrap();
        site.faults.lock().unwrap().change_file = Some((edited.to_vec(), keep_time));
        let hashed = files.hashes();
        engine
            .submit(batch(
                site.clone(),
                vec![Job::Upload {
                    local,
                    remote: p(name),
                    size: draft.len() as u64,
                }],
            ))
            .unwrap();
        let s = wait_idle(&engine).await;
        assert_eq!(
            (s.files_done, s.files_failed),
            (1, 0),
            "{name}: {}",
            brief(&s)
        );
        // The first try met the 502; the second looked again, hashed the new contents and
        // sent them. Sent with the old digest, the mock would have refused a third time.
        assert_eq!(site.uploads_of(name), 2, "{name}");
        assert_eq!(files.hashes() - hashed, 2, "{name}");
        assert_eq!(mock.contents(&p(name)).as_deref(), Some(edited), "{name}");
        let len = edited.len() as u64;
        assert_eq!((s.bytes_done, s.bytes_total), (len, len), "{name}");
    }
}

/// Holds `path` open the way another program can, sharing it with no one, and returns the
/// handle with what the engine should say when it can't read it. Windows only.
#[cfg(windows)]
fn lock(path: &Path) -> Option<(std::fs::File, String)> {
    use std::os::windows::fs::OpenOptionsExt;

    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .unwrap();
    let why = messages::local_file(path, &std::fs::File::open(path).unwrap_err());
    Some((held, why))
}

#[cfg(not(windows))]
fn lock(_path: &Path) -> Option<(std::fs::File, String)> {
    None
}

/// A local file that can't be read (gone, or locked by another program) fails its job with the
/// system's message; the other jobs go on.
#[tokio::test(start_paused = true)]
async fn locked_file_fails_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(1));
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    let jobs = upload_jobs(dir.path(), 4, 10);
    let gone = dir.path().join("f1.txt");
    std::fs::remove_file(&gone).unwrap();
    let mut expected = vec![(
        "up/f1.txt".to_owned(),
        messages::local_file(&gone, &std::fs::metadata(&gone).unwrap_err()),
    )];
    let locked = lock(&dir.path().join("f2.txt"));
    expected.extend(
        locked
            .as_ref()
            .map(|(_, why)| ("up/f2.txt".to_owned(), why.clone())),
    );

    engine.submit(batch(mock.clone(), jobs)).unwrap();
    let s = wait_idle(&engine).await;
    drop(locked);
    let failed: Vec<(String, String)> = s
        .failed
        .iter()
        .map(|view| (view.path.clone(), view.error.clone().unwrap_or_default()))
        .collect();
    assert_eq!(failed, expected);
    assert_eq!(s.files_done as usize, 4 - expected.len(), "{}", brief(&s));
    for i in 0..4 {
        let path = format!("up/f{i}.txt");
        let failed = expected.iter().any(|(failed, _)| *failed == path);
        assert_eq!(mock.contents(&p(&path)).is_some(), !failed, "{path}");
    }
    let reasons: Vec<Option<String>> = journal
        .records()
        .into_iter()
        .filter(|r| r.state == JobState::Failed)
        .map(|r| r.error)
        .collect();
    let expected: Vec<Option<String>> = expected.into_iter().map(|(_, why)| Some(why)).collect();
    assert_eq!(reasons, expected);
}

/// A download that finds its drive full stops everything, with the drive and the download bytes
/// still to come on it, and nothing fails; resuming carries on.
#[tokio::test(start_paused = true)]
async fn disk_full_pauses_with_drive_and_bytes() {
    let (engine, journal) = engine(with(1));
    let disk = Arc::new(FullDisk {
        full: AtomicBool::new(true),
    });
    engine.set_local_files(disk.clone());
    let mock = Arc::new(
        MockProvider::new(Capabilities::default())
            .with_file("a.bin", &[1; 1000])
            .with_file("b.bin", &[2; 2000])
            .with_file("c.bin", &[3; 4000])
            .with_file("d.bin", &[4; 3000]),
    );
    // `c.bin` goes to another drive (Windows): it needs nothing on the full one.
    #[cfg(windows)]
    let (full, other, drive, needed) = (r"Q:\downloads", r"R:\elsewhere", "Q:", 6000);
    #[cfg(not(windows))]
    let (full, other, drive, needed) = ("/downloads", "/elsewhere", "/", 10_000);
    let (full, other) = (PathBuf::from(full), PathBuf::from(other));
    let jobs = vec![
        download("a.bin", full.join("a.bin"), 1000),
        download("b.bin", full.join("b.bin"), 2000),
        download("c.bin", other.join("c.bin"), 4000),
        download("d.bin", full.join("d.bin"), 3000),
    ];
    engine.submit(batch(mock.clone(), jobs)).unwrap();
    let s = wait_for(&engine, "the full disk", |s| {
        matches!(s.state, EngineState::DiskFull { .. }) && s.active.is_empty()
    })
    .await;
    assert_eq!(
        s.state,
        EngineState::DiskFull {
            drive: drive.into(),
            needed,
        }
    );
    assert_eq!(
        (s.files_done, s.files_failed, s.queued_count),
        (0, 0, 4),
        "{}",
        brief(&s)
    );
    assert_eq!(s.queued[0].path, "a.bin", "back at the front");

    // Nothing starts until the user resumes.
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(mock.calls(MockOp::Download), 1);
    assert_eq!(engine.snapshot().state, s.state);
    assert_eq!(count(&journal, JobState::Failed), 0);

    disk.full.store(false, Ordering::SeqCst);
    engine.resume_all();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (4, 0), "{}", brief(&s));
    assert_eq!(s.state, EngineState::Idle);
    assert_eq!(mock.calls(MockOp::Download), 5);
}

/// The user chose not to resume: the jobs a crash left go. An engine with work of its own keeps
/// that work journalled.
#[tokio::test(start_paused = true)]
async fn discard_journal_clears_unfinished() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    let mut left = queued_records(&site(), upload_jobs(&dir.path().join("left"), 3, 10));
    journal.insert(&mut left).unwrap();
    journal
        .set_state(3, JobState::Failed, Some("No connection"))
        .unwrap();
    let engine = engine_on(journal.clone(), TransferSettings::default());
    let mock: Arc<dyn StorageProvider> = Arc::new(MockProvider::new(Capabilities::default()));
    assert_eq!(engine.unfinished_count(), 3);
    engine.discard_journal();
    assert_eq!(engine.unfinished_count(), 0);
    assert!(journal.unfinished().unwrap().is_empty());
    assert_eq!(engine.resume_from_journal(&only_site(mock.clone())), 0);

    // More left over, while the engine holds two jobs of its own (paused, so still queued).
    let mut more = queued_records(&site(), upload_jobs(&dir.path().join("more"), 2, 10));
    journal.insert(&mut more).unwrap();
    engine.pause_all();
    let own = engine
        .submit(batch(
            mock.clone(),
            upload_jobs(&dir.path().join("own"), 2, 10),
        ))
        .unwrap();
    assert_eq!(engine.unfinished_count(), 2);
    engine.discard_journal();
    assert_eq!(engine.unfinished_count(), 0);
    let unfinished = journal.unfinished().unwrap();
    assert_eq!(unfinished.len(), 2);
    assert!(unfinished.iter().all(|r| r.batch == own), "only its own");
    let leftover_ids: Vec<i64> = more.iter().map(|r| r.id).collect();
    assert!(
        journal
            .records()
            .iter()
            .filter(|r| leftover_ids.contains(&r.id))
            .all(|r| r.state == JobState::Cancelled)
    );

    engine.resume_all();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (2, 0), "{}", brief(&s));
    assert!(journal.unfinished().unwrap().is_empty());
}

/// Ruling Q37: a transfer cancelled while it waited for bandwidth gives back what it booked,
/// so the next one isn't held up by bytes that never went.
#[tokio::test(start_paused = true)]
async fn a_cancelled_transfer_gives_its_bandwidth_back() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings {
        bandwidth_limit: Some(1000),
        ..with(1)
    });
    let mock = Arc::new(
        MockProvider::new(Capabilities::default())
            .with_file("big.bin", &[7; 10_000])
            .with_file("small.bin", &[8; 1000]),
    );
    let start = Instant::now();
    engine
        .submit(batch(
            mock.clone(),
            vec![
                download("big.bin", dir.path().join("big.bin"), 10_000),
                download("small.bin", dir.path().join("small.bin"), 1000),
            ],
        ))
        .unwrap();
    tokio::time::sleep_until(start + secs(2.0)).await;
    engine.cancel(active_id(&engine, "big.bin"));
    let s = wait_idle(&engine).await;
    // The small file takes its own second at 1,000 bytes/s, not the 8 s the big one booked.
    let took = start.elapsed();
    assert!(took <= secs(3.0), "{took:?}");
    assert_eq!((s.files_done, s.files_total), (1, 1), "{}", brief(&s));
    assert_eq!(
        std::fs::read(dir.path().join("small.bin")).unwrap(),
        [8; 1000]
    );
    assert!(!dir.path().join("big.bin").exists());
}

/// Ruling Q37: a new bandwidth limit applies at once, to transfers already waiting under the
/// old one.
#[tokio::test(start_paused = true)]
async fn a_new_bandwidth_limit_applies_to_transfers_under_way() {
    let dir = tempfile::tempdir().unwrap();
    let limited = TransferSettings {
        bandwidth_limit: Some(1000),
        ..TransferSettings::default()
    };
    let (engine, _journal) = engine(limited.clone());
    let mock =
        Arc::new(MockProvider::new(Capabilities::default()).with_file("big.bin", &[7; 10_000]));
    let start = Instant::now();
    engine
        .submit(batch(
            mock.clone(),
            vec![download("big.bin", dir.path().join("big.bin"), 10_000)],
        ))
        .unwrap();
    tokio::time::sleep_until(start + secs(1.0)).await;
    engine.set_settings(TransferSettings {
        bandwidth_limit: None,
        ..limited
    });
    let s = wait_idle(&engine).await;
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(1),
        "not the 10 s booked"
    );
    assert_eq!(s.files_done, 1);
}

/// Ruling Q37: journal writes that fail are counted in the snapshot, and the jobs themselves
/// carry on.
#[tokio::test(start_paused = true)]
async fn journal_write_errors_show_in_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Unreliable::default());
    let engine = engine_on(journal.clone(), TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities::default()).with_file("old.txt", b"old"));
    assert_eq!(engine.snapshot().journal_errors, 0);

    journal.fail_writes.store(true, Ordering::SeqCst);
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 3, 10)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (3, 0), "{}", brief(&s));
    assert_eq!(s.journal_errors, 3, "three Done records not written");
    assert_eq!(journal.records.unfinished().unwrap().len(), 3);

    // A delete writes twice: marked possibly sent as it goes, then done.
    engine
        .submit(batch(mock.clone(), vec![delete("old.txt")]))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 1);
    assert_eq!(s.journal_errors, 5);

    // A journal that can't be read offers nothing to resume, and that is counted too.
    journal.fail_reads.store(true, Ordering::SeqCst);
    assert_eq!(engine.unfinished_count(), 0);
    let lookup = only_site(mock.clone());
    assert_eq!(engine.resume_from_journal(&lookup), 0);
    assert_eq!(engine.snapshot().journal_errors, 7);
}

/// Ruling Q37 (ledger 179): a delete is marked in the journal as possibly sent when it goes
/// out. Resumed after a crash, such a delete takes `NotFound` as done: its first request did
/// the work. A delete never sent still reports `NotFound`.
#[tokio::test(start_paused = true)]
async fn a_resumed_delete_that_may_have_been_sent_treats_not_found_as_done() {
    for batch_delete_max in [1, 10] {
        let caps = Capabilities {
            batch_delete_max,
            ..Capabilities::default()
        };
        let journal = Arc::new(MemoryJournal::new());
        // The delete reaches the server, and the app goes away before the answer comes back.
        let mock = Arc::new(MockProvider::new(caps.clone()).with_file("gone.txt", b"g"));
        let hangs = Arc::new(Faulty::new(
            mock.clone(),
            Faults {
                hang_after_delete: true,
                ..Faults::default()
            },
        ));
        let a = engine_on(journal.clone(), TransferSettings::default());
        a.submit(batch(hangs, vec![delete("gone.txt")])).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(mock.calls(MockOp::DeleteFile), 1);
        assert_eq!(mock.contents(&p("gone.txt")), None);
        drop(a);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let sent = journal.records()[0].clone();
        assert_eq!(sent.state, JobState::Queued);
        assert!(sent.multipart.is_some(), "marked as possibly sent");
        let mut never = queued_records(&site(), vec![delete("never.txt")]);
        journal.insert(&mut never).unwrap();

        let b = engine_on(journal.clone(), TransferSettings::default());
        let fresh = Arc::new(MockProvider::new(caps));
        assert_eq!(b.resume_from_journal(&only_site(fresh.clone())), 2);
        let s = wait_idle(&b).await;
        assert_eq!(
            (s.files_done, s.files_failed),
            (1, 1),
            "batch_delete_max {batch_delete_max}: {}",
            brief(&s)
        );
        assert_eq!(s.failed[0].path, "never.txt");
        assert_eq!(s.failed[0].error, Some(describe(&ProviderError::NotFound)));
        let states: Vec<JobState> = journal.records().iter().map(|r| r.state).collect();
        assert_eq!(states, [JobState::Done, JobState::Failed]);
        if batch_delete_max > 1 {
            assert_eq!(fresh.calls(MockOp::DeleteBatch), 1, "both in one request");
        } else {
            assert_eq!(fresh.calls(MockOp::DeleteFile), 2);
        }
    }
}

/// A job whose requests never get through while the site answers its probes uses up its
/// retries on network errors, sending the lane offline each time; the third time it fails,
/// with the `describe` hook's words, instead of holding the lane forever.
#[tokio::test(start_paused = true)]
async fn a_job_the_site_never_lets_through_fails_after_three_strikes() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings {
        retries: 0,
        ..TransferSettings::default()
    });
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.fail_next(MockOp::Upload, ProviderError::Network("reset".into()), 3);
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 1, 10)))
        .unwrap();
    let s = wait_for(&engine, "the failure", |s| s.files_failed == 1).await;
    // Offline at 0 and 5 s; the probes at 5 and 10 s answer; the third strike fails it.
    assert_eq!(start.elapsed(), Duration::from_secs(10));
    assert_eq!(mock.calls(MockOp::Upload), 3);
    assert_eq!(mock.calls(MockOp::Reachable), 2);
    assert_eq!(s.failed[0].error.as_deref(), Some("No connection"));
    let s = wait_idle(&engine).await;
    assert_eq!(s.state, EngineState::Idle);
    let record = &journal.records()[0];
    assert_eq!(record.state, JobState::Failed);
    assert_eq!(record.error.as_deref(), Some("No connection"));
}
