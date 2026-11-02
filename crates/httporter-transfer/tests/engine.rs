//! The transfer engine against `MockProvider`, on a paused tokio clock: every wait below is
//! virtual time, so latency, retries and bandwidth limits cost no real time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use httporter_core::{
    AuthHint, ByteStream, Capabilities, EmptyFolders, EngineSnapshot, EngineState, Job, JobId,
    JobKind, JobRecord, JobState, Journal, JournalError, Listing, MemoryJournal, MockOp,
    MockProvider, MultipartProgress, ProviderError, RemotePath, SiteId, StorageProvider,
    TransferSettings, UploadOptions, UploadSource,
};
use httporter_core::{Direction, JobView};
use httporter_transfer::{AdaptiveLimiter, Batch, Engine, eta, hash_file};
use md5::Md5;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

const SITE_NAME: &str = "Test site";

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

fn engine(settings: TransferSettings) -> (Engine, Arc<MemoryJournal>) {
    let journal = Arc::new(MemoryJournal::new());
    let engine = Engine::new(
        tokio::runtime::Handle::current(),
        journal.clone(),
        settings,
        describe,
    );
    (engine, journal)
}

fn site() -> SiteId {
    SiteId("site-1".into())
}

fn batch(provider: Arc<dyn StorageProvider>, jobs: Vec<Job>) -> Batch {
    Batch {
        site: site(),
        site_name: SITE_NAME.into(),
        host: "storage.example".into(),
        provider,
        jobs,
    }
}

/// The bytes of local file number `i`: `size` bytes that differ from file to file.
fn content(i: usize, size: usize) -> Vec<u8> {
    (0..size).map(|b| ((b + i * 7) % 251) as u8).collect()
}

/// Writes `n` local files of `size` bytes (`f0.txt`, …) and returns upload jobs sending them to
/// `up/f0.txt`, ….
fn upload_jobs(dir: &Path, n: usize, size: usize) -> Vec<Job> {
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

/// The id of the queued job whose remote path is `path`.
fn queued_id(engine: &Engine, path: &str) -> JobId {
    engine
        .snapshot()
        .queued
        .iter()
        .find(|view| view.path == path)
        .unwrap_or_else(|| panic!("{path} is not queued"))
        .id
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

fn states(journal: &MemoryJournal) -> Vec<JobState> {
    journal.records().iter().map(|r| r.state).collect()
}

fn count(journal: &MemoryJournal, state: JobState) -> usize {
    states(journal).iter().filter(|s| **s == state).count()
}

fn slow_mock(latency: Duration) -> Arc<MockProvider> {
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.set_latency(MockOp::Upload, latency);
    mock
}

/// Forwards to a mock, noting the order of uploads and what the journal held at the first one.
struct Recorder {
    mock: MockProvider,
    journal: Arc<MemoryJournal>,
    uploads: Mutex<Vec<RemotePath>>,
    journal_at_first_upload: Mutex<Option<Vec<JobRecord>>>,
}

impl Recorder {
    fn new(mock: MockProvider, journal: Arc<MemoryJournal>) -> Self {
        Self {
            mock,
            journal,
            uploads: Mutex::new(Vec::new()),
            journal_at_first_upload: Mutex::new(None),
        }
    }
}

#[async_trait]
impl StorageProvider for Recorder {
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
        {
            let mut first = self.journal_at_first_upload.lock().unwrap();
            if first.is_none() {
                *first = Some(self.journal.records());
            }
        }
        self.uploads.lock().unwrap().push(path.clone());
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
}

#[tokio::test(start_paused = true)]
async fn uploads_all_jobs_and_marks_journal_done() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    let jobs = upload_jobs(dir.path(), 50, 1000);
    let batch_id = engine.submit(batch(mock.clone(), jobs)).unwrap();
    assert!(uuid_like(&batch_id), "{batch_id}");

    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 50, "{}", brief(&s));
    assert_eq!((s.files_total, s.files_failed), (50, 0));
    assert_eq!((s.bytes_done, s.bytes_total), (50_000, 50_000));
    assert_eq!(s.done_count, 50);
    assert_eq!(s.state, EngineState::Idle);
    assert!(journal.unfinished().unwrap().is_empty());
    assert_eq!(count(&journal, JobState::Done), 50);
    assert!(journal.records().iter().all(|r| r.batch == batch_id));
    for i in 0..50 {
        let stored = mock.contents(&p(&format!("up/f{i}.txt"))).unwrap();
        assert_eq!(stored.as_ref(), content(i, 1000).as_slice(), "f{i}");
    }
    let last = s.last_batch.expect("a finished batch is summarised");
    assert_eq!((last.files, last.bytes), (50, 50_000));
    assert_eq!(last.direction, Direction::Upload);
    assert_eq!(last.site, SITE_NAME);
    assert_eq!(s.site, SITE_NAME);
    assert_eq!(s.host, "storage.example");
}

fn uuid_like(text: &str) -> bool {
    text.len() == 36 && text.chars().filter(|c| *c == '-').count() == 4
}

