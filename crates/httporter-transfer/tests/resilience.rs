//! Resume after a crash, offline pauses, damaged or changing files and a full disk (Task 14),
//! and uploads a crash cut off on a service that deletes them late (ruling Q55),
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
    MultipartProgress, ProviderError, Rechecked, RemotePath, SiteId, StorageProvider,
    TransferSettings, UploadOptions, UploadSource,
};
use httporter_platform::SqliteJournal;
use httporter_transfer::{
    Batch, CHECK_AFTER, Engine, FileHashes, LocalFiles, MAX_CHECKS, SiteInfo, hash_file, messages,
};
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

/// The words of an upload's "possibly sent" mark in the journal (ruling Q55), part of its file
/// format.
const UPLOAD_SENT: &str = "httporter:upload-sent";

/// The mark itself, as the engine writes it.
fn upload_sent() -> MultipartProgress {
    MultipartProgress {
        upload_id: UPLOAD_SENT.into(),
        part_size: 0,
        parts: Vec::new(),
        file: None,
    }
}

/// A service that can delete a cut-off upload's path late, as Bunny Storage does (ruling Q55).
fn deletes_late() -> Capabilities {
    Capabilities {
        cut_uploads_delete_late: true,
        ..Capabilities::default()
    }
}

/// The remote paths of the unfinished records marked as possibly sent uploads, in id order.
fn marked(journal: &MemoryJournal) -> Vec<String> {
    journal
        .unfinished()
        .unwrap()
        .iter()
        .filter(|r| matches!(r.job, Job::Upload { .. }) && r.multipart == Some(upload_sent()))
        .map(|r| remote_of(&r.job))
        .collect()
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
    /// Every upload of this remote path gets no answer (a proxy that resets this one file),
    /// while everything else, the probe included, gets through.
    stubborn: Option<String>,
    /// Bunny Storage ending an upload that was cut off (ruling Q55): the next this many uploads
    /// of each of these paths that succeed are deleted [`Self::delete_after`] later, as the
    /// service deletes the path when it gets round to the cut-off one.
    delete_late: HashMap<String, usize>,
    delete_after: Duration,
    /// The journal to look at as each upload starts: one sent before the journal had its
    /// "possibly sent" mark is noted (ruling Q55).
    journal: Option<Arc<MemoryJournal>>,
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
    /// Uploads that started while [`Faults::journal`] had no "possibly sent" mark for them.
    unmarked: Mutex<Vec<String>>,
}

impl Faulty {
    fn new(mock: Arc<MockProvider>, faults: Faults) -> Self {
        Self {
            mock,
            faults: Mutex::new(faults),
            uploads: Mutex::new(HashMap::new()),
            probes: AtomicUsize::new(0),
            unmarked: Mutex::new(Vec::new()),
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
        let journal = self.faults.lock().unwrap().journal.clone();
        if let Some(journal) = journal
            && !marked(&journal).contains(&path.as_str().to_owned())
        {
            self.unmarked.lock().unwrap().push(path.as_str().to_owned());
        }
        self.down()?;
        let stubborn = self.faults.lock().unwrap().stubborn.clone();
        if stubborn.as_deref() == Some(path.as_str()) {
            return Err(ProviderError::Network("connection reset".into()));
        }
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
        let stored = self.mock.upload(path, source, opts).await;
        let late = {
            let mut faults = self.faults.lock().unwrap();
            let after = faults.delete_after;
            match faults.delete_late.get_mut(path.as_str()) {
                Some(left) if stored.is_ok() && *left > 0 => {
                    *left -= 1;
                    Some(after)
                }
                _ => None,
            }
        };
        if let Some(after) = late {
            let (mock, path) = (self.mock.clone(), path.clone());
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                let _ = mock.delete_file(&path).await;
            });
        }
        stored
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

