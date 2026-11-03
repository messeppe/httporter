//! Ruling Q49: the engine keeps every upload's multipart progress in the journal, so a large
//! file sent in parts carries on from the parts already stored after a crash, a pause or a
//! failed try; and an upload the user cancels or discards is given up on the service, so its
//! parts aren't left there to be billed.
//!
//! [`PartsSite`] plays an S3-like service that sends a file in parts through the part journal
//! the engine passes, the way `httporter-s3` does, on a paused tokio clock.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use httporter_core::{
    ByteStream, Capabilities, EngineSnapshot, FileStamp, Job, JobRecord, JobState, Journal,
    JournalError, JournalUpdate, Listing, MemoryJournal, MockProvider, MultipartProgress,
    ProviderError, RemotePath, SiteId, StorageProvider, TransferSettings, UploadOptions,
    UploadSource,
};
use httporter_transfer::{Batch, Engine, SiteInfo};
use tokio::time::Instant;

const SITE_NAME: &str = "Test site";
const HOST: &str = "s3.example";
/// The size of a part: the 20-byte file below is five parts.
const PART: u64 = 4;
const FILE: &[u8] = b"0123456789abcdefghij";
const OBJECT: &str = "video/big.bin";

fn describe(err: &ProviderError) -> String {
    format!("Failed: {err}")
}