#[tokio::test(start_paused = true)]
async fn journal_written_before_first_request() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    let recorder = Arc::new(Recorder::new(
        MockProvider::new(Capabilities::default()),
        journal.clone(),
    ));
    engine
        .submit(batch(recorder.clone(), upload_jobs(dir.path(), 50, 10)))
        .unwrap();
    wait_idle(&engine).await;

    let seen = recorder.journal_at_first_upload.lock().unwrap().clone();
    let seen = seen.expect("an upload was made");
    assert_eq!(seen.len(), 50);
    assert!(seen.iter().all(|r| r.state == JobState::Queued));
    assert!(seen.iter().all(|r| r.site == site()));
    // The engine's ids are the journal's ids.
    let ids: Vec<i64> = seen.iter().map(|r| r.id).collect();
    assert_eq!(ids, (1..=50).collect::<Vec<i64>>());
}

#[tokio::test(start_paused = true)]
async fn never_exceeds_the_lane_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(with(8));
    let mock = slow_mock(Duration::from_millis(100));
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 50, 10)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 50);
    assert_eq!(mock.max_in_flight(MockOp::Upload), 8);
    assert_eq!(engine.site_transfer_limit(&site()), Some(8));
}

#[test]
fn slow_down_halves_and_recovers() {
    let mut limiter = AdaptiveLimiter::new(32);
    assert_eq!(limiter.limit(), 32);
    limiter.on_slow_down();
    assert_eq!(limiter.limit(), 16);
    for _ in 0..19 {
        limiter.on_success();
    }
    assert_eq!(limiter.limit(), 16, "one back per 20 successes in a row");
    limiter.on_success();
    assert_eq!(limiter.limit(), 17);

    // A slow-down starts the count again.
    for _ in 0..10 {
        limiter.on_success();
    }
    limiter.on_slow_down();
    assert_eq!(limiter.limit(), 8);
    for _ in 0..10 {
        limiter.on_success();
    }
    assert_eq!(limiter.limit(), 8);

    // Never below 1.
    for _ in 0..10 {
        limiter.on_slow_down();
    }
    assert_eq!(limiter.limit(), 1);

    // Never above the configured value.
    for _ in 0..(20 * 40) {
        limiter.on_success();
    }
    assert_eq!(limiter.limit(), 32);

    // The configured value can change: a limiter at full speed follows it, a slowed one only
    // comes down to it.
    limiter.set_configured(8);
    assert_eq!(limiter.limit(), 8);
    limiter.set_configured(64);
    assert_eq!(limiter.limit(), 64);
    limiter.on_slow_down();
    limiter.set_configured(100);
    assert_eq!(limiter.limit(), 32);
    limiter.set_configured(4);
    assert_eq!(limiter.limit(), 4);
    for _ in 0..100 {
        limiter.on_success();
    }
    assert_eq!(limiter.limit(), 4);

    let mut floor = AdaptiveLimiter::new(0);
    assert_eq!(floor.limit(), 1);
    floor.on_slow_down();
    assert_eq!(floor.limit(), 1);
}

fn busy() -> ProviderError {
    ProviderError::Server {
        status: 400,
        detail: "busy".into(),
    }
}

#[tokio::test(start_paused = true)]
async fn busy_400_counts_as_slow_down() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(8));
    let mock = slow_mock(Duration::from_millis(100));
    mock.fail_next(MockOp::Upload, busy(), 5);
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 50, 10)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (50, 0), "{}", brief(&s));
    assert_eq!(mock.calls(MockOp::Upload), 55);
    assert!(mock.max_in_flight(MockOp::Upload) <= 8);
    let limit = engine.site_transfer_limit(&site()).unwrap();
    assert!(
        limit < 8,
        "the limit dropped and has not fully recovered: {limit}"
    );
    assert!(journal.unfinished().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn adaptive_off_keeps_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings {
        adaptive: false,
        ..with(8)
    });
    let mock = slow_mock(Duration::from_millis(100));
    mock.fail_next(MockOp::Upload, busy(), 5);
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 50, 10)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (50, 0));
    assert_eq!(engine.site_transfer_limit(&site()), Some(8));
    assert_eq!(mock.max_in_flight(MockOp::Upload), 8);
}

fn refused() -> ProviderError {
    ProviderError::Auth {
        hint: AuthHint::Unknown,
    }
}