    // The first three tries fail at once: three errors within 10 s take the lane offline. The
    // fourth transfer, already under way, is left to try (review N1); its error meets the lane
    // offline, and it goes back to the queue as it was.
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
    engine.discard_journal(&only_site(mock.clone()));
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
    engine.discard_journal(&only_site(mock.clone()));
    // Those go through the journal writer, in chunks, which closing waits for (review N2).
    assert!(engine.pending_writes() > 0);
    wait_for(&engine, "the discards to be written", |_| {
        engine.pending_writes() == 0
    })
    .await;
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

/// Ruling Q50 with the default settings (5 retries): a job whose requests never get through
/// while the site answers its probes takes the lane offline itself (three errors within 10 s,
/// before its retries run out). Each such offline→online cycle is a strike, and the third fails
/// it with the network reason instead of looping forever. Retry starts it again with none.
#[tokio::test(start_paused = true)]
async fn a_stubborn_job_fails_after_three_strikes_with_default_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    assert_eq!(TransferSettings::default().retries, 5);
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.fail_next(MockOp::Upload, ProviderError::Network("reset".into()), 1000);
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 1, 10)))
        .unwrap();

    let s = wait_for(&engine, "the failure", |s| s.files_failed == 1).await;
    // Tries at 0, 1 and 3 s (offline), 8, 9 and 11 s (offline), 16, 17 and 19 s: the third
    // strike.
    assert_eq!(start.elapsed(), Duration::from_secs(19));
    assert_eq!(mock.calls(MockOp::Upload), 9);
    assert_eq!(mock.calls(MockOp::Reachable), 2);
    assert_eq!(s.failed[0].error.as_deref(), Some("No connection"));
    let s = wait_idle(&engine).await;
    assert_eq!(s.state, EngineState::Idle, "the site itself is fine");
    assert_eq!(journal.records()[0].state, JobState::Failed);
    assert_eq!(journal.records()[0].error.as_deref(), Some("No connection"));

    // Retry: no strikes are left over, so the first cycle sends it back to the queue.
    let failed = s.failed[0].id;
    engine.retry(&[failed]);
    let s = wait_for(&engine, "the lane to go offline again", |s| {
        s.state == offline() && s.active.is_empty()
    })
    .await;
    assert_eq!((s.files_failed, s.queued_count), (0, 1), "{}", brief(&s));
    let s = wait_for(&engine, "the second failure", |s| s.files_failed == 1).await;
    assert_eq!(mock.calls(MockOp::Upload), 18, "{}", brief(&s));
}

/// Ruling Q50 for file deletes sent together: a batch whose requests never get through takes
/// the lane offline, a strike for each of its files, and the third strike fails them.
#[tokio::test(start_paused = true)]
async fn a_stubborn_batch_delete_fails_after_three_strikes_with_default_retries() {
    let caps = Capabilities {
        batch_delete_max: 10,
        ..Capabilities::default()
    };
    let (engine, journal) = engine(TransferSettings::default());
    let mock = Arc::new(
        MockProvider::new(caps)
            .with_file("a.txt", b"a")
            .with_file("b.txt", b"b"),
    );
    mock.fail_next(
        MockOp::DeleteBatch,
        ProviderError::Network("reset".into()),
        1000,
    );
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), vec![delete("a.txt"), delete("b.txt")]))
        .unwrap();

    let s = wait_for(&engine, "the failures", |s| s.files_failed == 2).await;
    assert_eq!(start.elapsed(), Duration::from_secs(19));
    assert_eq!(mock.calls(MockOp::DeleteBatch), 9, "one request a try");
    assert!(
        s.failed
            .iter()
            .all(|view| view.error.as_deref() == Some("No connection")),
        "{:?}",
        s.failed
    );
    let s = wait_idle(&engine).await;
    assert_eq!(s.state, EngineState::Idle);
    assert_eq!(count(&journal, JobState::Failed), 2);
    assert!(mock.contents(&p("a.txt")).is_some() && mock.contents(&p("b.txt")).is_some());
}