fn p(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

fn site() -> SiteId {
    SiteId("site-1".into())
}

fn engine_on(journal: Arc<dyn Journal>) -> Engine {
    Engine::new(
        tokio::runtime::Handle::current(),
        journal,
        TransferSettings::default(),
        describe,
    )
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

fn only_site(provider: Arc<dyn StorageProvider>) -> impl Fn(&SiteId) -> Option<SiteInfo> {
    move |id: &SiteId| {
        (*id == site()).then(|| (SITE_NAME.to_owned(), HOST.to_owned(), provider.clone()))
    }
}

/// The file to upload, written into `dir`, and the job that sends it.
fn big_file(dir: &Path) -> (PathBuf, Job) {
    let local = dir.join("big.bin");
    std::fs::write(&local, FILE).unwrap();
    let job = Job::Upload {
        local: local.clone(),
        remote: p(OBJECT),
        size: FILE.len() as u64,
    };
    (local, job)
}

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

/// Waits, in virtual time, until `done` holds.
async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3600);
    while !done() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_idle(engine: &Engine) -> EngineSnapshot {
    until("the engine to go idle", || !engine.is_busy()).await;
    engine.snapshot()
}

/// An S3-like service for one object: a file is sent in parts of [`PART`] bytes through the
/// part journal in the upload's options, resuming from what it says was stored, and completed
/// once every part is there. It can stop answering part way (the app is then dropped, or
/// paused), and it notes every part, completion and abort.
struct PartsSite {
    mock: MockProvider,
    next_id: AtomicU64,
    /// The uploads under way: their parts, by number.
    uploads: Mutex<HashMap<String, BTreeMap<u32, Vec<u8>>>>,
    /// (upload id, part number), in the order they were stored.
    sent: Mutex<Vec<(String, u32)>>,
    completed: Mutex<Vec<String>>,
    aborted: Mutex<Vec<(RemotePath, String)>>,
    /// After this many parts in all, no part gets an answer.
    hang_after: Mutex<Option<usize>>,
    /// Each upload call: whether it had a part journal.
    journalled: Mutex<Vec<bool>>,
}

impl PartsSite {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mock: MockProvider::new(Capabilities::default()),
            next_id: AtomicU64::new(0),
            uploads: Mutex::default(),
            sent: Mutex::default(),
            completed: Mutex::default(),
            aborted: Mutex::default(),
            hang_after: Mutex::default(),
            journalled: Mutex::default(),
        })
    }

    fn hang_after(&self, parts: Option<usize>) {
        *self.hang_after.lock().unwrap() = parts;
    }

    fn sent(&self) -> Vec<(String, u32)> {
        self.sent.lock().unwrap().clone()
    }

    fn completed(&self) -> Vec<String> {
        self.completed.lock().unwrap().clone()
    }

    fn aborted(&self) -> Vec<(RemotePath, String)> {
        self.aborted.lock().unwrap().clone()
    }

    fn creates(&self) -> u64 {
        self.next_id.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl StorageProvider for PartsSite {
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
        _path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        self.journalled.lock().unwrap().push(opts.parts.is_some());
        let journal = opts
            .parts
            .clone()
            .ok_or_else(|| ProviderError::Permanent("no part journal".into()))?;
        let UploadSource::File { path: local, len } = source else {
            return Err(ProviderError::Permanent("not a file".into()));
        };
        let data = std::fs::read(&local).map_err(|e| ProviderError::Permanent(e.to_string()))?;
        assert_eq!(data.len() as u64, len);
        let total = len.div_ceil(PART) as u32;

        let earlier = journal.load().filter(|progress| {
            self.uploads
                .lock()
                .unwrap()
                .contains_key(&progress.upload_id)
        });
        let id = match &earlier {
            Some(progress) => progress.upload_id.clone(),
            None => {
                let id = format!("u-{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
                self.uploads
                    .lock()
                    .unwrap()
                    .insert(id.clone(), BTreeMap::new());
                journal.started(&id, PART);
                id
            }
        };
        let recorded: Vec<u32> = earlier
            .map(|progress| progress.parts.iter().map(|(n, _)| *n).collect())
            .unwrap_or_default();
        for number in 1..=total {
            if recorded.contains(&number) {
                continue;
            }
            let hang = self
                .hang_after
                .lock()
                .unwrap()
                .is_some_and(|after| self.sent.lock().unwrap().len() >= after);
            if hang {
                std::future::pending::<()>().await;
            }
            let start = (u64::from(number) - 1) * PART;
            let end = (start + PART).min(len);
            let bytes = data[start as usize..end as usize].to_vec();
            {
                let mut uploads = self.uploads.lock().unwrap();
                let parts = uploads.get_mut(&id).ok_or(ProviderError::NotFound)?;
                parts.insert(number, bytes);
            }
            self.sent.lock().unwrap().push((id.clone(), number));
            journal.part_done(number, &format!("\"{id}/{number}\""));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let parts = self
            .uploads
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or(ProviderError::NotFound)?;
        let whole: Vec<u8> = parts.into_values().flatten().collect();
        assert_eq!(whole, data, "the parts make the file");
        self.completed.lock().unwrap().push(id);
        journal.finished();
        Ok(())
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
    async fn abort_multipart(
        &self,
        path: &RemotePath,
        upload_id: &str,
    ) -> Result<(), ProviderError> {
        self.uploads.lock().unwrap().remove(upload_id);
        self.aborted
            .lock()
            .unwrap()
            .push((path.clone(), upload_id.to_owned()));
        Ok(())
    }
}

/// Starts uploading the big file on a new engine over `journal` and lets it store `parts`
/// parts; then the app goes away (the engine is dropped, as in a crash).
async fn crash_after(
    journal: &Arc<MemoryJournal>,
    service: &Arc<PartsSite>,
    job: Job,
    parts: usize,
) {
    service.hang_after(Some(parts));
    let a = engine_on(journal.clone());
    a.submit(batch(service.clone(), vec![job])).unwrap();
    until("the parts to be stored", || service.sent().len() == parts).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(a);
    tokio::time::sleep(Duration::from_secs(1)).await;
    service.hang_after(None);
}

/// The ids of the parts of `progress`, in order.
fn numbers(progress: &MultipartProgress) -> Vec<u32> {
    let mut numbers: Vec<u32> = progress.parts.iter().map(|(n, _)| *n).collect();
    numbers.sort_unstable();
    numbers
}

#[tokio::test(start_paused = true)]
async fn a_multipart_upload_resumes_from_its_journalled_parts_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let (local, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    crash_after(&journal, &service, job, 3).await;

    // The journal holds the upload, the parts stored, and the file they were cut from.
    let record = journal.records()[0].clone();
    assert_eq!(record.state, JobState::Queued);
    let progress = record
        .multipart
        .clone()
        .expect("the progress is journalled");
    assert_eq!(progress.upload_id, "u-1");
    assert_eq!(progress.part_size, PART);
    assert_eq!(numbers(&progress), [1, 2, 3]);
    let meta = std::fs::metadata(&local).unwrap();
    assert_eq!(
        progress.file,
        Some(FileStamp::new(meta.len(), meta.modified().ok()))
    );

    // The next start resumes it: only parts 4 and 5 are sent, on the same upload.
    let b = engine_on(journal.clone());
    assert_eq!(b.resume_from_journal(&only_site(service.clone())), 1);
    let s = wait_idle(&b).await;
    assert_eq!((s.files_done, s.files_failed), (1, 0), "{}", brief(&s));
    let sent: Vec<(String, u32)> = (1..=5).map(|n| ("u-1".to_owned(), n)).collect();
    assert_eq!(service.sent(), sent);
    assert_eq!(service.creates(), 1, "not started again");
    assert_eq!(service.completed(), ["u-1"]);
    assert!(service.aborted().is_empty());
    let record = journal.records()[0].clone();
    assert_eq!((record.state, record.multipart), (JobState::Done, None));
    assert!(service.journalled.lock().unwrap().iter().all(|&set| set));
}

/// A pause drops the upload under way and keeps it; Resume carries on from its parts.
#[tokio::test(start_paused = true)]
async fn a_paused_multipart_upload_carries_on_from_its_parts() {
    let dir = tempfile::tempdir().unwrap();
    let (_, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    service.hang_after(Some(2));
    let engine = engine_on(journal.clone());
    engine.submit(batch(service.clone(), vec![job])).unwrap();
    until("two parts", || service.sent().len() == 2).await;
    engine.pause_all();
    until("the pause", || engine.snapshot().active_count == 0).await;
    service.hang_after(None);
    engine.resume_all();
    let s = wait_idle(&engine).await;
    assert_eq!(s.files_done, 1, "{}", brief(&s));
    assert_eq!(service.creates(), 1);
    assert_eq!(service.sent().len(), 5, "{:?}", service.sent());
    assert!(service.aborted().is_empty());
}

/// Ruling Q49 (a): the user cancels an upload part way: the engine gives it up on the service,
/// from a task closing waits for, and the job is cancelled.
#[tokio::test(start_paused = true)]
async fn cancelling_a_multipart_upload_aborts_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    service.hang_after(Some(2));
    let engine = engine_on(journal.clone());
    engine.submit(batch(service.clone(), vec![job])).unwrap();
    until("two parts", || service.sent().len() == 2).await;
    let id = engine.snapshot().active[0].id;
    engine.cancel(id);
    let s = wait_idle(&engine).await;
    assert_eq!((s.files_done, s.files_total), (0, 0), "{}", brief(&s));
    assert_eq!(service.aborted(), [(p(OBJECT), "u-1".to_owned())]);
    assert_eq!(engine.pending_aborts(), 0);
    assert_eq!(journal.records()[0].state, JobState::Cancelled);
    assert!(service.completed().is_empty());
}

/// Ruling Q49 (a): an upload resumed from the journal and cancelled before it starts again is
/// given up too, and so is one that Cancel All finds failed.
#[tokio::test(start_paused = true)]
async fn cancelling_a_resumed_upload_still_queued_aborts_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    crash_after(&journal, &service, job, 2).await;

    let b = engine_on(journal.clone());
    b.pause_all();
    assert_eq!(b.resume_from_journal(&only_site(service.clone())), 1);
    b.cancel_all();
    wait_idle(&b).await;
    assert_eq!(service.aborted(), [(p(OBJECT), "u-1".to_owned())]);
    assert_eq!(journal.records()[0].state, JobState::Cancelled);
    assert_eq!(service.sent().len(), 2, "nothing more was sent");
}

/// Ruling Q49 (a): Discard on the resume banner gives up the uploads it drops, through the
/// site's client; a delete's "possibly sent" mark is no upload, and is left alone (C2 b).
#[tokio::test(start_paused = true)]
async fn discarding_the_journal_aborts_the_uploads_it_drops() {
    let dir = tempfile::tempdir().unwrap();
    let (_, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    crash_after(&journal, &service, job, 3).await;
    // A delete that may have been sent, with its mark in the same column.
    let mut sent_delete = vec![JobRecord {
        id: 0,
        batch: "b".into(),
        site: site(),
        job: Job::Delete {
            remote: p("old.txt"),
            folder: false,
        },
        state: JobState::Queued,
        error: None,
        multipart: Some(MultipartProgress {
            upload_id: "httporter:delete-sent".into(),
            part_size: 0,
            parts: Vec::new(),
            file: None,
        }),
    }];
    journal.insert(&mut sent_delete).unwrap();

    let b = engine_on(journal.clone());
    assert_eq!(b.unfinished().total, 2);
    assert_eq!(b.unfinished().deletes, 1);
    b.discard_journal(&only_site(service.clone()));
    wait_idle(&b).await;
    assert_eq!(service.aborted(), [(p(OBJECT), "u-1".to_owned())]);
    assert_eq!(b.unfinished_count(), 0);
}

/// Ruling Q49 (a): removing a site gives up the uploads a crash left for it.
#[tokio::test(start_paused = true)]
async fn forgetting_a_site_aborts_the_uploads_left_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    crash_after(&journal, &service, job, 3).await;

    let b = engine_on(journal.clone());
    let storage: Arc<dyn StorageProvider> = service.clone();
    b.forget_site(&site(), Some(storage));
    wait_idle(&b).await;
    assert_eq!(service.aborted(), [(p(OBJECT), "u-1".to_owned())]);
    assert_eq!(journal.records()[0].state, JobState::Cancelled);
    assert_eq!(b.unfinished_count(), 0);
}

/// A journal over `records` that counts the Cancelled states written for each record, one at a
/// time or together.
struct CountingCancels {
    records: Arc<MemoryJournal>,
    cancels: Mutex<HashMap<i64, usize>>,
}

impl CountingCancels {
    fn new(records: Arc<MemoryJournal>) -> Arc<Self> {
        Arc::new(Self {
            records,
            cancels: Mutex::default(),
        })
    }

    fn note(&self, id: i64, state: JobState) {
        if state == JobState::Cancelled {
            *self.cancels.lock().unwrap().entry(id).or_default() += 1;
        }
    }

    fn cancels(&self, id: i64) -> usize {
        self.cancels.lock().unwrap().get(&id).copied().unwrap_or(0)
    }
}

impl Journal for CountingCancels {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError> {
        self.records.insert(records)
    }
    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError> {
        self.note(id, state);
        self.records.set_state(id, state, error)
    }
    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError> {
        self.records.set_multipart(id, mp)
    }
    fn apply(&self, updates: &[JournalUpdate]) -> Result<(), JournalError> {
        for update in updates {
            if let JournalUpdate::State { id, state, .. } = update {
                self.note(*id, *state);
            }
        }
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

/// Review N2: removing a site whose upload the engine holds (resumed, waiting in the queue)
/// gives it up once and writes its cancel once. The engine's own cancel does both; the site's
/// records it doesn't hold are the only ones `forget_site` writes and gives up itself.
#[tokio::test(start_paused = true)]
async fn forgetting_a_site_gives_up_a_held_upload_once() {
    let dir = tempfile::tempdir().unwrap();
    let (_, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    crash_after(&journal, &service, job, 3).await;

    let counting = CountingCancels::new(journal.clone());
    let b = engine_on(counting.clone());
    b.pause_all();
    assert_eq!(b.resume_from_journal(&only_site(service.clone())), 1);
    let storage: Arc<dyn StorageProvider> = service.clone();
    b.forget_site(&site(), Some(storage));
    let mark = b.journal_mark();
    assert!(!b.journal_reached(mark), "the cancel is on its way");
    wait_idle(&b).await;
    assert!(b.journal_reached(mark));
    assert!(!b.site_has_work(&site()));
    assert_eq!(
        service.aborted(),
        [(p(OBJECT), "u-1".to_owned())],
        "given up once"
    );
    let record = journal.records()[0].clone();
    assert_eq!(record.state, JobState::Cancelled);
    assert_eq!(counting.cancels(record.id), 1, "written once");
    assert_eq!(b.unfinished_count(), 0);
}

/// Ruling Q49 (c): a file that changed since its parts were stored (another size or time) is
/// sent again as a new upload; the old one is given up.
#[tokio::test(start_paused = true)]
async fn a_file_that_changed_since_its_parts_were_stored_starts_over() {
    let dir = tempfile::tempdir().unwrap();
    let (local, job) = big_file(dir.path());
    let journal = Arc::new(MemoryJournal::new());
    let service = PartsSite::new();
    crash_after(&journal, &service, job, 3).await;
    std::fs::write(&local, b"ABCDEFGHIJKLMNOPQRSTUVWX").unwrap();

    let b = engine_on(journal.clone());
    b.resume_from_journal(&only_site(service.clone()));
    let s = wait_idle(&b).await;
    assert_eq!(s.files_done, 1, "{}", brief(&s));
    assert_eq!(service.creates(), 2);
    assert_eq!(service.completed(), ["u-2"]);
    assert_eq!(service.aborted(), [(p(OBJECT), "u-1".to_owned())]);
    let new: Vec<u32> = service
        .sent()
        .into_iter()
        .filter(|(id, _)| id == "u-2")
        .map(|(_, n)| n)
        .collect();
    assert_eq!(new, [1, 2, 3, 4, 5, 6], "every part of the new file");
}