#[tokio::test(start_paused = true)]
async fn auth_failure_pauses_site_without_failing_items() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    // The site's stored credentials stopped working: the first request and every later one
    // is refused.
    let old = slow_mock(Duration::from_millis(100));
    old.fail_next(MockOp::Upload, refused(), 100);
    engine
        .submit(batch(old.clone(), upload_jobs(dir.path(), 100, 10)))
        .unwrap();

    let stopped = EngineState::AuthStopped {
        site: SITE_NAME.into(),
    };
    let s = wait_for(&engine, "the auth stop", |s| {
        s.state == stopped && s.active.is_empty()
    })
    .await;
    assert_eq!(s.files_failed, 0);
    assert_eq!(s.queued_count, 100);
    assert!(s.failed.is_empty());
    let first_round = old.calls(MockOp::Upload);
    assert!(first_round <= 32, "{first_round}");

    // The lane stays stopped: no retry storm.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(old.calls(MockOp::Upload), first_round);
    assert_eq!(engine.snapshot().state, stopped);
    assert_eq!(count(&journal, JobState::Failed), 0);
    assert_eq!(count(&journal, JobState::Queued), 100);

    // The site is edited: its new client is used for everything left.
    let edited = Arc::new(MockProvider::new(Capabilities::default()));
    engine.resume_site(&site(), edited.clone());
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (100, 0), "{}", brief(&s));
    assert_eq!(s.state, EngineState::Idle);
    assert_eq!(edited.calls(MockOp::Upload), 100);
    assert_eq!(old.calls(MockOp::Upload), first_round);
    assert!(journal.unfinished().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn network_errors_retry_then_fail_with_reason() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    assert_eq!(TransferSettings::default().retries, 5);
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.fail_next(MockOp::Upload, ProviderError::Network("reset".into()), 6);
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 1, 10)))
        .unwrap();
    let s = wait_for(&engine, "the failure", |s| s.files_failed == 1).await;
    assert_eq!(start.elapsed(), Duration::from_secs(1 + 2 + 4 + 8 + 10));
    assert_eq!(mock.calls(MockOp::Upload), 6);
    assert_eq!(s.failed.len(), 1);
    assert_eq!(s.failed[0].error.as_deref(), Some("No connection"));
    assert_eq!(s.failed[0].path, "up/f0.txt");
    assert_eq!(s.files_done, 0);

    wait_idle(&engine).await;
    let record = &journal.records()[0];
    assert_eq!(record.state, JobState::Failed);
    assert_eq!(record.error.as_deref(), Some("No connection"));
}

#[tokio::test(start_paused = true)]
async fn zero_retries_fail_on_first_error() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings {
        retries: 0,
        ..TransferSettings::default()
    });
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.fail_next(MockOp::Upload, ProviderError::Network("reset".into()), 1);
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 1, 10)))
        .unwrap();
    let s = wait_for(&engine, "the failure", |s| s.files_failed == 1).await;
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(mock.calls(MockOp::Upload), 1);
    assert_eq!(s.failed[0].error.as_deref(), Some("No connection"));
}

#[tokio::test(start_paused = true)]
async fn paused_item_is_skipped_until_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(1));
    let mock = slow_mock(Duration::from_secs(1));
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 5, 10)))
        .unwrap();
    let held = queued_id(&engine, "up/f2.txt");
    engine.pause(held);

    let s = wait_for(&engine, "the others", |s| {
        s.files_done == 4 && s.active.is_empty()
    })
    .await;
    assert_eq!(s.queued_count, 1);
    assert_eq!(s.queued[0].id, held);
    assert_eq!(mock.contents(&p("up/f2.txt")), None);
    assert!(engine.is_busy(), "a paused item is still waiting");
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(mock.calls(MockOp::Upload), 4);

    engine.resume(held);
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 5);
    assert!(mock.contents(&p("up/f2.txt")).is_some());
    assert_eq!(count(&journal, JobState::Done), 5);
}

#[tokio::test(start_paused = true)]
async fn delete_waits_for_inflight_uploads_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities {
        recursive_delete: true,
        ..Capabilities::default()
    }));
    mock.set_latency(MockOp::Upload, Duration::from_secs(10));
    let local = dir.path().join("a.txt");
    std::fs::write(&local, b"fresh").unwrap();
    let start = Instant::now();
    engine
        .submit(batch(
            mock.clone(),
            vec![Job::Upload {
                local,
                remote: p("dir/a.txt"),
                size: 5,
            }],
        ))
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    engine
        .submit(batch(
            mock.clone(),
            vec![Job::Delete {
                remote: p("dir"),
                folder: true,
            }],
        ))
        .unwrap();

    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        mock.calls(MockOp::DeleteFolder),
        0,
        "the upload is still under way"
    );

    let s = wait_idle(&engine).await;
    assert!(start.elapsed() >= Duration::from_secs(10));
    assert_eq!((s.files_done, s.files_failed), (2, 0), "{}", brief(&s));
    assert_eq!(mock.calls(MockOp::DeleteFolder), 1);
    // The upload landed first and the delete then removed it.
    assert_eq!(mock.contents(&p("dir/a.txt")), None);
    assert_eq!(count(&journal, JobState::Done), 2);
}

#[tokio::test(start_paused = true)]
async fn delete_uses_batches_when_supported() {
    let (engine, journal) = engine(TransferSettings::default());
    let mut mock = MockProvider::new(Capabilities {
        batch_delete_max: 1000,
        ..Capabilities::default()
    });
    for i in 0..2500 {
        mock = mock.with_file(&format!("old/f{i}.txt"), b"x");
    }
    let mock = Arc::new(mock);
    let jobs = (0..2500)
        .map(|i| Job::Delete {
            remote: p(&format!("old/f{i}.txt")),
            folder: false,
        })
        .collect();
    engine.submit(batch(mock.clone(), jobs)).unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (2500, 0), "{}", brief(&s));
    assert_eq!(s.direction, Direction::Delete);
    assert_eq!(mock.calls(MockOp::DeleteBatch), 3);
    assert_eq!(mock.calls(MockOp::DeleteFile), 0);
    assert!(
        mock.list(&RemotePath::root())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(count(&journal, JobState::Done), 2500);
}