/// Review M1: when a lane's site is out of reach, the transfers in flight on it go back to the
/// queue, their attempts unused, instead of sitting on dead connections until they time out.
/// Since review N1 that is when the first probe gets no answer either, 5 s after the lane went
/// offline: a site that answers it takes them back at once, untouched.
#[tokio::test(start_paused = true)]
async fn going_offline_sends_the_work_in_flight_back_to_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(4));
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.set_latency(MockOp::Upload, Duration::from_secs(60));
    let site = Arc::new(Faulty::new(mock.clone(), Faults::default()));
    let mut jobs = upload_jobs(dir.path(), 4, 10);
    let rest = jobs.split_off(1);
    let start = Instant::now();
    engine.submit(batch(site.clone(), jobs)).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(engine.snapshot().active_count, 1);

    // The connection drops: the next requests get no answer, three of them within 10 s.
    site.faults.lock().unwrap().down_until = Some(Instant::now() + Duration::from_secs(20));
    engine.submit(batch(site.clone(), rest)).unwrap();
    let s = wait_for(&engine, "the lane to go offline", |s| s.state == offline()).await;
    assert_eq!(start.elapsed(), Duration::from_secs(1));
    assert!(
        s.active.iter().any(|view| view.path == "up/f0.txt"),
        "left alone until a probe fails: {}",
        brief(&s)
    );
    let s = wait_for(&engine, "the lane to go offline, nothing in flight", |s| {
        s.state == offline() && s.active.is_empty()
    })
    .await;
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(6),
        "at the first probe that got no answer, not after 60 s"
    );
    assert_eq!(site.probes(), 1);
    assert_eq!((s.queued_count, s.files_failed), (4, 0), "{}", brief(&s));
    assert_eq!(mock.max_in_flight(MockOp::Upload), 1);

    // Back online at 21 s: all four go, the interrupted one again from the start.
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (4, 0), "{}", brief(&s));
    assert_eq!(site.uploads_of("up/f0.txt"), 2);
    assert_eq!(count(&journal, JobState::Done), 4);
}

/// Review N1: a file a proxy resets every time takes its lane offline twice before it strikes
/// out (ruling Q50), while the site answers every probe. The upload in flight beside it is left
/// alone through both spells: sent once, never restarted from byte 0 (Bunny can't resume one,
/// nor can an S3 single PUT), and done.
#[tokio::test(start_paused = true)]
async fn a_stubborn_job_leaves_the_upload_beside_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(4));
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.set_latency(MockOp::Upload, Duration::from_secs(60));
    let site = Arc::new(Faulty::new(
        mock.clone(),
        Faults {
            stubborn: Some("up/f1.txt".into()),
            ..Faults::default()
        },
    ));
    let mut jobs = upload_jobs(dir.path(), 2, 10);
    let stubborn = jobs.split_off(1);
    let start = Instant::now();
    engine.submit(batch(site.clone(), jobs)).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(engine.snapshot().active_count, 1);
    engine.submit(batch(site.clone(), stubborn)).unwrap();

    let s = wait_for(&engine, "the stubborn job to fail", |s| s.files_failed == 1).await;
    // Tries at 1, 2 and 4 s (offline), 9, 10 and 12 s (offline), 17, 18 and 20 s: the third
    // strike. Both probes, at 9 and 17 s, got an answer.
    assert_eq!(start.elapsed(), Duration::from_secs(20));
    assert_eq!(site.uploads_of("up/f1.txt"), 9);
    assert_eq!(site.probes(), 2);
    assert_eq!(s.failed[0].path, "up/f1.txt");
    assert_eq!(s.failed[0].error.as_deref(), Some("No connection"));
    assert_eq!(
        site.uploads_of("up/f0.txt"),
        1,
        "the upload beside it was never sent again"
    );
    assert_eq!(s.active_count, 1, "{}", brief(&s));

    let s = wait_idle(&engine).await;
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(60),
        "done when first sent"
    );
    assert_eq!((s.files_done, s.files_failed), (1, 1), "{}", brief(&s));
    assert_eq!(site.uploads_of("up/f0.txt"), 1);
    assert_eq!(mock.calls(MockOp::Upload), 1);
    assert_eq!(count(&journal, JobState::Done), 1);
    assert_eq!(count(&journal, JobState::Failed), 1);
}