#[tokio::test(start_paused = true)]
async fn a_failed_path_in_a_batch_is_retried_alone() {
    let (engine, _journal) = engine(TransferSettings::default());
    let mock = Arc::new(
        MockProvider::new(Capabilities {
            batch_delete_max: 1000,
            ..Capabilities::default()
        })
        .with_file("a.txt", b"1")
        .with_file("b.txt", b"2")
        .with_file("c.txt", b"3"),
    );
    // The first path's answer is lost; the second is refused for good.
    mock.fail_next(
        MockOp::DeleteFile,
        ProviderError::Network("reset".into()),
        1,
    );
    mock.fail_next(
        MockOp::DeleteFile,
        ProviderError::Permanent("locked".into()),
        1,
    );
    let jobs = ["a.txt", "b.txt", "c.txt"]
        .into_iter()
        .map(|path| Job::Delete {
            remote: p(path),
            folder: false,
        })
        .collect();
    engine.submit(batch(mock.clone(), jobs)).unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (2, 1), "{}", brief(&s));
    assert_eq!(mock.calls(MockOp::DeleteBatch), 2);
    assert_eq!(s.failed[0].path, "b.txt");
    assert_eq!(
        s.failed[0].error.as_deref(),
        Some("Failed: permanent error: locked")
    );
    assert_eq!(mock.contents(&p("b.txt")).as_deref(), Some(&b"2"[..]));
}

#[tokio::test(start_paused = true)]
async fn not_found_after_a_lost_response_counts_as_deleted() {
    let (engine, journal) = engine(TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    // The file is already gone: the first request's answer was lost on the way back.
    mock.fail_next(
        MockOp::DeleteFile,
        ProviderError::Network("timed out".into()),
        1,
    );
    engine
        .submit(batch(
            mock.clone(),
            vec![Job::Delete {
                remote: p("gone.txt"),
                folder: false,
            }],
        ))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (1, 0));
    assert_eq!(mock.calls(MockOp::DeleteFile), 2);

    // Without a lost answer, a missing file is reported.
    engine
        .submit(batch(
            mock.clone(),
            vec![Job::Delete {
                remote: p("never-there.txt"),
                folder: false,
            }],
        ))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (0, 1));
    assert_eq!(s.failed[0].error.as_deref(), Some("Failed: not found"));
    assert_eq!(count(&journal, JobState::Failed), 1);
}

#[tokio::test(start_paused = true)]
async fn cancel_all_marks_cancelled_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(8));
    let mock = slow_mock(Duration::from_secs(1));
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 50, 10)))
        .unwrap();
    tokio::time::sleep_until(start + secs(1.5)).await;
    let s = engine.snapshot();
    assert_eq!((s.files_done, s.active.len()), (8, 8), "{}", brief(&s));

    engine.cancel_all();
    let s = wait_idle(&engine).await;
    assert_eq!(s.state, EngineState::Idle);
    assert_eq!((s.files_done, s.files_total), (8, 8));
    assert_eq!(s.queued_count, 0);
    assert!(s.active.is_empty() && s.queued.is_empty() && s.failed.is_empty());
    assert_eq!(mock.calls(MockOp::Upload), 16);
    assert_eq!(count(&journal, JobState::Done), 8);
    assert_eq!(count(&journal, JobState::Cancelled), 42);
    assert!(journal.unfinished().unwrap().is_empty());

    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(mock.calls(MockOp::Upload), 16);
    let stored = (0..50)
        .filter(|i| mock.contents(&p(&format!("up/f{i}.txt"))).is_some())
        .count();
    assert_eq!(stored, 8, "the cancelled uploads under way were not stored");
}

#[tokio::test(start_paused = true)]
async fn cancel_one() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(1));
    let mock = slow_mock(Duration::from_secs(1));
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 5, 10)))
        .unwrap();
    tokio::time::sleep_until(start + secs(0.5)).await;
    let running = active_id(&engine, "up/f0.txt");
    let waiting = queued_id(&engine, "up/f3.txt");
    engine.cancel(running);
    engine.cancel(waiting);

    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_total, s.files_failed), (3, 3, 0));
    assert_eq!(mock.contents(&p("up/f0.txt")), None);
    assert_eq!(mock.contents(&p("up/f3.txt")), None);
    for kept in ["up/f1.txt", "up/f2.txt", "up/f4.txt"] {
        assert!(mock.contents(&p(kept)).is_some(), "{kept}");
    }
    assert_eq!(mock.calls(MockOp::Upload), 4, "f3 was never sent");
    let records = journal.records();
    let state_of = |id: JobId| {
        records
            .iter()
            .find(|r| r.id as u64 == id.0)
            .map(|r| r.state)
    };
    assert_eq!(state_of(running), Some(JobState::Cancelled));
    assert_eq!(state_of(waiting), Some(JobState::Cancelled));
    assert_eq!(count(&journal, JobState::Done), 3);
}

#[tokio::test(start_paused = true)]
async fn pause_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(8));
    let mock = slow_mock(Duration::from_secs(1));
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 50, 10)))
        .unwrap();
    tokio::time::sleep_until(start + secs(1.5)).await;

    engine.pause_all();
    let s = wait_for(&engine, "the pause", |s| s.active.is_empty()).await;
    assert_eq!(s.state, EngineState::Paused);
    assert_eq!((s.files_done, s.queued_count), (8, 42));
    assert_eq!(s.eta, None);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(mock.calls(MockOp::Upload), 16);
    assert_eq!(engine.snapshot().state, EngineState::Paused);
    assert!(engine.is_busy());

    engine.resume_all();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (50, 0), "{}", brief(&s));
    // The eight interrupted by the pause were sent again.
    assert_eq!(mock.calls(MockOp::Upload), 58);
    assert!(journal.unfinished().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn move_to_top_runs_next() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(1));
    let mock = MockProvider::new(Capabilities::default());
    mock.set_latency(MockOp::Upload, Duration::from_secs(1));
    let recorder = Arc::new(Recorder::new(mock, journal.clone()));
    let start = Instant::now();
    engine
        .submit(batch(recorder.clone(), upload_jobs(dir.path(), 10, 10)))
        .unwrap();
    tokio::time::sleep_until(start + secs(0.5)).await;
    engine.move_to_top(queued_id(&engine, "up/f7.txt"));
    assert_eq!(engine.snapshot().queued[0].path, "up/f7.txt");

    wait_idle(&engine).await;
    let order: Vec<String> = recorder
        .uploads
        .lock()
        .unwrap()
        .iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    let expected: Vec<String> = [0, 7, 1, 2, 3, 4, 5, 6, 8, 9]
        .iter()
        .map(|i| format!("up/f{i}.txt"))
        .collect();
    assert_eq!(order, expected);
}

#[tokio::test(start_paused = true)]
async fn retry_selected_requeues() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.fail_next(
        MockOp::Upload,
        ProviderError::Permanent("disk on fire".into()),
        3,
    );
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 3, 10)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (0, 3));
    assert!(
        s.failed
            .iter()
            .all(|view| view.error.as_deref() == Some("Failed: permanent error: disk on fire"))
    );
    let second = s
        .failed
        .iter()
        .find(|view| view.path == "up/f1.txt")
        .unwrap()
        .id;

    engine.retry(&[second]);
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (1, 2), "{}", brief(&s));
    assert!(mock.contents(&p("up/f1.txt")).is_some());
    assert_eq!(mock.contents(&p("up/f0.txt")), None);
    assert_eq!(count(&journal, JobState::Done), 1);
    assert_eq!(count(&journal, JobState::Failed), 2);
    assert!(s.failed.iter().all(|view| view.id != second));

    engine.retry_all_failed();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (3, 0), "{}", brief(&s));
    assert!(s.failed.is_empty());
    assert!(journal.unfinished().unwrap().is_empty());
    let errors: Vec<Option<String>> = journal.records().into_iter().map(|r| r.error).collect();
    assert_eq!(
        errors,
        [None, None, None],
        "a retried job loses its old reason"
    );
}

#[tokio::test(start_paused = true)]
async fn bandwidth_cap_limits_throughput() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings {
        bandwidth_limit: Some(1_000_000),
        ..TransferSettings::default()
    });
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    let start = Instant::now();
    engine
        .submit(batch(mock, upload_jobs(dir.path(), 3, 1_000_000)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 3);
    let took = start.elapsed();
    assert!(took >= secs(2.9), "{took:?}");
    assert!(took <= secs(3.5), "{took:?}");
}

#[tokio::test(start_paused = true)]
async fn a_bandwidth_limit_of_zero_is_no_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings {
        bandwidth_limit: Some(0),
        ..TransferSettings::default()
    });
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    let start = Instant::now();
    engine
        .submit(batch(mock, upload_jobs(dir.path(), 3, 1_000_000)))
        .unwrap();
    wait_idle(&engine).await;
    assert_eq!(start.elapsed(), Duration::ZERO);
}

fn near(actual: f64, expected: f64, tolerance: f64) -> bool {
    (actual - expected).abs() <= tolerance
}