/// A journal in memory that counts how its updates arrive: one at a time, or together.
#[derive(Default)]
struct Chunked {
    records: MemoryJournal,
    singles: AtomicUsize,
    applies: AtomicUsize,
    largest: AtomicUsize,
}

impl Journal for Chunked {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError> {
        self.records.insert(records)
    }
    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError> {
        self.singles.fetch_add(1, Ordering::SeqCst);
        self.records.set_state(id, state, error)
    }
    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError> {
        self.singles.fetch_add(1, Ordering::SeqCst);
        self.records.set_multipart(id, mp)
    }
    fn apply(&self, updates: &[httporter_core::JournalUpdate]) -> Result<(), JournalError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        self.largest.fetch_max(updates.len(), Ordering::SeqCst);
        self.records.apply(updates)
    }
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError> {
        self.records.unfinished()
    }
    fn clear_finished(&self) -> Result<(), JournalError> {
        self.records.clear_finished()
    }
    fn discard_unfinished(&self) -> Result<(), JournalError> {
        self.records.discard_unfinished()
    }
}

/// Review C1: the journal writer hands its updates to `Journal::apply` in chunks of up to 512
/// (one transaction each for SQLite), so cancelling 2,000 jobs is four writes, not 2,000.
#[tokio::test(start_paused = true)]
async fn cancels_reach_the_journal_in_chunks() {
    let journal = Arc::new(Chunked::default());
    let engine = engine_on(journal.clone(), TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    engine.pause_all();
    let jobs = (0..2000).map(|i| delete(&format!("f{i}.txt"))).collect();
    engine.submit(batch(mock.clone(), jobs)).unwrap();
    engine.cancel_all();
    wait_idle(&engine).await;
    assert_eq!(
        journal.singles.load(Ordering::SeqCst),
        0,
        "never one at a time"
    );
    assert_eq!(journal.applies.load(Ordering::SeqCst), 4);
    assert_eq!(journal.largest.load(Ordering::SeqCst), 512);
    assert!(journal.records.unfinished().unwrap().is_empty());
    assert_eq!(engine.snapshot().journal_errors, 0);
}

// ---- Ruling Q55: uploads a crash cut off, on a service that deletes them late ----

/// Journals `jobs` on `site()` as a crash leaves uploads that were in flight on such a service:
/// queued, marked as possibly sent.
fn left_in_flight(journal: &MemoryJournal, jobs: Vec<Job>) {
    let mut records = queued_records(&site(), jobs);
    journal.insert(&mut records).unwrap();
    for record in &records {
        journal
            .set_multipart(record.id, Some(&upload_sent()))
            .unwrap();
    }
}

/// Acceptance S5: the uploads in flight when the app is killed are marked in the journal. Sent
/// again on resume and answered, they are then deleted by the site as it ends the killed
/// uploads (50 s on; Bunny's live probes put it about a minute after the cut). Each waits,
/// counted done, until its lane has finished and [`CHECK_AFTER`] has passed, is found missing in
/// its folder's one listing, is sent once more and checked again. The uploads that never
/// started are sent once and never looked for, and the journal says done only once checked.
#[tokio::test(start_paused = true)]
async fn uploads_a_crash_cut_off_are_checked_after_the_resume_and_sent_again_if_gone() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    let mock = Arc::new(MockProvider::new(deletes_late()));
    mock.set_latency(MockOp::Upload, Duration::from_secs(1));
    let jobs = upload_jobs(dir.path(), 20, 10);
    let paths: Vec<String> = jobs.iter().map(remote_of).collect();

    let a = engine_on(journal.clone(), with(4));
    let start = Instant::now();
    a.submit(batch(mock.clone(), jobs)).unwrap();
    // Two rounds of 4 are done and the third is under way.
    tokio::time::sleep_until(start + secs(2.5)).await;
    assert_eq!(a.snapshot().files_done, 8);
    drop(a);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let cut = marked(&journal);
    assert_eq!(cut, paths[8..12], "the four in flight are marked");
    assert_eq!(journal.unfinished().unwrap().len(), 12);

    let site = Arc::new(Faulty::new(
        mock.clone(),
        Faults {
            delete_late: cut.iter().map(|path| (path.clone(), 1)).collect(),
            delete_after: Duration::from_secs(50),
            ..Faults::default()
        },
    ));
    let b = engine_on(journal.clone(), with(4));
    assert_eq!(b.resume_from_journal(&only_site(site.clone())), 12);
    let resumed = Instant::now();
    let s = wait_for(&b, "the resumed uploads to be answered", |s| {
        s.files_done == 12 && !b.has_work() && b.pending_writes() == 0
    })
    .await;
    assert!(resumed.elapsed() < secs(5.0), "{:?}", resumed.elapsed());
    assert_eq!((s.files_failed, s.state), (0, EngineState::Idle));
    assert_eq!(
        b.pending_checks(),
        4,
        "the four sent again wait for their check"
    );
    assert_eq!(
        marked(&journal),
        cut,
        "not done in the journal before the check"
    );
    assert_eq!(b.unfinished_count(), 0, "held, not offered to resume");
    assert_eq!(mock.calls(MockOp::List), 0);

    let s = wait_idle(&b).await;
    assert_eq!((s.files_done, s.files_failed), (12, 0), "{}", brief(&s));
    assert_eq!(
        s.rechecked,
        Rechecked {
            checked: 8,
            resent: 4
        },
        "four missing at the first check, four there at the second"
    );
    assert_eq!(mock.calls(MockOp::List), 2, "one listing of up/ per round");
    assert!(
        resumed.elapsed() >= CHECK_AFTER * 2,
        "each round waited the settle delay: {:?}",
        resumed.elapsed()
    );
    for path in &paths {
        assert!(mock.contents(&p(path)).is_some(), "{path} is on the site");
    }
    for path in &paths[8..] {
        let sent = if cut.contains(path) { 2 } else { 1 };
        assert_eq!(site.uploads_of(path), sent, "{path}");
    }
    assert!(journal.unfinished().unwrap().is_empty());
    assert_eq!(count(&journal, JobState::Done), 20);
}