#[tokio::test(start_paused = true)]
async fn progress_counts_bytes_via_meter_and_rates_use_5s_window() {
    let dir = tempfile::tempdir().unwrap();
    // One file at a time through a 1 MB/s cap: file k finishes at k seconds.
    let (engine, _journal) = engine(TransferSettings {
        bandwidth_limit: Some(1_000_000),
        ..with(1)
    });
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    let start = Instant::now();
    engine
        .submit(batch(mock, upload_jobs(dir.path(), 10, 1_000_000)))
        .unwrap();

    tokio::time::sleep_until(start + secs(6.5)).await;
    let s = engine.snapshot();
    assert_eq!((s.files_done, s.files_total), (6, 10), "{}", brief(&s));
    assert_eq!(s.bytes_total, 10_000_000);
    // Half of the seventh file is counted as it is sent, not when it finishes.
    assert!(
        (6_400_000..=6_600_000).contains(&s.bytes_done),
        "{}",
        s.bytes_done
    );
    assert_eq!(s.active.len(), 1);
    let sending = &s.active[0];
    assert_eq!(sending.kind, JobKind::Upload);
    assert!(
        (400_000..=600_000).contains(&sending.done_bytes),
        "{}",
        sending.done_bytes
    );
    // Five files and 5 MB in the last five seconds.
    assert!(near(s.files_per_s, 1.0, 0.05), "{}", s.files_per_s);
    assert!(near(s.bytes_per_s, 1e6, 0.05e6), "{}", s.bytes_per_s);
    let left = s.eta.expect("an estimate while running");
    assert!(left >= secs(3.5) && left <= secs(4.5), "{left:?}");

    let s = wait_idle(&engine).await;
    assert_eq!(s.bytes_done, 10_000_000);

    // The window slides: files 8, 9 and 10 and the last 2.5 MB fall within it at 12.5 s…
    tokio::time::sleep_until(start + secs(12.5)).await;
    let s = engine.snapshot();
    assert!(near(s.files_per_s, 0.6, 0.05), "{}", s.files_per_s);
    assert!(near(s.bytes_per_s, 0.5e6, 0.05e6), "{}", s.bytes_per_s);
    // …and nothing does by 16 s.
    tokio::time::sleep_until(start + secs(16.0)).await;
    let s = engine.snapshot();
    assert_eq!((s.files_per_s, s.bytes_per_s), (0.0, 0.0));
}

#[test]
fn eta_is_the_longer_estimate() {
    // 100 files at 10/s is 10 s; 1,000 bytes at 1,000/s is 1 s.
    assert_eq!(eta(100, 1000, 10.0, 1000.0), Some(secs(10.0)));
    // 10 files at 10/s is 1 s; 10,000 bytes at 100/s is 100 s.
    assert_eq!(eta(10, 10_000, 10.0, 100.0), Some(secs(100.0)));
    // Nothing left takes no time.
    assert_eq!(eta(0, 0, 0.0, 0.0), Some(Duration::ZERO));
    // With nothing measured yet there is no estimate.
    assert_eq!(eta(5, 500, 0.0, 0.0), None);
    // One big file under way: no file has finished, but the bytes say how long.
    assert_eq!(eta(1, 4_000, 0.0, 1_000.0), Some(secs(4.0)));
    // Deletes move no bytes.
    assert_eq!(eta(30, 0, 15.0, 0.0), Some(secs(2.0)));
    assert_eq!(eta(30, 0, f64::NAN, 0.0), None);
}