/// A journal in memory on a slow disk: every write takes 200 ms (of real time, on the writer's
/// blocking thread).
struct SlowDisk(Arc<MemoryJournal>);

impl SlowDisk {
    fn wait() {
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Journal for SlowDisk {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError> {
        self.0.insert(records)
    }
    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError> {
        Self::wait();
        self.0.set_state(id, state, error)
    }
    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError> {
        Self::wait();
        self.0.set_multipart(id, mp)
    }
    fn apply(&self, updates: &[httporter_core::JournalUpdate]) -> Result<(), JournalError> {
        Self::wait();
        self.0.apply(updates)
    }
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError> {
        self.0.unfinished()
    }
    fn clear_finished(&self) -> Result<(), JournalError> {
        self.0.clear_finished()
    }
    fn discard_unfinished(&self) -> Result<(), JournalError> {
        self.0.discard_unfinished()
    }
}

/// Ruling Q55: on such a service an upload sent for the first time is marked in the journal
/// before its request goes out (the unit waits for the write, here on a slow disk), and is done
/// as soon as it is answered: never held for a check, never looked for.
#[tokio::test(start_paused = true)]
async fn an_upload_sent_once_is_marked_before_it_goes_and_never_looked_for() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    let mock = Arc::new(MockProvider::new(deletes_late()));
    mock.set_latency(MockOp::Upload, Duration::from_secs(1));
    let site = Arc::new(Faulty::new(
        mock.clone(),
        Faults {
            journal: Some(journal.clone()),
            ..Faults::default()
        },
    ));
    let e = engine_on(Arc::new(SlowDisk(journal.clone())), with(4));
    e.submit(batch(site.clone(), upload_jobs(dir.path(), 10, 10)))
        .unwrap();
    let s = wait_for(&e, "the uploads", |s| s.files_done == 10 && !e.has_work()).await;
    assert_eq!(e.pending_checks(), 0);
    assert_eq!(site.unmarked.lock().unwrap().clone(), Vec::<String>::new());
    tokio::time::sleep(CHECK_AFTER * 2).await;
    let s2 = wait_idle(&e).await;
    assert_eq!((s.files_failed, s2.files_done), (0, 10));
    assert_eq!(s2.rechecked, Rechecked::default());
    assert_eq!(mock.calls(MockOp::List), 0, "nothing was looked for");
    assert_eq!(count(&journal, JobState::Done), 10);
    let records = journal.records();
    assert!(
        records.iter().all(|r| r.multipart == Some(upload_sent())),
        "each was marked before it went"
    );

    // A service that answers for its uploads gets neither the mark nor a check.
    let journal = Arc::new(MemoryJournal::new());
    let plain = Arc::new(MockProvider::new(Capabilities::default()));
    let e = engine_on(journal.clone(), with(4));
    e.submit(batch(
        plain.clone(),
        upload_jobs(&dir.path().join("p"), 4, 10),
    ))
    .unwrap();
    wait_idle(&e).await;
    assert!(journal.records().iter().all(|r| r.multipart.is_none()));
    assert_eq!(plain.calls(MockOp::List), 0);
}

/// Ruling Q55: a file the site keeps deleting after answering for it is sent again after each
/// check that misses it, and fails at the [`MAX_CHECKS`]th, with the engine's words for it.
#[tokio::test(start_paused = true)]
async fn a_file_the_site_keeps_losing_fails_after_the_last_check() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    left_in_flight(&journal, upload_jobs(dir.path(), 1, 10));
    let mock = Arc::new(MockProvider::new(deletes_late()));
    let site = Arc::new(Faulty::new(
        mock.clone(),
        Faults {
            delete_late: [("up/f0.txt".to_owned(), 99)].into(),
            delete_after: Duration::from_secs(50),
            ..Faults::default()
        },
    ));
    let e = engine_on(journal.clone(), TransferSettings::default());
    assert_eq!(e.resume_from_journal(&only_site(site.clone())), 1);
    let s = wait_idle(&e).await;
    assert_eq!((s.files_done, s.files_failed), (0, 1), "{}", brief(&s));
    assert_eq!(
        s.failed[0].error.as_deref(),
        Some(messages::LOST_AFTER_UPLOAD)
    );
    let checks = u64::from(MAX_CHECKS);
    assert_eq!(site.uploads_of("up/f0.txt") as u64, checks);
    assert_eq!(
        s.rechecked,
        Rechecked {
            checked: checks,
            resent: checks - 1
        }
    );
    assert_eq!(journal.records()[0].state, JobState::Failed);