#[test]
fn hash_file_matches_known_digests() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("abc.txt");
    std::fs::write(&path, b"abc").unwrap();
    let hashes = hash_file(&path).unwrap();
    assert_eq!(
        hex::encode(hashes.sha256),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(hex::encode(hashes.md5), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(hashes.size, 3);
    assert_eq!(
        hashes.modified,
        std::fs::metadata(&path).unwrap().modified().unwrap()
    );

    let empty = dir.path().join("empty");
    std::fs::write(&empty, b"").unwrap();
    let hashes = hash_file(&empty).unwrap();
    assert_eq!(
        hex::encode(hashes.sha256),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(hex::encode(hashes.md5), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(hashes.size, 0);

    // Larger than one read buffer.
    let big = dir.path().join("big.bin");
    let data = content(3, 1_000_003);
    std::fs::write(&big, &data).unwrap();
    let hashes = hash_file(&big).unwrap();
    assert_eq!(hashes.size, 1_000_003);
    assert_eq!(hashes.sha256, <[u8; 32]>::from(Sha256::digest(&data)));
    assert_eq!(hashes.md5, <[u8; 16]>::from(Md5::digest(&data)));

    assert!(hash_file(&dir.path().join("missing")).is_err());
}

#[test]
fn hash_file_reads_paths_longer_than_260_characters() {
    let dir = tempfile::tempdir().unwrap();
    let mut deep: PathBuf = dir.path().to_owned();
    for i in 0..6 {
        deep.push(format!("{i}-{}", "d".repeat(48)));
    }
    std::fs::create_dir_all(&deep).unwrap();
    let file = deep.join("abc.txt");
    assert!(file.as_os_str().len() > 260, "{}", file.display());
    std::fs::write(&file, b"abc").unwrap();
    let hashes = hash_file(&file).unwrap();
    assert_eq!(hashes.size, 3);
    assert_eq!(hex::encode(hashes.md5), "900150983cd24fb0d6963f7d28e17f72");
}

#[tokio::test(start_paused = true)]
async fn twenty_thousand_jobs_stay_cheap() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings::default());
    let mock = slow_mock(Duration::from_secs(1));
    let local = dir.path().join("tiny.txt");
    std::fs::write(&local, b"x").unwrap();
    let jobs: Vec<Job> = (0..20_000)
        .map(|i| Job::Upload {
            local: local.clone(),
            remote: p(&format!("big/f{i}.txt")),
            size: 1,
        })
        .collect();
    let start = Instant::now();
    engine.submit(batch(mock, jobs)).unwrap();
    // Seven rounds of 32 finish by 7.5 s.
    tokio::time::sleep_until(start + secs(7.5)).await;

    let mut fastest = Duration::MAX;
    let mut s = EngineSnapshot::default();
    for _ in 0..3 {
        let clock = std::time::Instant::now();
        s = engine.snapshot();
        fastest = fastest.min(clock.elapsed());
    }
    assert!(fastest < Duration::from_millis(5), "{fastest:?}");
    assert_eq!(s.queued.len(), 200);
    assert_eq!(s.done.len(), 200);
    assert_eq!(s.done_count, 224);
    assert_eq!(s.active.len(), 32);
    assert_eq!(s.queued_count, 20_000 - 224 - 32);
    assert_eq!(s.files_total, 20_000);
    // The done window holds the latest finished jobs, each once, complete.
    let mut ids: Vec<u64> = s.done.iter().map(|view| view.id.0).collect();
    assert!(s.done.iter().all(|view| view.done_bytes == view.size));
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 200);
    // Round k sends ids 32(k-1)+1 ..= 32k. The 24 that left the window finished first, in
    // round 1 (in no particular order); rounds 2 to 7 are all there.
    assert!(ids.iter().all(|&id| id <= 224), "{ids:?}");
    assert!(
        (33..=224).all(|id| ids.binary_search(&id).is_ok()),
        "{ids:?}"
    );
    assert_eq!(s.queued[0].path, "big/f256.txt");
}

#[tokio::test(start_paused = true)]
async fn downloads_stream_to_a_part_file_then_rename() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    let mock = Arc::new(
        MockProvider::new(Capabilities::default())
            .with_file("site/a.txt", b"hello")
            .with_file("site/sub/b.txt", b"world"),
    );
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("a.txt"), b"old contents").unwrap();
    let jobs = vec![
        Job::Download {
            remote: p("site/a.txt"),
            local: out.join("a.txt"),
            size: 5,
        },
        Job::Download {
            remote: p("site/sub/b.txt"),
            local: out.join("sub").join("b.txt"),
            size: 5,
        },
        Job::Download {
            remote: p("site/missing.txt"),
            local: out.join("missing.txt"),
            size: 5,
        },
    ];
    engine.submit(batch(mock.clone(), jobs)).unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (2, 1), "{}", brief(&s));
    assert_eq!(s.direction, Direction::Download);
    assert_eq!(s.bytes_done, 10);
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"hello");
    assert_eq!(
        std::fs::read(out.join("sub").join("b.txt")).unwrap(),
        b"world"
    );
    assert!(!out.join("missing.txt").exists());
    assert_eq!(s.failed[0].error.as_deref(), Some("Failed: not found"));
    assert_eq!(part_files(&out), Vec::<PathBuf>::new());
    assert_eq!(count(&journal, JobState::Done), 2);
}

/// Every `*.httporter-part` file below `dir`.
fn part_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(part_files(&path));
        } else if path.to_string_lossy().ends_with(".httporter-part") {
            found.push(path);
        }
    }
    found
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_download_leaves_no_part_file() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings {
        bandwidth_limit: Some(1_000),
        ..TransferSettings::default()
    });
    let mock =
        Arc::new(MockProvider::new(Capabilities::default()).with_file("big.bin", &[7; 10_000]));
    let target = dir.path().join("big.bin");
    let start = Instant::now();
    engine
        .submit(batch(
            mock,
            vec![Job::Download {
                remote: p("big.bin"),
                local: target.clone(),
                size: 10_000,
            }],
        ))
        .unwrap();
    tokio::time::sleep_until(start + secs(2.0)).await;
    engine.cancel(active_id(&engine, "big.bin"));
    wait_idle(&engine).await;
    assert!(!target.exists());
    assert_eq!(part_files(dir.path()), Vec::<PathBuf>::new());
}

#[tokio::test(start_paused = true)]
async fn create_folder_jobs_make_folders() {
    let (engine, _journal) = engine(TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities {
        empty_folders: EmptyFolders::Supported,
        ..Capabilities::default()
    }));
    engine
        .submit(batch(
            mock.clone(),
            vec![Job::CreateFolder {
                remote: p("new/inner"),
            }],
        ))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 1);
    let names: Vec<String> = mock
        .list(&p("new"))
        .await
        .unwrap()
        .entries
        .iter()
        .map(|e| e.name().to_owned())
        .collect();
    assert_eq!(names, ["inner"]);
}

/// A journal that can't write anything down.
struct BrokenJournal;