    // Retry starts it afresh: the site keeps it this time.
    site.faults.lock().unwrap().delete_late.clear();
    e.retry_all_failed();
    let s = wait_idle(&e).await;
    assert_eq!((s.files_done, s.files_failed), (1, 0), "{}", brief(&s));
    assert!(mock.contents(&p("up/f0.txt")).is_some());
    assert_eq!(journal.records()[0].state, JobState::Done);
}

/// Ruling Q55: closing doesn't wait for a check, and the quit alert doesn't ask about one. An
/// engine that goes while uploads wait for theirs leaves their records unfinished and marked;
/// the next start sends them again and checks them (here the site had deleted them meanwhile).
#[tokio::test(start_paused = true)]
async fn a_quit_before_the_check_leaves_it_to_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    left_in_flight(&journal, upload_jobs(dir.path(), 2, 10));
    let mock = Arc::new(MockProvider::new(deletes_late()));
    let site = Arc::new(Faulty::new(
        mock.clone(),
        Faults {
            delete_late: [("up/f0.txt".to_owned(), 1), ("up/f1.txt".to_owned(), 1)].into(),
            delete_after: Duration::from_secs(50),
            ..Faults::default()
        },
    ));
    let b = engine_on(journal.clone(), TransferSettings::default());
    assert_eq!(b.resume_from_journal(&only_site(site.clone())), 2);
    wait_for(&b, "the uploads", |s| s.files_done == 2 && !b.has_work()).await;
    assert_eq!(b.pending_checks(), 2);
    assert!(!b.has_work(), "nothing for the quit alert to ask about");
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(mock.contents(&p("up/f0.txt")), None, "deleted late");
    drop(b);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(marked(&journal).len(), 2, "still unfinished and marked");

    let c = engine_on(journal.clone(), TransferSettings::default());
    assert_eq!(c.unfinished_count(), 2);
    assert_eq!(c.resume_from_journal(&only_site(site.clone())), 2);
    let s = wait_idle(&c).await;
    assert_eq!((s.files_done, s.files_failed), (2, 0), "{}", brief(&s));
    assert_eq!(
        s.rechecked,
        Rechecked {
            checked: 2,
            resent: 0
        }
    );
    assert!(mock.contents(&p("up/f0.txt")).is_some());
    assert!(mock.contents(&p("up/f1.txt")).is_some());
    assert_eq!(site.uploads_of("up/f0.txt"), 2);
    assert!(journal.unfinished().unwrap().is_empty());
}

/// Ruling Q55: an upload cut off in this run (Pause All, then Resume) is checked once sent
/// again, as one a crash cut off is.
#[tokio::test(start_paused = true)]
async fn an_upload_a_pause_cut_off_is_checked_once_sent_again() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(MemoryJournal::new());
    let mock = Arc::new(MockProvider::new(deletes_late()));
    mock.set_latency(MockOp::Upload, Duration::from_secs(10));
    let e = engine_on(journal.clone(), with(4));
    e.submit(batch(mock.clone(), upload_jobs(dir.path(), 3, 10)))
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(e.snapshot().active_count, 3);
    e.pause_all();
    tokio::time::sleep(Duration::from_secs(1)).await;
    e.resume_all();
    let s = wait_idle(&e).await;
    assert_eq!((s.files_done, s.files_failed), (3, 0), "{}", brief(&s));
    assert_eq!(
        s.rechecked,
        Rechecked {
            checked: 3,
            resent: 0
        }
    );
    assert_eq!(mock.calls(MockOp::List), 1);
    assert_eq!(count(&journal, JobState::Done), 3);
}

/// Ruling Q55: the "possibly sent" mark is kept in the SQLite journal's multipart column and
/// read back after a reopen. A record without it, as every journal written before has them,
/// still opens and resumes as before: sent once, never looked for.
#[tokio::test(start_paused = true)]
async fn the_upload_mark_survives_sqlite_and_unmarked_records_resume_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("httporter.db");
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::open(&path).unwrap());
    let mut older = queued_records(&site(), upload_jobs(&dir.path().join("old"), 1, 10));
    if let Job::Upload { remote, .. } = &mut older[0].job {
        *remote = p("up/older.txt");
    }
    journal.insert(&mut older).unwrap();
    let mock = Arc::new(MockProvider::new(deletes_late()));
    mock.set_latency(MockOp::Upload, Duration::from_secs(10));
    let a = engine_on(journal.clone(), with(4));
    a.submit(batch(mock.clone(), upload_jobs(dir.path(), 2, 10)))
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    wait_for(&a, "the marks to be written", |s| {
        s.active_count == 2 && a.pending_writes() == 0
    })
    .await;
    drop(a);
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(journal);

    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::open(&path).unwrap());
    let left = journal.unfinished().unwrap();
    let marks: Vec<(String, Option<MultipartProgress>)> = left
        .iter()
        .map(|r| (remote_of(&r.job), r.multipart.clone()))
        .collect();
    assert_eq!(
        marks,
        [
            ("up/older.txt".to_owned(), None),
            ("up/f0.txt".to_owned(), Some(upload_sent())),
            ("up/f1.txt".to_owned(), Some(upload_sent())),
        ]
    );

    let b = engine_on(journal.clone(), with(4));
    assert_eq!(b.resume_from_journal(&only_site(mock.clone())), 3);
    let s = wait_idle(&b).await;
    assert_eq!((s.files_done, s.files_failed), (3, 0), "{}", brief(&s));
    assert_eq!(
        s.rechecked,
        Rechecked {
            checked: 2,
            resent: 0
        },
        "only the two marked ones"
    );
    assert!(journal.unfinished().unwrap().is_empty());
}