impl Journal for BrokenJournal {
    fn insert(&self, _records: &mut [JobRecord]) -> Result<(), JournalError> {
        Err(JournalError("disk is read-only".into()))
    }
    fn set_state(&self, _: i64, _: JobState, _: Option<&str>) -> Result<(), JournalError> {
        Err(JournalError("disk is read-only".into()))
    }
    fn set_multipart(&self, _: i64, _: Option<&MultipartProgress>) -> Result<(), JournalError> {
        Err(JournalError("disk is read-only".into()))
    }
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError> {
        Ok(Vec::new())
    }
    fn clear_finished(&self) -> Result<(), JournalError> {
        Ok(())
    }
    fn discard_unfinished(&self) -> Result<(), JournalError> {
        Ok(())
    }
}

/// Ruling Q21: a batch the journal can't record is refused whole, not run unjournalled.
#[tokio::test(start_paused = true)]
async fn a_batch_the_journal_cannot_record_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        tokio::runtime::Handle::current(),
        Arc::new(BrokenJournal),
        TransferSettings::default(),
        describe,
    );
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    let err = engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 3, 10)))
        .unwrap_err();
    assert_eq!(err.0, "disk is read-only");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(mock.calls(MockOp::Upload), 0);
    let s = engine.snapshot();
    assert_eq!((s.files_total, s.queued_count), (0, 0));
    assert!(!engine.is_busy());
}

#[tokio::test(start_paused = true)]
async fn dropping_the_engine_stops_new_work_and_keeps_the_rest_queued() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(with(8));
    let mock = slow_mock(Duration::from_secs(1));
    let start = Instant::now();
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 50, 10)))
        .unwrap();
    tokio::time::sleep_until(start + secs(1.5)).await;
    drop(engine);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(mock.calls(MockOp::Upload), 16);
    assert_eq!(count(&journal, JobState::Done), 8);
    assert_eq!(journal.unfinished().unwrap().len(), 42);
}

#[tokio::test(start_paused = true)]
async fn views_show_kind_path_size_and_progress() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(with(1));
    let mock = slow_mock(Duration::from_secs(1));
    engine
        .submit(batch(mock, upload_jobs(dir.path(), 2, 10)))
        .unwrap();
    let s = engine.snapshot();
    assert_eq!(s.state, EngineState::Running);
    assert_eq!(
        s.queued[1],
        JobView {
            id: JobId(2),
            kind: JobKind::Upload,
            path: "up/f1.txt".into(),
            size: 10,
            done_bytes: 0,
            error: None,
        }
    );
}

/// Review finding: a paused job moved to the top stays paused, and the engine still knows the
/// lane has nothing else to run.
#[tokio::test(start_paused = true)]
async fn a_paused_job_moved_to_the_top_stays_paused() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(with(1));
    let mock = slow_mock(Duration::from_secs(1));
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 3, 10)))
        .unwrap();
    let held = queued_id(&engine, "up/f2.txt");
    engine.pause(held);
    engine.move_to_top(held);
    assert_eq!(engine.snapshot().queued[0].id, held);

    let s = wait_for(&engine, "the others", |s| {
        s.files_done == 2 && s.active.is_empty()
    })
    .await;
    assert_eq!(s.state, EngineState::Idle, "only a paused job is left");
    assert_eq!(mock.contents(&p("up/f2.txt")), None);

    engine.resume(held);
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 3);
}

/// Review finding: cancelling a failed job left over from a finished batch doesn't announce
/// the batch as finished again.
#[tokio::test(start_paused = true)]
async fn cancelling_a_failed_job_does_not_finish_its_batch_again() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, journal) = engine(TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.fail_next(MockOp::Upload, ProviderError::Permanent("no".into()), 1);
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 2, 10)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_failed), (1, 1));
    let finished = s.last_batch.clone().expect("the batch finished");
    assert_eq!(finished.files, 1);

    engine.cancel(s.failed[0].id);
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_failed, 0);
    assert_eq!(s.files_total, 1);
    assert_eq!(s.last_batch, Some(finished));
    assert_eq!(count(&journal, JobState::Cancelled), 1);
    assert!(journal.unfinished().unwrap().is_empty());
}

/// A retried job finishing its batch again does announce it: there is news.
#[tokio::test(start_paused = true)]
async fn a_retried_job_finishes_its_batch_again() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _journal) = engine(TransferSettings::default());
    let mock = Arc::new(MockProvider::new(Capabilities::default()));
    mock.fail_next(MockOp::Upload, ProviderError::Permanent("no".into()), 1);
    engine
        .submit(batch(mock.clone(), upload_jobs(dir.path(), 2, 10)))
        .unwrap();
    let s = wait_idle(&engine).await;
    assert_eq!(s.last_batch.as_ref().map(|b| b.files), Some(1));
    engine.retry_all_failed();
    let s = wait_idle(&engine).await;
    assert_eq!(
        s.last_batch.as_ref().map(|b| (b.files, b.bytes)),
        Some((2, 20))
    );
}
