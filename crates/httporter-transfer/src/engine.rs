//! The engine: every job's state, one lane per site, the dispatcher that starts work, and the
//! snapshot the transfer drawer reads.
//!
//! All state sits behind one mutex, held only for short, synchronous updates (never across an
//! `.await`, never while telling watchers about a change). Units of work run in their own tasks
//! and report back through [`Inner`]'s methods. The journal is written by a single task, in the
//! order the changes happened, so a job's last record is always its latest state.
//!
//! A delete whose request may have reached the server is marked in the journal through the
//! record's multipart column, which a delete never otherwise uses: a `Delete` record with
//! `multipart: Some(_)` (the [`DELETE_SENT`] marker) was possibly sent, so after a crash a
//! `NotFound` for it means done (ledger 179). This keeps the journal's shape as it is.
//!
//! An upload's column holds its multipart progress (ruling Q49): every upload is given a
//! [`JobParts`] part journal, which keeps the progress on the job's entry and writes it through
//! the same ordered writer, so an upload interrupted by a crash, a pause, an offline spell or a
//! failed try carries on from the parts already stored. Only an upload's part journal writes
//! the column, and only a delete's record is read as possibly sent, so the two uses never meet.
//! An upload the user cancels or discards is given up on the service
//! ([`StorageProvider::abort_multipart`]) by a task the app waits for as it closes
//! ([`Engine::pending_aborts`]), so its stored parts aren't left to be billed.
//!
//! On a service that can delete a cut-off upload's path late
//! ([`Capabilities::cut_uploads_delete_late`](httporter_core::Capabilities), Bunny Storage), an
//! upload is marked in the journal as possibly sent before its first request goes out, the way a
//! delete is: the [`UPLOAD_SENT`] marker in the multipart column, which such an upload doesn't
//! otherwise use (ruling Q55). The unit waits until the journal has the mark before it sends.
//! An upload sent again after an earlier request for it may have been cut off (a crash or a quit,
//! read back from the mark, or a pause, an offline spell or an auth stop in this run) is not done
//! in the journal when the service answers: it waits, counted done in the drawer, until its
//! site's lane has nothing left to do and [`CHECK_AFTER`] has passed since that answer, and is
//! then looked for in its folder's listing, one listing per folder. There with its size, it is
//! done; missing or of another size, it is sent again, and after [`MAX_CHECKS`] such checks it
//! fails. A quit before the check leaves its record unfinished, so the next start sends it and
//! checks it again. An upload sent for the first time is never checked.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::Utc;
use httporter_core::{
    BatchSummary, Checksum, Direction, EngineSnapshot, EngineState, FileStamp, Job, JobId, JobKind,
    JobRecord, JobState, JobView, Journal, JournalError, JournalUpdate, Listing, MultipartProgress,
    PartJournal, ProviderError, Rechecked, RemotePath, RetryPolicy, SiteId, StorageProvider,
    TransferSettings, with_retry,
};
use tokio::runtime::Handle;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::time::Instant;

use crate::bucket::TokenBucket;
use crate::lane::{
    ActiveUnit, Failure, Lane, LaneState, SlotKind, UnitId, UnitSpec, Work, run_unit,
};
use crate::local::{DiskFiles, LocalFiles};
use crate::messages;
use crate::offline::{self, MAX_STRIKES, drive_of};
use crate::rate::{RateWindow, eta};

/// The `upload_id` of the multipart progress that marks a delete as possibly sent.
pub(crate) const DELETE_SENT: &str = "httporter:delete-sent";

/// The `upload_id` of the multipart progress that marks an upload as possibly sent, on a service
/// that can delete a cut-off upload's path late (ruling Q55). Part of the file format: never
/// rename it.
pub(crate) const UPLOAD_SENT: &str = "httporter:upload-sent";

/// How long after an upload sent again was answered it is checked (ruling Q55). Bunny Storage
/// deleted a cut-off upload's path when it ended that upload: at once when it saw the connection
/// close, and 54 to 59 s after a newer upload's answer (about a minute after the cut-off
/// upload's last bytes) when it never did (live probes, `httporter-bunny/tests/live.rs`). Three
/// times that, for a loaded server: S5's upload calls took up to 65 s.
pub const CHECK_AFTER: Duration = Duration::from_secs(180);

/// How long after a check whose listing failed it is tried again. It doesn't count as a check.
const CHECK_RETRY: Duration = Duration::from_secs(60);

/// How often a check that is due looks again whether its site's lane has finished its work.
const CHECK_POLL: Duration = Duration::from_secs(5);

/// How many times an upload may be found missing after it was answered, and sent again, before
/// it fails (ruling Q55).
pub const MAX_CHECKS: u8 = 3;

/// Whether `progress` is a real multipart upload on the service, one that can be resumed or must
/// be given up: not the "possibly sent" mark of a delete or of an upload.
pub(crate) fn is_upload(progress: &MultipartProgress) -> bool {
    !progress.upload_id.is_empty()
        && progress.upload_id != DELETE_SENT
        && progress.upload_id != UPLOAD_SENT
}

/// The multipart progress a journal record carries for its job: an upload's own, never a
/// "possibly sent" mark (review C2 b, ruling Q55).
fn upload_progress(job: &Job, multipart: Option<MultipartProgress>) -> Option<MultipartProgress> {
    match job {
        Job::Upload { .. } => multipart.filter(is_upload),
        _ => None,
    }
}

/// Whether a journal record's request may have reached the server: a delete with anything in
/// its column (review C2 b), or an upload whose column holds the [`UPLOAD_SENT`] mark, never
/// its multipart progress (ruling Q55).
fn possibly_sent(job: &Job, multipart: Option<&MultipartProgress>) -> bool {
    match job {
        Job::Delete { .. } => multipart.is_some(),
        Job::Upload { .. } => multipart.is_some_and(|progress| progress.upload_id == UPLOAD_SENT),
        Job::Download { .. } | Job::CreateFolder { .. } => false,
    }
}

/// What didn't finish last time, for the resume banner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unfinished {
    pub total: usize,
    /// How many of them are deletes, which Resume carries out.
    pub deletes: usize,
}

/// A site as [`Engine::resume_from_journal`] needs it: its name, its host, and its client.
pub type SiteInfo = (String, String, Arc<dyn StorageProvider>);

/// What a site the journal names is now; `None` when the site no longer exists.
pub type SiteLookup<'a> = &'a dyn Fn(&SiteId) -> Option<SiteInfo>;

/// How many jobs each of a snapshot's lists shows at most.
pub const WINDOW: usize = 200;

/// How many job events wait for a slow receiver before it loses the oldest.
const EVENTS: usize = 4096;

/// The journal writer takes at most this many updates in one go.
const JOURNAL_CHUNK: usize = 512;

/// The waits before a chunk the journal failed to write is tried again, once after each.
const JOURNAL_RETRIES: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];

/// Jobs for one site, submitted together.
pub struct Batch {
    pub site: SiteId,
    pub site_name: String,
    pub host: String,
    /// The site's client; the newest batch's client is the one the site's lane uses.
    pub provider: Arc<dyn StorageProvider>,
    pub jobs: Vec<Job>,
    /// The listed SHA-256 or MD5 of files to download, by remote path, so what arrives is
    /// checked (ruling Q27). Not journalled: a download resumed after a crash has no hint and
    /// completes unverified.
    pub checksums: HashMap<RemotePath, Checksum>,
}

/// What became of each job of a batch submitted with [`Engine::submit_tracked`], by remote path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    pub done: Vec<RemotePath>,
    pub failed: Vec<(RemotePath, String)>,
    pub cancelled: Vec<RemotePath>,
    pub bytes_done: u64,
}

/// Runs transfer jobs: journalled before they start, per-site lanes with adaptive limits,
/// retries, a global bandwidth limit, and a cheap snapshot for the UI.
///
/// Clones share one engine. When the last clone is dropped the engine stops: nothing new
/// starts, work in flight is interrupted, and the journal keeps every unfinished job as Queued
/// for a later resume.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
    _owner: Arc<Owner>,
}

/// Shuts the engine down when the last [`Engine`] clone goes.
struct Owner(Arc<Inner>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.shut_down();
    }
}

/// What the engine's tasks share.
pub(crate) struct Inner {
    rt: Handle,
    journal: Arc<dyn Journal>,
    writer: JournalWriter,
    aborts: Aborts,
    describe: fn(&ProviderError) -> String,
    pub(crate) bucket: TokenBucket,
    state: Mutex<State>,
    /// Wakes the dispatcher.
    wake: Notify,
    /// Wakes the checker (ruling Q55): a check was added, or the engine is stopping.
    check_wake: Notify,
    changes: Arc<watch::Sender<u64>>,
}

/// Multipart uploads being given up (ruling Q49): each `abort_multipart` runs in its own task,
/// counted while it runs so that closing can wait for it. Best effort: a failed abort only
/// leaves the parts where they are.
#[derive(Clone)]
struct Aborts {
    rt: Handle,
    running: Arc<AtomicUsize>,
    changes: Arc<watch::Sender<u64>>,
}

/// Counts one abort as running until it is dropped: when the task ends, or is dropped unrun.
/// Only counts: a task dropped unrun (the runtime shutting down) is dropped where it was
/// spawned, which may be with the engine's state locked, where watchers must not be told.
struct AbortRunning(Arc<AtomicUsize>);

impl Drop for AbortRunning {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Aborts {
    /// Gives up the upload `upload_id` of `remote` through `provider`, in a task, which tells
    /// the watchers once it is no longer counted.
    fn send(&self, provider: Arc<dyn StorageProvider>, remote: RemotePath, upload_id: String) {
        self.running.fetch_add(1, Ordering::SeqCst);
        let running = AbortRunning(self.running.clone());
        let changes = self.changes.clone();
        crate::task::spawn_on(&self.rt, async move {
            let _ = provider.abort_multipart(&remote, &upload_id).await;
            drop(running);
            changes.send_modify(|n| *n = n.wrapping_add(1));
        });
    }

    /// Gives up the multipart upload of `job`, if `progress` is one.
    fn give_up(
        &self,
        provider: &Arc<dyn StorageProvider>,
        job: &Job,
        progress: Option<&MultipartProgress>,
    ) {
        if let (Job::Upload { remote, .. }, Some(progress)) = (job, progress)
            && is_upload(progress)
        {
            self.send(provider.clone(), remote.clone(), progress.upload_id.clone());
        }
    }

    fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }
}

/// Where a job stands in memory. A cancelled job is let go at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Queued,
    Active(UnitId),
    Done,
    Failed,
}

/// Why a unit in flight is being interrupted. A cancel outranks a requeue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stop {
    /// Back to the front of the queue (pause, auth stop, shutdown).
    Requeue,
    Cancel,
}

/// One job, in memory.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) job: Job,
    lane: usize,
    batch: u64,
    pub(crate) status: Status,
    /// Skipped by the dispatcher until resumed (queued jobs only).
    pub(crate) paused: bool,
    size: u64,
    done_bytes: u64,
    error: Option<String>,
    stop: Option<Stop>,
    /// A request for it may have reached the server. For a delete, a later `NotFound` then means
    /// done; for an upload on a service that can delete a cut-off upload late, the next one that
    /// succeeds is checked (ruling Q55).
    sent: bool,
    /// The upload in flight was sent with `sent` already set: once answered, it is checked
    /// before it is done in the journal (ruling Q55).
    recheck: bool,
    /// How many checks found it missing, each followed by sending it again (ruling Q55).
    checks: u8,
    /// What a download's contents must hash to, when its batch said.
    checksum: Option<Checksum>,
    /// How many offline cycles its own network errors started or met (see [`MAX_STRIKES`]).
    strikes: u8,
    /// The lane's offline cycle of its latest strike: one strike per cycle.
    struck: Option<u64>,
    /// An upload's multipart progress, as the journal holds it (ruling Q49). Never a delete's
    /// "possibly sent" mark.
    multipart: Option<MultipartProgress>,
}

/// Every job the engine still needs to know about: queued, active, failed, and the done ones
/// in the snapshot's window. A `Vec` of slots with an index by id; freed slots are reused.
#[derive(Debug, Default)]
pub(crate) struct Slab {
    slots: Vec<Option<Entry>>,
    free: Vec<usize>,
    index: HashMap<JobId, usize>,
}

impl Slab {
    fn insert(&mut self, id: JobId, entry: Entry) {
        let slot = match self.free.pop() {
            Some(slot) => {
                self.slots[slot] = Some(entry);
                slot
            }
            None => {
                self.slots.push(Some(entry));
                self.slots.len() - 1
            }
        };
        if let Some(old) = self.index.insert(id, slot) {
            // Ids are never reused, so this is a journal handing one out twice: keep the newer.
            self.slots[old] = None;
            self.free.push(old);
        }
    }

    pub(crate) fn get(&self, id: JobId) -> Option<&Entry> {
        self.slots[*self.index.get(&id)?].as_ref()
    }

    fn get_mut(&mut self, id: JobId) -> Option<&mut Entry> {
        self.slots[*self.index.get(&id)?].as_mut()
    }

    fn remove(&mut self, id: JobId) -> Option<Entry> {
        let slot = self.index.remove(&id)?;
        self.free.push(slot);
        self.slots[slot].take()
    }

    /// The ids of every job on `lane`, whatever its status.
    fn on_lane(&self, lane: usize) -> HashSet<JobId> {
        self.index
            .iter()
            .filter(|&(_, &slot)| {
                self.slots[slot]
                    .as_ref()
                    .is_some_and(|entry| entry.lane == lane)
            })
            .map(|(&id, _)| id)
            .collect()
    }
}

/// Which ways the jobs of a run or a batch go.
#[derive(Debug, Default, Clone, Copy)]
struct Kinds {
    up: bool,
    down: bool,
    delete: bool,
}

impl Kinds {
    fn note(&mut self, kind: JobKind) {
        match kind {
            JobKind::Upload | JobKind::CreateFolder => self.up = true,
            JobKind::Download => self.down = true,
            JobKind::Delete => self.delete = true,
        }
    }

    fn direction(self) -> Direction {
        match (self.up, self.down, self.delete) {
            (_, false, false) => Direction::Upload,
            (false, true, false) => Direction::Download,
            (false, false, true) => Direction::Delete,
            _ => Direction::Mixed,
        }
    }
}

/// The totals the drawer shows. A run begins when work is submitted to an engine with nothing
/// queued or running; failed jobs carry over into it, since they can still be retried.
#[derive(Debug, Default)]
struct Run {
    files_total: u64,
    files_done: u64,
    bytes_total: u64,
    /// The sizes of the jobs done.
    bytes_finished: u64,
    /// The bytes counted so far by jobs in flight.
    bytes_in_flight: u64,
    /// The sizes of the failed jobs, which are not left to transfer.
    bytes_failed: u64,
    kinds: Kinds,
    site: String,
    host: String,
}

/// An upload sent again after a request for it may have been cut off, counted done but not yet
/// in the journal, waiting to be looked for on its site (ruling Q55). Kept apart from the jobs,
/// which let a done job go.
#[derive(Debug)]
struct Check {
    /// To queue it again if it has gone.
    job: Job,
    lane: usize,
    batch: u64,
    /// The size it was sent with: what the listing must show.
    size: u64,
    /// When it may be looked for: [`CHECK_AFTER`] after its answer.
    due: Instant,
    /// Which check this is for it, from 1.
    round: u8,
    /// The run it was counted done in ([`State::run_seq`]).
    run: u64,
}

/// One folder's uploads that are due for their check, and how to list it.
struct CheckFolder {
    dir: RemotePath,
    ids: Vec<JobId>,
    provider: Arc<dyn StorageProvider>,
    policy: RetryPolicy,
}

/// What the checker is to do next.
enum Due {
    /// Nothing waits to be checked.
    Nothing,
    /// Look again then.
    At(Instant),
    /// List these folders now.
    Now(Vec<CheckFolder>),
}

/// A batch submitted with [`Engine::submit_tracked`]: how each of its jobs ended so far, kept
/// until [`Engine::wait_batch`] takes it.
#[derive(Debug)]
struct Tracked {
    /// The id [`Engine::submit_tracked`] returned.
    id: String,
    lane: usize,
    jobs: HashMap<JobId, TrackedJob>,
}

#[derive(Debug)]
struct TrackedJob {
    remote: RemotePath,
    /// `None` while it is queued or running, or once a check sent it again.
    end: Option<End>,
}

/// How a job of a tracked batch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum End {
    Done { bytes: u64 },
    Failed(String),
    Cancelled,
}

impl Tracked {
    /// Every job has ended (a failed one too).
    fn closed(&self) -> bool {
        self.jobs.values().all(|job| job.end.is_some())
    }

    /// The outcome; a job not ended yet counts as cancelled (the engine stopped).
    fn outcome(self) -> BatchOutcome {
        let mut jobs: Vec<(JobId, TrackedJob)> = self.jobs.into_iter().collect();
        jobs.sort_unstable_by_key(|(id, _)| *id);
        let mut outcome = BatchOutcome::default();
        for (_, job) in jobs {
            match job.end {
                Some(End::Done { bytes }) => {
                    outcome.done.push(job.remote);
                    outcome.bytes_done += bytes;
                }
                Some(End::Failed(reason)) => outcome.failed.push((job.remote, reason)),
                Some(End::Cancelled) | None => outcome.cancelled.push(job.remote),
            }
        }
        outcome
    }
}

/// A submitted batch, until all of its jobs have ended.
#[derive(Debug)]
struct BatchInfo {
    site: String,
    /// Queued or active.
    open: u64,
    failed: u64,
    done_files: u64,
    done_bytes: u64,
    kinds: Kinds,
}

struct State {
    settings: TransferSettings,
    /// The user paused everything.
    paused: bool,
    shut_down: bool,
    jobs: Slab,
    lanes: Vec<Lane>,
    lane_of: HashMap<SiteId, usize>,
    batches: HashMap<u64, BatchInfo>,
    next_batch: u64,
    next_unit: u64,
    run: Run,
    failed: BTreeSet<JobId>,
    /// The latest done jobs, oldest first, at most [`WINDOW`].
    done: VecDeque<JobId>,
    last_batch: Option<BatchSummary>,
    rates: RateWindow,
    /// A download found its drive full: (drive, download bytes still to come on it). Nothing
    /// starts until the user resumes.
    disk_full: Option<(String, u64)>,
    /// The local file operations: the disk, unless a test stands in for it.
    files: Arc<dyn LocalFiles>,
    /// Where cancelled uploads are given up.
    aborts: Aborts,
    /// Uploads waiting for their check (ruling Q55), by id.
    checks: HashMap<JobId, Check>,
    /// Counts the runs, so a check knows whether the run it was counted done in is still the
    /// one the drawer shows.
    run_seq: u64,
    /// What the checks found since the engine started.
    rechecked: Rechecked,
    /// The batches submitted with [`Engine::submit_tracked`], by their number, until
    /// [`Engine::wait_batch`] takes them.
    tracked: HashMap<u64, Tracked>,
    /// Where every job that ends is told ([`Engine::events`]).
    events: broadcast::Sender<JobEvent>,
    /// The `Cache-Control` rules of each site: (folder, value).
    cache_rules: HashMap<SiteId, Vec<(RemotePath, String)>>,
}

/// How a job ended, for [`JobEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEventOutcome {
    Done,
    /// Failed, with the words of the Failed tab.
    Failed(String),
    Cancelled,
}

/// A job reached its end (done, failed or cancelled): one per job, from [`Engine::events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEvent {
    pub site: SiteId,
    pub site_name: String,
    pub kind: JobKind,
    pub path: String,
    pub bytes: u64,
    pub outcome: JobEventOutcome,
}

impl Engine {
    /// The jobs that end from now on, one [`JobEvent`] each (a channel of 4096; a receiver that
    /// lags loses the oldest).
    ///
    /// An upload that waits for its late-delete check (ruling Q55) is told done once the check
    /// found it, so nothing hears "done" of a file the site may still drop.
    pub fn events(&self) -> broadcast::Receiver<JobEvent> {
        self.inner.lock().events.subscribe()
    }

    /// Uploads under `prefix` on `site` are sent with this `Cache-Control` (the longest prefix
    /// wins); `None` removes the rule. Uploads queued already take it when they start.
    pub fn set_cache_control(&self, site: &SiteId, prefix: &RemotePath, value: Option<String>) {
        let mut st = self.inner.lock();
        let rules = st.cache_rules.entry(site.clone()).or_default();
        rules.retain(|(folder, _)| folder != prefix);
        if let Some(value) = value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            rules.push((prefix.clone(), value));
        }
        if rules.is_empty() {
            st.cache_rules.remove(site);
        }
    }

    /// `error` in the words of the Failed tab (the app's `describe`).
    pub(crate) fn describe(&self, error: &ProviderError) -> String {
        (self.inner.describe)(error)
    }
}

/// The value of the longest of `rules` (folder, value) that `path` is within.
fn cache_control_for(rules: &[(RemotePath, String)], path: &RemotePath) -> Option<String> {
    rules
        .iter()
        .filter(|(folder, _)| path.is_within(folder))
        .max_by_key(|(folder, _)| folder.segments().count())
        .map(|(_, value)| value.clone())
}

impl Engine {
    /// An idle engine. `describe` turns an error into the plain words shown in the Failed tab
    /// (the app passes its copy), so the engine holds no UI wording.
    pub fn new(
        rt: Handle,
        journal: Arc<dyn Journal>,
        settings: TransferSettings,
        describe: fn(&ProviderError) -> String,
    ) -> Self {
        let changes = Arc::new(watch::channel(0u64).0);
        let writer = JournalWriter::spawn(&rt, journal.clone(), changes.clone());
        let aborts = Aborts {
            rt: rt.clone(),
            running: Arc::new(AtomicUsize::new(0)),
            changes: changes.clone(),
        };
        let inner = Arc::new(Inner {
            rt: rt.clone(),
            journal,
            writer,
            aborts: aborts.clone(),
            describe,
            bucket: TokenBucket::new(settings.bandwidth_limit_bytes()),
            state: Mutex::new(State::new(settings, aborts)),
            wake: Notify::new(),
            check_wake: Notify::new(),
            changes,
        });
        crate::task::spawn_on(&rt, dispatch_loop(inner.clone()));
        crate::task::spawn_on(&rt, check_loop(inner.clone()));
        Self {
            _owner: Arc::new(Owner(inner.clone())),
            inner,
        }
    }

    /// New settings, for work starting from now on (and the bandwidth limit at once). Settings
    /// a site's client is built with (the proxy, the protocol, its own transfer slots) reach the
    /// work already queued through [`Self::replace_provider`].
    pub fn set_settings(&self, settings: TransferSettings) {
        self.inner.bucket.set_rate(settings.bandwidth_limit_bytes());
        {
            let mut st = self.inner.lock();
            for lane in &mut st.lanes {
                lane.configure(&settings);
            }
            st.settings = settings;
        }
        self.inner.poke();
    }

    /// Hands `site` a new client (reviews I3, M2): rebuilt for new settings, or for the edited
    /// site. The units that start from now on and the offline probe use it; the units in flight
    /// finish with the one they started with. Nothing else about the site's lane changes: a
    /// stopped lane stays stopped ([`Self::resume_site`] restarts one).
    pub fn replace_provider(&self, site: &SiteId, provider: Arc<dyn StorageProvider>) {
        {
            let mut guard = self.inner.lock();
            let State {
                lanes,
                lane_of,
                settings,
                ..
            } = &mut *guard;
            let Some(&lane) = lane_of.get(site) else {
                return;
            };
            lanes[lane].provider = provider;
            lanes[lane].configure(settings);
        }
        self.inner.poke();
    }

    /// The sites the engine has a lane for: those it was given work for since it started.
    pub fn sites(&self) -> Vec<SiteId> {
        self.inner.lock().lane_of.keys().cloned().collect()
    }

    /// The client `site`'s lane starts its units with, if it has a lane.
    pub fn site_provider(&self, site: &SiteId) -> Option<Arc<dyn StorageProvider>> {
        let st = self.inner.lock();
        let lane = *st.lane_of.get(site)?;
        Some(st.lanes[lane].provider.clone())
    }

    /// Journals every job of `batch` (one transaction), then queues them on the site's lane.
    /// Returns the batch id (a UUID). A batch the journal can't record is refused whole, and
    /// nothing of it runs (ruling Q21). A batch without jobs changes nothing.
    pub fn submit(&self, batch: Batch) -> Result<String, JournalError> {
        self.submit_batch(batch, false)
    }

    /// As [`Self::submit`]; `tracked` keeps each job's outcome for [`Self::wait_batch`].
    fn submit_batch(&self, batch: Batch, tracked: bool) -> Result<String, JournalError> {
        let Batch {
            site,
            site_name,
            host,
            provider,
            jobs,
            checksums,
        } = batch;
        let batch_id = uuid::Uuid::new_v4().to_string();
        if jobs.is_empty() {
            // Nothing to run, so the run's totals stay as they are (ruling Q37).
            return Ok(batch_id);
        }
        let mut records: Vec<JobRecord> = jobs
            .into_iter()
            .map(|job| JobRecord {
                id: 0,
                batch: batch_id.clone(),
                site: site.clone(),
                job,
                state: JobState::Queued,
                error: None,
                multipart: None,
            })
            .collect();
        self.inner.journal.insert(&mut records)?;
        let ids = records
            .iter()
            .map(|record| job_id(record.id))
            .collect::<Option<Vec<JobId>>>()
            .ok_or_else(|| JournalError("the journal handed out an id below 1".into()))?;
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            if !st.has_work() {
                st.start_run(Instant::now());
            }
            let lane = st.lane_for(site, site_name.clone(), host.clone(), provider);
            st.run.site.clone_from(&site_name);
            st.run.host = host;
            let seq = st.next_batch;
            st.next_batch += 1;
            let mut info = BatchInfo {
                site: site_name,
                open: 0,
                failed: 0,
                done_files: 0,
                done_bytes: 0,
                kinds: Kinds::default(),
            };
            let mut track = tracked.then(|| Tracked {
                id: batch_id.clone(),
                lane,
                jobs: HashMap::new(),
            });
            for (id, record) in ids.into_iter().zip(records) {
                let job = record.job;
                let size = size_of(&job);
                let checksum = match &job {
                    Job::Download { remote, .. } => checksums.get(remote).cloned(),
                    _ => None,
                };
                if let Some(track) = &mut track {
                    let remote = crate::lane::remote_of(&job).clone();
                    track.jobs.insert(id, TrackedJob { remote, end: None });
                }
                info.open += 1;
                info.kinds.note(job.kind());
                st.run.kinds.note(job.kind());
                st.run.files_total += 1;
                st.run.bytes_total += size;
                st.lanes[lane].queue.push_back(id);
                let mut entry = Entry::new(job, lane, seq, size);
                entry.checksum = checksum;
                st.jobs.insert(id, entry);
            }
            if info.open > 0 {
                st.batches.insert(seq, info);
            }
            if let Some(track) = track {
                st.tracked.insert(seq, track);
            }
        }
        self.inner.poke();
        Ok(batch_id)
    }

    /// Pauses everything: nothing new starts, and work in flight goes back to the queue.
    pub fn pause_all(&self) {
        {
            let mut st = self.inner.lock();
            st.paused = true;
            st.interrupt_all(Stop::Requeue);
        }
        self.inner.poke();
    }

    /// Lifts a pause, and the stop a full disk made (the user freed some space).
    pub fn resume_all(&self) {
        {
            let mut st = self.inner.lock();
            st.paused = false;
            st.disk_full = None;
        }
        self.inner.poke();
    }

    /// Cancels everything not yet done: queued, paused, running and failed jobs alike, and
    /// lifts a pause (or a full disk's stop).
    pub fn cancel_all(&self) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            st.paused = false;
            st.disk_full = None;
            for lane in 0..st.lanes.len() {
                let queued: Vec<JobId> = st.lanes[lane].queue.drain(..).collect();
                st.lanes[lane].paused = 0;
                for id in queued {
                    st.mark_cancelled(id, &self.inner.writer);
                }
            }
            let failed: Vec<JobId> = st.failed.iter().copied().collect();
            for id in failed {
                st.mark_cancelled(id, &self.inner.writer);
            }
            st.interrupt_all(Stop::Cancel);
        }
        self.inner.poke();
    }

    /// Holds a queued job back until [`resume`](Self::resume); the jobs behind it go ahead.
    pub fn pause(&self, id: JobId) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get_mut(id) else {
                return;
            };
            if entry.status != Status::Queued || entry.paused {
                return;
            }
            entry.paused = true;
            let lane = entry.lane;
            st.lanes[lane].paused += 1;
        }
        self.inner.poke();
    }

    pub fn resume(&self, id: JobId) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get_mut(id) else {
                return;
            };
            if entry.status != Status::Queued || !entry.paused {
                return;
            }
            entry.paused = false;
            let lane = entry.lane;
            st.lanes[lane].paused -= 1;
        }
        self.inner.poke();
    }

    /// Cancels one job: queued, running (it is interrupted) or failed.
    pub fn cancel(&self, id: JobId) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get(id) else {
                return;
            };
            match entry.status {
                Status::Queued => {
                    let (lane, paused) = (entry.lane, entry.paused);
                    if st.unqueue(lane, id) && paused {
                        st.lanes[lane].paused -= 1;
                    }
                    st.mark_cancelled(id, &self.inner.writer);
                }
                Status::Failed => st.mark_cancelled(id, &self.inner.writer),
                Status::Active(unit) => {
                    let lane = entry.lane;
                    if let Some(entry) = st.jobs.get_mut(id) {
                        entry.stop = Some(Stop::Cancel);
                    }
                    // The rest of its unit (other deletes of a batch) goes back to the queue.
                    if let Some(active) = st.lanes[lane].units.get(&unit) {
                        interrupt(&mut st.jobs, unit, active, Stop::Requeue);
                    }
                }
                Status::Done => return,
            }
        }
        self.inner.poke();
    }

    /// Sends failed jobs again, at the back of their queues. Other ids are ignored.
    pub fn retry(&self, ids: &[JobId]) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            for &id in ids {
                st.requeue_failed(id, &self.inner.writer);
            }
        }
        self.inner.poke();
    }

    pub fn retry_all_failed(&self) {
        let failed: Vec<JobId> = self.inner.lock().failed.iter().copied().collect();
        self.retry(&failed);
    }

    /// Makes a queued job the next to start on its site.
    pub fn move_to_top(&self, id: JobId) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get(id) else {
                return;
            };
            if entry.status != Status::Queued {
                return;
            }
            let lane = entry.lane;
            if st.unqueue(lane, id) {
                st.lanes[lane].queue.push_front(id);
            }
        }
        self.inner.poke();
    }

    /// Restarts a site stopped by an auth failure, with the edited site's client, or one
    /// stopped by a clock that was off (review I4), with the client it had.
    pub fn resume_site(&self, site: &SiteId, provider: Arc<dyn StorageProvider>) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let Some(&lane) = st.lane_of.get(site) else {
                return;
            };
            let lane = &mut st.lanes[lane];
            lane.provider = provider;
            lane.state = LaneState::Running;
            lane.configure(&st.settings);
        }
        self.inner.poke();
    }

    /// Whether `site` is stopped by an auth failure (its jobs wait for [`Self::resume_site`]).
    pub fn is_auth_stopped(&self, site: &SiteId) -> bool {
        let st = self.inner.lock();
        st.lane_of
            .get(site)
            .is_some_and(|&lane| st.lanes[lane].state == LaneState::AuthStopped)
    }

    /// The site the sign-in banner is about: the first one stopped by an auth failure that still
    /// has work (the one [`EngineState::AuthStopped`] names).
    pub fn auth_stopped_site(&self) -> Option<SiteId> {
        let st = self.inner.lock();
        let lane = st
            .lanes
            .iter()
            .position(|lane| lane.state == LaneState::AuthStopped && lane.has_work())?;
        st.lane_of
            .iter()
            .find(|&(_, &index)| index == lane)
            .map(|(site, _)| site.clone())
    }

    /// The site the clock banner is about (review I4): the first one stopped because it found
    /// the PC's clock too far off that still has work (the one [`EngineState::ClockSkew`]
    /// names), with the client its lane has, for [`Self::resume_site`] once the clock is set.
    pub fn clock_skewed_site(&self) -> Option<(SiteId, Arc<dyn StorageProvider>)> {
        let st = self.inner.lock();
        let lane = st
            .lanes
            .iter()
            .position(|lane| lane.state == LaneState::ClockSkew && lane.has_work())?;
        let site = st
            .lane_of
            .iter()
            .find(|&(_, &index)| index == lane)
            .map(|(site, _)| site.clone())?;
        Some((site, st.lanes[lane].provider.clone()))
    }

    /// Cancels everything of `site` not yet done: its queued, paused, running and failed jobs
    /// (the site is being removed). The other sites carry on.
    pub fn cancel_site(&self, site: &SiteId) {
        self.cancel_site_held(site);
    }

    /// As [`Self::cancel_site`]; returns the ids of every job of the site the engine held, done
    /// ones included: their records are the engine's to write. Its uploads waiting for their
    /// check are not checked: their records are marked Cancelled (ruling Q55).
    fn cancel_site_held(&self, site: &SiteId) -> HashSet<JobId> {
        let held = {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let Some(&lane) = st.lane_of.get(site) else {
                return HashSet::new();
            };
            let mut held = st.jobs.on_lane(lane);
            let checked: Vec<JobId> = st
                .checks
                .iter()
                .filter(|(_, check)| check.lane == lane)
                .map(|(&id, _)| id)
                .collect();
            for id in checked {
                if let Some(check) = st.checks.remove(&id) {
                    st.track(check.batch, id, Some(End::Cancelled));
                    st.emit(check.lane, &check.job, 0, JobEventOutcome::Cancelled);
                }
                self.inner.writer.set_state(id, JobState::Cancelled, None);
                held.insert(id);
            }
            let queued: Vec<JobId> = st.lanes[lane].queue.drain(..).collect();
            st.lanes[lane].paused = 0;
            for id in queued {
                st.mark_cancelled(id, &self.inner.writer);
            }
            let failed: Vec<JobId> = st
                .failed
                .iter()
                .copied()
                .filter(|&id| st.jobs.get(id).is_some_and(|entry| entry.lane == lane))
                .collect();
            for id in failed {
                st.mark_cancelled(id, &self.inner.writer);
            }
            for (&unit, active) in &st.lanes[lane].units {
                interrupt(&mut st.jobs, unit, active, Stop::Cancel);
            }
            held
        };
        self.inner.poke();
        held
    }

    /// The site is being removed: cancels everything of it (as [`Self::cancel_site`]), and marks
    /// every other unfinished journal record of it Cancelled, those a crash left. The multipart
    /// uploads among those are given up through `provider` when there is one (ruling Q49).
    ///
    /// The jobs the engine held are left to its own cancel, which writes each record once and
    /// gives up each upload once (review N2). The others are written through the journal writer
    /// too, in chunks of one transaction each, so this returns at once however many there are
    /// (one disk sync per record took about 9 s for 13,793), and closing waits for them
    /// ([`Self::pending_writes`]), as for the aborts ([`Self::pending_aborts`]). Reads the
    /// journal: call it off the UI thread.
    pub fn forget_site(&self, site: &SiteId, provider: Option<Arc<dyn StorageProvider>>) {
        let held = self.cancel_site_held(site);
        let records = match self.inner.journal.unfinished() {
            Ok(records) => records,
            Err(_) => {
                self.inner.writer.count_error();
                self.inner.bump();
                return;
            }
        };
        {
            let st = self.inner.lock();
            for record in records.iter().filter(|record| record.site == *site) {
                let Some(id) = job_id(record.id) else {
                    continue;
                };
                if held.contains(&id) || st.holds(id) {
                    continue;
                }
                if let Some(provider) = &provider {
                    self.inner
                        .aborts
                        .give_up(provider, &record.job, record.multipart.as_ref());
                }
                self.inner.writer.set_state(id, JobState::Cancelled, None);
            }
        }
        self.inner.bump();
    }

    /// What the drawer shows now. Costs the same however many jobs there are: the lists are
    /// windows of at most [`WINDOW`] jobs and the totals are kept as jobs change.
    pub fn snapshot(&self) -> EngineSnapshot {
        let now = Instant::now();
        let mut snapshot = self.inner.lock().snapshot(now);
        snapshot.journal_errors = self.inner.writer.errors();
        snapshot
    }

    /// Ticks on every change. The UI throttles how often it reads a snapshot.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// Something is queued (paused ones too), running, not yet written to the journal, a
    /// multipart upload is still being given up, or an upload waits for its check.
    pub fn is_busy(&self) -> bool {
        self.has_work()
            || self.pending_writes() > 0
            || self.pending_aborts() > 0
            || self.pending_checks() > 0
    }

    /// Uploads sent again after a request for them may have been cut off, counted done and
    /// waiting to be looked for on their site (ruling Q55). They are not transfers: the quit
    /// alert doesn't ask about them and closing doesn't wait for them. Their records stay
    /// unfinished until they are checked, so after a quit the next start sends and checks them
    /// again.
    pub fn pending_checks(&self) -> usize {
        self.inner.lock().checks.len()
    }

    /// Transfers are queued (paused ones too) or running: what the quit alert asks about. The
    /// journal writes and aborts still on their way are not transfers (review C1); closing waits
    /// for them, with [`Self::pending_writes`] and [`Self::pending_aborts`].
    pub fn has_work(&self) -> bool {
        self.inner.lock().has_work()
    }

    /// Journal updates queued and not yet written.
    pub fn pending_writes(&self) -> usize {
        self.inner.writer.pending()
    }

    /// Multipart uploads still being given up (ruling Q49).
    pub fn pending_aborts(&self) -> usize {
        self.inner.aborts.running()
    }

    /// How many uploads and downloads the site may have in flight now, after any slow-down.
    /// `None` for a site the engine hasn't seen.
    pub fn site_transfer_limit(&self, site: &SiteId) -> Option<usize> {
        let st = self.inner.lock();
        let lane = *st.lane_of.get(site)?;
        Some(st.lanes[lane].transfers.limiter.limit())
    }

    /// Where the local file work of units dispatched from now on goes: the disk by default. A
    /// test passes a stand-in, such as one whose disk is full.
    pub fn set_local_files(&self, files: Arc<dyn LocalFiles>) {
        self.inner.lock().files = files;
    }

    /// The jobs a crash (or a quit) left unfinished in the journal, Queued or Failed, that this
    /// engine doesn't hold: what [`resume_from_journal`](Self::resume_from_journal) would offer
    /// again. 0 when the journal can't be read (counted as a journal error).
    pub fn unfinished_count(&self) -> usize {
        self.unfinished().total
    }

    /// As [`Self::unfinished_count`], with how many of them are deletes: the resume banner names
    /// them, since Resume carries them out (review C1).
    pub fn unfinished(&self) -> Unfinished {
        let records = match self.inner.journal.unfinished() {
            Ok(records) => records,
            Err(_) => {
                self.inner.writer.count_error();
                return Unfinished::default();
            }
        };
        let st = self.inner.lock();
        let mut left = Unfinished::default();
        for record in &records {
            if job_id(record.id).is_some_and(|id| !st.holds(id)) {
                left.total += 1;
                if matches!(record.job, Job::Delete { .. }) {
                    left.deletes += 1;
                }
            }
        }
        left
    }

    /// Re-queues every unfinished journal record whose site `lookup` still knows, at the back
    /// of its site's queue, and returns how many. Failed records go back to the queue too: a
    /// resume is a retry. A delete marked as possibly sent treats `NotFound` as done; an upload
    /// marked so is checked on its site once sent again (ruling Q55).
    ///
    /// The records of a site that no longer exists are marked Cancelled, so they aren't
    /// offered again; records this engine already holds (or is about to check) are left alone.
    /// Downloads resume unverified (their listed checksums aren't journalled).
    pub fn resume_from_journal(&self, lookup: SiteLookup<'_>) -> usize {
        self.resume_from_journal_except(lookup, &HashSet::new())
    }

    /// As [`Self::resume_from_journal`], leaving the records of the sites in `keep` as they are:
    /// neither resumed nor cancelled, so they are offered again (review M14: a site whose
    /// password can't be read now doesn't hold the other sites back).
    pub fn resume_from_journal_except(
        &self,
        lookup: SiteLookup<'_>,
        keep: &HashSet<SiteId>,
    ) -> usize {
        let records = match self.inner.journal.unfinished() {
            Ok(records) => records,
            Err(_) => {
                self.inner.writer.count_error();
                return 0;
            }
        };
        let records: Vec<JobRecord> = records
            .into_iter()
            .filter(|record| !keep.contains(&record.site))
            .collect();
        // Each site is looked up once, before the lock is taken: `lookup` is the app's code.
        let mut sites: HashMap<SiteId, Option<SiteInfo>> = HashMap::new();
        for record in &records {
            if !sites.contains_key(&record.site) {
                let found = lookup(&record.site);
                sites.insert(record.site.clone(), found);
            }
        }
        let mut resumed = 0;
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let writer = &self.inner.writer;
            let mut lanes: HashMap<SiteId, usize> = HashMap::new();
            let mut batches: HashMap<String, u64> = HashMap::new();
            for record in records {
                let Some(id) = job_id(record.id) else {
                    continue;
                };
                if st.holds(id) {
                    continue;
                }
                let Some(Some((name, host, provider))) = sites.get(&record.site) else {
                    writer.set_state(id, JobState::Cancelled, None);
                    continue;
                };
                if !st.has_work() {
                    st.start_run(Instant::now());
                }
                let lane = match lanes.get(&record.site) {
                    Some(&lane) => lane,
                    None => {
                        let lane = st.lane_for(
                            record.site.clone(),
                            name.clone(),
                            host.clone(),
                            provider.clone(),
                        );
                        lanes.insert(record.site.clone(), lane);
                        lane
                    }
                };
                st.run.site.clone_from(name);
                st.run.host.clone_from(host);
                let seq = match batches.get(&record.batch) {
                    Some(&seq) => seq,
                    None => {
                        let seq = st.next_batch;
                        st.next_batch += 1;
                        batches.insert(record.batch.clone(), seq);
                        seq
                    }
                };
                let job = record.job;
                let size = size_of(&job);
                let info = st.batches.entry(seq).or_insert_with(|| BatchInfo {
                    site: name.clone(),
                    open: 0,
                    failed: 0,
                    done_files: 0,
                    done_bytes: 0,
                    kinds: Kinds::default(),
                });
                info.open += 1;
                info.kinds.note(job.kind());
                st.run.kinds.note(job.kind());
                st.run.files_total += 1;
                st.run.bytes_total += size;
                if record.state == JobState::Failed {
                    writer.set_state(id, JobState::Queued, None);
                }
                let sent = possibly_sent(&job, record.multipart.as_ref());
                let multipart = upload_progress(&job, record.multipart);
                let mut entry = Entry::new(job, lane, seq, size);
                entry.sent = sent;
                entry.multipart = multipart;
                st.lanes[lane].queue.push_back(id);
                st.jobs.insert(id, entry);
                resumed += 1;
            }
        }
        self.inner.poke();
        resumed
    }

    /// Drops the unfinished jobs a crash left in the journal, when the user chooses not to
    /// resume them. The jobs this engine holds stay journalled. The multipart uploads among the
    /// dropped ones are given up on their sites, through the clients `lookup` gives (ruling
    /// Q49); a site it doesn't know keeps them.
    ///
    /// While the engine holds unfinished jobs, the others are marked Cancelled through the
    /// journal writer, in chunks (review N2), which closing waits for; otherwise every
    /// unfinished record goes in one statement.
    pub fn discard_journal(&self, lookup: SiteLookup<'_>) {
        let holds_unfinished = {
            let st = self.inner.lock();
            st.has_work() || !st.failed.is_empty() || !st.checks.is_empty()
        };
        let journal = &self.inner.journal;
        let records = match journal.unfinished() {
            Ok(records) => records,
            Err(_) => {
                self.inner.writer.count_error();
                self.inner.bump();
                return;
            }
        };
        let others: Vec<JobRecord> = {
            let st = self.inner.lock();
            records
                .into_iter()
                .filter(|record| job_id(record.id).is_none_or(|id| !st.holds(id)))
                .collect()
        };
        let mut sites: HashMap<SiteId, Option<Arc<dyn StorageProvider>>> = HashMap::new();
        for record in &others {
            let uploading = upload_progress(&record.job, record.multipart.clone()).is_some();
            if !uploading {
                continue;
            }
            let provider = sites
                .entry(record.site.clone())
                .or_insert_with(|| lookup(&record.site).map(|(_, _, provider)| provider));
            if let Some(provider) = provider {
                self.inner
                    .aborts
                    .give_up(provider, &record.job, record.multipart.as_ref());
            }
        }
        if holds_unfinished {
            let st = self.inner.lock();
            for id in others.iter().filter_map(|record| job_id(record.id)) {
                if !st.holds(id) {
                    self.inner.writer.set_state(id, JobState::Cancelled, None);
                }
            }
        } else if journal.discard_unfinished().is_err() {
            self.inner.writer.count_error();
        }
        self.inner.bump();
    }

    /// Whether `site` has transfers queued (paused ones too) or running.
    pub fn site_has_work(&self, site: &SiteId) -> bool {
        let st = self.inner.lock();
        st.lane_of
            .get(site)
            .is_some_and(|&lane| st.lanes[lane].has_work())
    }

    /// A mark for the journal updates queued so far, for [`Self::journal_reached`].
    pub fn journal_mark(&self) -> u64 {
        self.inner.writer.queued()
    }

    /// Whether every journal update queued before `mark` was taken has been written, or has
    /// failed (counted in the snapshot's `journal_errors`): the journal then shows the changes
    /// made until then (review N2: what the resume banner counts after a site is removed).
    pub fn journal_reached(&self, mark: u64) -> bool {
        self.inner.writer.dealt_with() >= mark
    }

    /// Submits `batch` as [`Self::submit`] does, and keeps its per-job outcomes for
    /// [`Self::wait_batch`], which must be called once for it. A batch without jobs is never
    /// waited for.
    pub fn submit_tracked(&self, batch: Batch) -> Result<String, JournalError> {
        self.submit_batch(batch, true)
    }

    /// Resolves when every job of the batch is done, failed or cancelled AND
    /// `pending_checks_for(site) == 0`; a check that sends a file again reopens the batch; a
    /// stopped engine reports the rest as cancelled. A batch id the engine doesn't track (one
    /// without jobs, or taken already) gives an empty outcome at once.
    pub async fn wait_batch(&self, batch_id: &str) -> BatchOutcome {
        let mut changes = self.changes();
        loop {
            {
                let mut st = self.inner.lock();
                let Some((&seq, tracked)) = st
                    .tracked
                    .iter()
                    .find(|(_, tracked)| tracked.id == batch_id)
                else {
                    return BatchOutcome::default();
                };
                let lane = tracked.lane;
                let settled =
                    tracked.closed() && !st.checks.values().any(|check| check.lane == lane);
                if (settled || st.shut_down)
                    && let Some(tracked) = st.tracked.remove(&seq)
                {
                    return tracked.outcome();
                }
            }
            if changes.changed().await.is_err() {
                let mut st = self.inner.lock();
                let seq = st
                    .tracked
                    .iter()
                    .find(|(_, tracked)| tracked.id == batch_id)
                    .map(|(&seq, _)| seq);
                return seq
                    .and_then(|seq| st.tracked.remove(&seq))
                    .map(Tracked::outcome)
                    .unwrap_or_default();
            }
        }
    }

    /// The late checks (Q55) still waiting for `site`.
    pub fn pending_checks_for(&self, site: &SiteId) -> usize {
        let st = self.inner.lock();
        st.lane_of.get(site).map_or(0, |&lane| {
            st.checks
                .values()
                .filter(|check| check.lane == lane)
                .count()
        })
    }
}

/// The record id as a job id: ids start at 1.
fn job_id(record: i64) -> Option<JobId> {
    u64::try_from(record).ok().filter(|&id| id > 0).map(JobId)
}

fn record_id(id: JobId) -> i64 {
    i64::try_from(id.0).unwrap_or(i64::MAX)
}

fn size_of(job: &Job) -> u64 {
    match job {
        Job::Upload { size, .. } | Job::Download { size, .. } => *size,
        Job::Delete { .. } | Job::CreateFolder { .. } => 0,
    }
}

/// Starts whatever may start, whenever woken.
async fn dispatch_loop(inner: Arc<Inner>) {
    loop {
        inner.wake.notified().await;
        let units = {
            let mut st = inner.lock();
            if st.shut_down {
                return;
            }
            st.dispatch(&inner.writer)
        };
        if units.is_empty() {
            continue;
        }
        for spec in units {
            crate::task::spawn_on(&inner.rt, run_unit(inner.clone(), spec));
        }
        inner.bump();
    }
}

/// Checks the uploads waiting for it (ruling Q55), for as long as the engine runs: each folder
/// due is listed once, under the retry policy, through its site's current client. Sleeps until
/// the next check is due, or until one is added; ends when the engine stops, leaving what it
/// had not checked unfinished in the journal.
async fn check_loop(inner: Arc<Inner>) {
    loop {
        let due = {
            let st = inner.lock();
            if st.shut_down {
                return;
            }
            st.due_checks(Instant::now())
        };
        match due {
            Due::Nothing => inner.check_wake.notified().await,
            Due::At(at) => {
                tokio::select! {
                    () = tokio::time::sleep_until(at) => {}
                    () = inner.check_wake.notified() => {}
                }
            }
            Due::Now(folders) => {
                for folder in folders {
                    let listed =
                        with_retry(&folder.policy, |_| folder.provider.list(&folder.dir)).await;
                    inner.checked(folder, listed);
                }
            }
        }
    }
}

/// Asks every job of an active unit to stop for `reason`, and wakes the unit.
fn interrupt(jobs: &mut Slab, unit: UnitId, active: &ActiveUnit, reason: Stop) {
    for &id in &active.jobs {
        if let Some(entry) = jobs.get_mut(id)
            && entry.status == Status::Active(unit)
        {
            entry.stop = entry.stop.max(Some(reason));
        }
    }
    active.stop.notify_one();
}

impl Entry {
    fn new(job: Job, lane: usize, batch: u64, size: u64) -> Self {
        Self {
            job,
            lane,
            batch,
            status: Status::Queued,
            paused: false,
            size,
            done_bytes: 0,
            error: None,
            stop: None,
            sent: false,
            recheck: false,
            checks: 0,
            checksum: None,
            strikes: 0,
            struck: None,
            multipart: None,
        }
    }

    /// The job's own network error took its lane offline, or met it offline, in the lane's
    /// offline cycle `cycle`: a strike, counted once per cycle (ruling Q50). True once that
    /// makes [`MAX_STRIKES`]: the job then fails rather than loop.
    fn strike(&mut self, cycle: u64) -> bool {
        if self.struck != Some(cycle) {
            self.struck = Some(cycle);
            self.strikes = self.strikes.saturating_add(1);
        }
        self.strikes >= MAX_STRIKES
    }

    fn view(&self, id: JobId) -> JobView {
        JobView {
            id,
            kind: self.job.kind(),
            path: crate::lane::remote_of(&self.job).as_str().to_owned(),
            size: self.size,
            done_bytes: self.done_bytes,
            error: self.error.clone(),
            paused: self.paused,
        }
    }
}

impl State {
    fn new(settings: TransferSettings, aborts: Aborts) -> Self {
        Self {
            settings,
            paused: false,
            shut_down: false,
            jobs: Slab::default(),
            lanes: Vec::new(),
            lane_of: HashMap::new(),
            batches: HashMap::new(),
            next_batch: 0,
            next_unit: 0,
            run: Run::default(),
            failed: BTreeSet::new(),
            done: VecDeque::new(),
            last_batch: None,
            rates: RateWindow::default(),
            disk_full: None,
            files: Arc::new(DiskFiles),
            aborts,
            checks: HashMap::new(),
            run_seq: 0,
            rechecked: Rechecked::default(),
            tracked: HashMap::new(),
            events: broadcast::channel(EVENTS).0,
            cache_rules: HashMap::new(),
        }
    }

    /// The site whose lane is `lane`.
    fn site_of(&self, lane: usize) -> Option<&SiteId> {
        self.lane_of
            .iter()
            .find(|&(_, &index)| index == lane)
            .map(|(site, _)| site)
    }

    /// Tells the receivers of [`Engine::events`] that `job` on `lane` ended.
    fn emit(&self, lane: usize, job: &Job, bytes: u64, outcome: JobEventOutcome) {
        if self.events.receiver_count() == 0 {
            return;
        }
        let Some(site) = self.site_of(lane).cloned() else {
            return;
        };
        let _ = self.events.send(JobEvent {
            site,
            site_name: self.lanes[lane].name.clone(),
            kind: job.kind(),
            path: crate::lane::remote_of(job).as_str().to_owned(),
            bytes,
            outcome,
        });
    }

    /// Notes how job `id` of batch `batch` ended (`None`: it is open again), when the batch is
    /// tracked.
    fn track(&mut self, batch: u64, id: JobId, end: Option<End>) {
        if let Some(job) = self
            .tracked
            .get_mut(&batch)
            .and_then(|tracked| tracked.jobs.get_mut(&id))
        {
            job.end = end;
        }
    }

    fn has_work(&self) -> bool {
        self.lanes.iter().any(Lane::has_work)
    }

    /// Whether job `id` is the engine's: in memory, or waiting for its check (ruling Q55). Its
    /// journal record is then the engine's to write, never one to offer, resume or discard.
    fn holds(&self, id: JobId) -> bool {
        self.jobs.get(id).is_some() || self.checks.contains_key(&id)
    }

    /// A new run: the done jobs are let go and the totals start again from the failed jobs,
    /// which carry over.
    fn start_run(&mut self, now: Instant) {
        self.run_seq += 1;
        for id in self.done.drain(..) {
            self.jobs.remove(id);
        }
        let mut run = Run::default();
        for &id in &self.failed {
            if let Some(entry) = self.jobs.get(id) {
                run.files_total += 1;
                run.bytes_total += entry.size;
                run.bytes_failed += entry.size;
                run.kinds.note(entry.job.kind());
            }
        }
        self.run = run;
        self.rates.restart(now);
        // Nothing waits for the full disk any more.
        self.disk_full = None;
    }

    /// The lane for `site`, made on first use; its name, host and client follow the newest
    /// batch.
    fn lane_for(
        &mut self,
        site: SiteId,
        name: String,
        host: String,
        provider: Arc<dyn StorageProvider>,
    ) -> usize {
        if let Some(&index) = self.lane_of.get(&site) {
            let lane = &mut self.lanes[index];
            lane.name = name;
            lane.host = host;
            lane.provider = provider;
            lane.configure(&self.settings);
            return index;
        }
        let index = self.lanes.len();
        self.lanes
            .push(Lane::new(name, host, provider, &self.settings));
        self.lane_of.insert(site, index);
        index
    }

    /// Takes every unit that may start now off the lanes' queues. A delete about to be sent
    /// for the first time is marked possibly sent, in memory and in the journal; so is an upload,
    /// on a service that can delete a cut-off upload late, and one sent with the mark already
    /// set is to be checked once answered (ruling Q55).
    fn dispatch(&mut self, writer: &JournalWriter) -> Vec<UnitSpec> {
        let mut units = Vec::new();
        if self.paused || self.shut_down || self.disk_full.is_some() {
            return units;
        }
        let policy = self.settings.retry_policy();
        let verify = self.settings.verify_checksums;
        for lane in 0..self.lanes.len() {
            let late = self.lanes[lane]
                .provider
                .capabilities()
                .cut_uploads_delete_late;
            let rules = self
                .site_of(lane)
                .and_then(|site| self.cache_rules.get(site))
                .cloned()
                .unwrap_or_default();
            while let Some((kind, ids)) = self.lanes[lane].next_unit(&self.jobs) {
                let unit = UnitId(self.next_unit);
                self.next_unit += 1;
                let mut items = Vec::with_capacity(ids.len());
                for &id in &ids {
                    if let Some(entry) = self.jobs.get_mut(id) {
                        entry.status = Status::Active(unit);
                        entry.stop = None;
                        let sent = entry.sent;
                        // The writer's count once the mark is queued: the unit sends nothing
                        // until the journal has it.
                        let mut journalled = None;
                        match entry.job.kind() {
                            JobKind::Delete if !sent => {
                                entry.sent = true;
                                writer.set_multipart(id, Some(delete_sent()));
                            }
                            JobKind::Upload if late => {
                                entry.recheck = sent;
                                if !sent {
                                    entry.sent = true;
                                    // An upload's own multipart progress keeps the column
                                    // (ruling Q49): no service that sends in parts deletes late.
                                    if entry.multipart.is_none() {
                                        writer.set_multipart(id, Some(upload_sent()));
                                        journalled = Some(writer.queued());
                                    }
                                }
                            }
                            _ => {}
                        }
                        let cache_control = match &entry.job {
                            Job::Upload { remote, .. } => cache_control_for(&rules, remote),
                            _ => None,
                        };
                        items.push((
                            id,
                            entry.job.clone(),
                            sent,
                            entry.checksum.clone(),
                            journalled,
                            cache_control,
                        ));
                    }
                }
                let work = match items.len() {
                    1 => {
                        let (id, job, sent, checksum, journalled, cache_control) = items.remove(0);
                        work_for(id, job, sent, checksum, journalled, cache_control)
                    }
                    _ => Work::DeleteFiles {
                        items: items
                            .into_iter()
                            .map(|(id, job, sent, _, _, _)| {
                                (id, crate::lane::remote_of(&job).clone(), sent)
                            })
                            .collect(),
                    },
                };
                let stop = Arc::new(Notify::new());
                let lane_ref = &mut self.lanes[lane];
                lane_ref.units.insert(
                    unit,
                    ActiveUnit {
                        jobs: ids,
                        kind,
                        stop: stop.clone(),
                    },
                );
                units.push(UnitSpec {
                    unit,
                    lane,
                    kind,
                    provider: lane_ref.provider.clone(),
                    policy: policy.clone(),
                    verify,
                    work,
                    stop,
                    files: self.files.clone(),
                });
            }
        }
        units
    }

    /// A download found `drive` full: every lane stops (the work in flight goes back to the
    /// queue) until the user resumes, and the snapshot says how much the downloads still
    /// waiting need on that drive.
    fn hold_disk_full(&mut self, drive: String) {
        let queued = self
            .lanes
            .iter()
            .flat_map(|lane| lane.queue.iter())
            .filter_map(|&id| self.jobs.get(id))
            .filter(|entry| entry.status == Status::Queued);
        // A unit's list keeps the jobs it already gave back, so only those still active count.
        let active = self
            .lanes
            .iter()
            .flat_map(|lane| lane.units.values())
            .flat_map(|unit| unit.jobs.iter())
            .filter_map(|&id| self.jobs.get(id))
            .filter(|entry| matches!(entry.status, Status::Active(_)));
        let needed = queued
            .chain(active)
            .filter(|entry| match &entry.job {
                Job::Download { local, .. } => drive_of(local) == drive,
                _ => false,
            })
            .map(|entry| entry.size)
            .sum();
        self.disk_full = Some((drive, needed));
        self.interrupt_all(Stop::Requeue);
    }

    fn interrupt_all(&mut self, reason: Stop) {
        for lane in &self.lanes {
            for (&unit, active) in &lane.units {
                interrupt(&mut self.jobs, unit, active, reason);
            }
        }
    }

    /// Takes a queued job out of its lane's queue. False when it wasn't there. The lane's
    /// count of paused jobs is the caller's to keep.
    fn unqueue(&mut self, lane: usize, id: JobId) -> bool {
        let lane = &mut self.lanes[lane];
        let Some(pos) = lane.queue.iter().position(|&queued| queued == id) else {
            return false;
        };
        lane.queue.remove(pos);
        true
    }

    /// A job leaves the work in flight: its paths are free and its partial bytes uncounted.
    fn leave_active(&mut self, id: JobId) {
        let Some(entry) = self.jobs.get_mut(id) else {
            return;
        };
        if !matches!(entry.status, Status::Active(_)) {
            return;
        }
        self.run.bytes_in_flight -= entry.done_bytes;
        let lane = entry.lane;
        self.lanes[lane].paths.remove(&entry.job);
    }

    /// A job is done. `own_request` counts it as a success for the lane's limiter; the jobs of
    /// a `delete_batch` request count once for the whole request instead (ruling Q37).
    ///
    /// An upload sent again after a request for it may have been cut off is done in the drawer
    /// but not yet in the journal: it waits for its check (ruling Q55).
    fn mark_done(&mut self, id: JobId, writer: &JournalWriter, now: Instant, own_request: bool) {
        self.leave_active(id);
        let Some(entry) = self.jobs.get_mut(id) else {
            return;
        };
        let unmetered = entry.size.saturating_sub(entry.done_bytes);
        entry.status = Status::Done;
        entry.done_bytes = entry.size;
        entry.error = None;
        entry.stop = None;
        let (size, batch, lane, slot) = (
            entry.size,
            entry.batch,
            entry.lane,
            SlotKind::of(&entry.job),
        );
        let check = std::mem::take(&mut entry.recheck).then(|| Check {
            job: entry.job.clone(),
            lane,
            batch,
            size,
            due: now + CHECK_AFTER,
            round: entry.checks.saturating_add(1),
            run: self.run_seq,
        });
        // Told done now, or once its check found it (ruling Q55).
        let told = (check.is_none() && self.events.receiver_count() > 0).then(|| entry.job.clone());
        match check {
            Some(check) => {
                self.checks.insert(id, check);
            }
            None => writer.set_state(id, JobState::Done, None),
        }
        if let Some(job) = told {
            self.emit(lane, &job, size, JobEventOutcome::Done);
        }
        self.run.files_done += 1;
        self.run.bytes_finished += size;
        self.rates.record(now, 1, unmetered);
        if own_request {
            self.lanes[lane].slots_mut(slot).limiter.on_success();
        }
        if let Some(info) = self.batches.get_mut(&batch) {
            info.open -= 1;
            info.done_files += 1;
            info.done_bytes += size;
        }
        self.track(batch, id, Some(End::Done { bytes: size }));
        self.batch_changed(batch, true);
        self.done.push_back(id);
        if self.done.len() > WINDOW
            && let Some(old) = self.done.pop_front()
        {
            self.jobs.remove(old);
        }
    }

    fn mark_failed(&mut self, id: JobId, reason: String, writer: &JournalWriter) {
        self.leave_active(id);
        let Some(entry) = self.jobs.get_mut(id) else {
            return;
        };
        writer.set_state(id, JobState::Failed, Some(&reason));
        entry.status = Status::Failed;
        entry.done_bytes = 0;
        entry.error = Some(reason.clone());
        entry.stop = None;
        let (size, batch, lane) = (entry.size, entry.batch, entry.lane);
        let told = (self.events.receiver_count() > 0).then(|| entry.job.clone());
        if let Some(job) = told {
            self.emit(lane, &job, 0, JobEventOutcome::Failed(reason.clone()));
        }
        self.run.bytes_failed += size;
        self.failed.insert(id);
        if let Some(info) = self.batches.get_mut(&batch) {
            info.open -= 1;
            info.failed += 1;
        }
        self.track(batch, id, Some(End::Failed(reason)));
        self.batch_changed(batch, true);
    }

    /// Cancels a job that is queued (already out of its queue), active or failed, and lets it
    /// go. An upload's multipart upload is given up, so the service frees its parts (ruling
    /// Q49): one queued after a resume, one paused part way, or one cut short in flight.
    fn mark_cancelled(&mut self, id: JobId, writer: &JournalWriter) {
        self.leave_active(id);
        let Some(entry) = self.jobs.remove(id) else {
            return;
        };
        writer.set_state(id, JobState::Cancelled, None);
        let provider = &self.lanes[entry.lane].provider;
        self.aborts
            .give_up(provider, &entry.job, entry.multipart.as_ref());
        let was_failed = entry.status == Status::Failed;
        // A failed job was told already; letting it go is no new end.
        if !was_failed {
            self.emit(entry.lane, &entry.job, 0, JobEventOutcome::Cancelled);
        }
        if was_failed {
            self.failed.remove(&id);
            self.run.bytes_failed -= entry.size;
        }
        self.run.files_total -= 1;
        self.run.bytes_total -= entry.size;
        if let Some(info) = self.batches.get_mut(&entry.batch) {
            if was_failed {
                info.failed -= 1;
            } else {
                info.open -= 1;
            }
        }
        self.track(entry.batch, id, Some(End::Cancelled));
        // A failed job's batch had already finished: dropping the job is no news.
        self.batch_changed(entry.batch, !was_failed);
    }

    /// Active jobs back to the front of their lane's queue, in the order given.
    fn requeue_front(&mut self, lane: usize, ids: Vec<JobId>) {
        for &id in ids.iter().rev() {
            self.leave_active(id);
            let Some(entry) = self.jobs.get_mut(id) else {
                continue;
            };
            entry.status = Status::Queued;
            entry.done_bytes = 0;
            entry.stop = None;
            self.lanes[lane].queue.push_front(id);
        }
    }

    fn requeue_failed(&mut self, id: JobId, writer: &JournalWriter) {
        let Some(entry) = self.jobs.get_mut(id) else {
            return;
        };
        if entry.status != Status::Failed {
            return;
        }
        writer.set_state(id, JobState::Queued, None);
        entry.status = Status::Queued;
        entry.error = None;
        entry.done_bytes = 0;
        // The user's Retry starts it afresh (rulings Q50, Q55).
        entry.strikes = 0;
        entry.struck = None;
        entry.checks = 0;
        let (lane, batch, size) = (entry.lane, entry.batch, entry.size);
        self.failed.remove(&id);
        self.run.bytes_failed -= size;
        if let Some(info) = self.batches.get_mut(&batch) {
            info.failed -= 1;
            info.open += 1;
        }
        self.track(batch, id, None);
        self.lanes[lane].queue.push_back(id);
    }

    /// An upload counted done whose check found it gone (ruling Q55) is queued again in memory:
    /// taken off the run's done count (or, if a new run has started since, added to its totals),
    /// its batch opened again if it is still kept (a batch that has closed for good stays closed,
    /// so the drawer's "last transfer" keeps its whole count), and its entry back as Queued (made
    /// again if the done jobs let it go). The caller puts it in its lane's queue, or fails it.
    fn reopen(&mut self, id: JobId, check: &Check) {
        if check.run == self.run_seq {
            self.run.files_done = self.run.files_done.saturating_sub(1);
            self.run.bytes_finished = self.run.bytes_finished.saturating_sub(check.size);
        } else {
            self.run.files_total += 1;
            self.run.bytes_total += check.size;
            self.run.kinds.note(JobKind::Upload);
        }
        match self.jobs.get_mut(id) {
            Some(entry) => {
                entry.status = Status::Queued;
                entry.done_bytes = 0;
                entry.error = None;
                entry.checks = check.round;
                if let Some(pos) = self.done.iter().position(|&done| done == id) {
                    self.done.remove(pos);
                }
            }
            None => {
                let mut entry = Entry::new(check.job.clone(), check.lane, check.batch, check.size);
                entry.sent = true;
                entry.checks = check.round;
                self.jobs.insert(id, entry);
            }
        }
        if let Some(info) = self.batches.get_mut(&check.batch) {
            info.open += 1;
            info.done_files = info.done_files.saturating_sub(1);
            info.done_bytes = info.done_bytes.saturating_sub(check.size);
        }
        self.track(check.batch, id, None);
    }

    /// The checks that may be made now (ruling Q55), grouped by folder: those past their settle
    /// delay whose site's lane is running with nothing queued or in flight (the batch has
    /// finished). Otherwise when to look again.
    fn due_checks(&self, now: Instant) -> Due {
        let mut folders: HashMap<(usize, RemotePath), Vec<JobId>> = HashMap::new();
        let mut next: Option<Instant> = None;
        for (&id, check) in &self.checks {
            let lane = &self.lanes[check.lane];
            let idle = lane.state == LaneState::Running && !lane.has_work();
            if check.due <= now && idle {
                let remote = crate::lane::remote_of(&check.job);
                let dir = remote.parent().unwrap_or_else(RemotePath::root);
                folders.entry((check.lane, dir)).or_default().push(id);
            } else {
                let at = if check.due <= now {
                    now + CHECK_POLL
                } else {
                    check.due
                };
                next = Some(next.map_or(at, |next| next.min(at)));
            }
        }
        if folders.is_empty() {
            return next.map_or(Due::Nothing, Due::At);
        }
        let policy = self.settings.retry_policy();
        let mut due: Vec<CheckFolder> = folders
            .into_iter()
            .map(|((lane, dir), mut ids)| {
                ids.sort_unstable();
                CheckFolder {
                    dir,
                    ids,
                    provider: self.lanes[lane].provider.clone(),
                    policy: policy.clone(),
                }
            })
            .collect();
        due.sort_by(|a, b| a.ids.cmp(&b.ids));
        Due::Now(due)
    }

    /// A batch whose last open job just ended (`closed`) is summarised as the last batch;
    /// once nothing of it can come back (no failed job left to retry) it is forgotten.
    fn batch_changed(&mut self, batch: u64, closed: bool) {
        let Some(info) = self.batches.get(&batch) else {
            return;
        };
        if info.open > 0 {
            return;
        }
        if closed && (info.done_files > 0 || info.failed > 0) {
            self.last_batch = Some(BatchSummary {
                finished_at: Utc::now(),
                files: info.done_files,
                bytes: info.done_bytes,
                direction: info.kinds.direction(),
                site: info.site.clone(),
            });
        }
        if info.failed == 0 {
            self.batches.remove(&batch);
        }
    }

    /// The site refused its credentials (`AuthStopped`) or the PC's clock (`ClockSkew`): its
    /// lane stops and everything in flight on it goes back to the queue.
    fn stop_lane(&mut self, lane: usize, why: LaneState) {
        self.lanes[lane].state = why;
        let lane = &self.lanes[lane];
        for (&unit, active) in &lane.units {
            interrupt(&mut self.jobs, unit, active, Stop::Requeue);
        }
    }

    fn engine_state(&self) -> EngineState {
        if let Some((drive, needed)) = &self.disk_full
            && self.has_work()
        {
            return EngineState::DiskFull {
                drive: drive.clone(),
                needed: *needed,
            };
        }
        if let Some(lane) = self
            .lanes
            .iter()
            .find(|lane| lane.state == LaneState::AuthStopped && lane.has_work())
        {
            return EngineState::AuthStopped {
                site: lane.name.clone(),
            };
        }
        if let Some(lane) = self
            .lanes
            .iter()
            .find(|lane| lane.state == LaneState::ClockSkew && lane.has_work())
        {
            return EngineState::ClockSkew {
                site: lane.name.clone(),
            };
        }
        // The user's pause outranks an offline site (review M13): nothing would be sent anyway,
        // and the drawer then offers Resume.
        if self.paused && self.has_work() {
            return EngineState::Paused;
        }
        if let Some(lane) = self
            .lanes
            .iter()
            .find(|lane| lane.state == LaneState::Offline && lane.has_work())
        {
            return EngineState::Offline {
                site: lane.name.clone(),
            };
        }
        if self.lanes.iter().any(Lane::is_moving) {
            EngineState::Running
        } else {
            EngineState::Idle
        }
    }

    fn snapshot(&self, now: Instant) -> EngineSnapshot {
        let state = self.engine_state();
        let run = &self.run;
        let files_failed = self.failed.len() as u64;
        let bytes_done = run.bytes_finished + run.bytes_in_flight;
        let (files_per_s, bytes_per_s) = self.rates.per_second(now);
        let eta = if state == EngineState::Running {
            eta(
                run.files_total
                    .saturating_sub(run.files_done + files_failed),
                run.bytes_total
                    .saturating_sub(bytes_done + run.bytes_failed),
                files_per_s,
                bytes_per_s,
            )
        } else {
            None
        };
        let (site, host) = match self.lanes.iter().find(|lane| lane.has_work()) {
            Some(lane) => (lane.name.clone(), lane.host.clone()),
            None => (run.site.clone(), run.host.clone()),
        };
        let view = |id: JobId| self.jobs.get(id).map(|entry| entry.view(id));

        let mut active: Vec<JobId> = self
            .lanes
            .iter()
            .flat_map(|lane| lane.units.values())
            .flat_map(|unit| unit.jobs.iter().copied())
            .filter(|&id| {
                self.jobs
                    .get(id)
                    .is_some_and(|entry| matches!(entry.status, Status::Active(_)))
            })
            .collect();
        active.sort_unstable();
        let active_count = active.len() as u64;
        let queued: Vec<JobView> = self
            .lanes
            .iter()
            .flat_map(|lane| lane.queue.iter().copied())
            .take(WINDOW)
            .filter_map(view)
            .collect();
        EngineSnapshot {
            state,
            direction: run.kinds.direction(),
            site,
            host,
            files_total: run.files_total,
            files_done: run.files_done,
            files_failed,
            bytes_total: run.bytes_total,
            bytes_done,
            files_per_s,
            bytes_per_s,
            eta,
            active: active.into_iter().take(WINDOW).filter_map(view).collect(),
            active_count,
            queued,
            queued_count: self.lanes.iter().map(|lane| lane.queue.len() as u64).sum(),
            failed: self
                .failed
                .iter()
                .copied()
                .take(WINDOW)
                .filter_map(view)
                .collect(),
            done: self.done.iter().copied().filter_map(view).collect(),
            done_count: run.files_done,
            last_batch: self.last_batch.clone(),
            // Filled in by `Engine::snapshot`, from the journal writer.
            journal_errors: 0,
            rechecked: self.rechecked,
        }
    }
}

/// The journal's mark on a delete that may have been sent (see the module's notes).
fn delete_sent() -> MultipartProgress {
    MultipartProgress {
        upload_id: DELETE_SENT.to_owned(),
        part_size: 0,
        parts: Vec::new(),
        file: None,
    }
}

/// The journal's mark on an upload that may have been sent, on a service that can delete a
/// cut-off upload late (see the module's notes, ruling Q55).
fn upload_sent() -> MultipartProgress {
    MultipartProgress {
        upload_id: UPLOAD_SENT.to_owned(),
        part_size: 0,
        parts: Vec::new(),
        file: None,
    }
}

/// The work a single job does. `journalled`: an upload's "possibly sent" mark is in the journal
/// once the writer has dealt with this many updates.
fn work_for(
    id: JobId,
    job: Job,
    sent: bool,
    checksum: Option<Checksum>,
    journalled: Option<u64>,
    cache_control: Option<String>,
) -> Work {
    match job {
        Job::Upload { local, remote, .. } => Work::Upload {
            id,
            local,
            remote,
            journalled,
            cache_control,
        },
        Job::Download { remote, local, .. } => Work::Download {
            id,
            remote,
            local,
            checksum,
        },
        Job::Delete { remote, folder } => Work::Delete {
            id,
            remote,
            folder,
            sent,
        },
        Job::CreateFolder { remote } => Work::CreateFolder { id, remote },
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Updates under this lock keep the state consistent at every step that can panic
        // (none is expected), so a poisoned lock is still worth using.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tells watchers something changed. Never called with the state locked: a watcher may
    /// hold the channel while it asks for a snapshot.
    fn bump(&self) {
        self.changes.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Something changed that may let more work start.
    fn poke(&self) {
        self.wake.notify_one();
        self.bump();
    }

    fn shut_down(&self) {
        {
            let mut st = self.lock();
            st.shut_down = true;
            st.interrupt_all(Stop::Requeue);
        }
        self.wake.notify_one();
        self.check_wake.notify_one();
    }

    /// Waits until the journal writer has dealt with the first `mark` updates queued (written
    /// them, or counted them as failed): an upload's "possibly sent" mark is then in the journal
    /// before its request goes out (ruling Q55). Ends at once if the engine's watchers are gone.
    pub(crate) async fn journalled(&self, mark: u64) {
        let mut changes = self.changes.subscribe();
        while self.writer.dealt_with() < mark {
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    /// The lane's slow-down generation for `kind`, read as a request starts.
    pub(crate) fn generation(&self, lane: usize, kind: SlotKind) -> u64 {
        self.lock().lanes[lane].slots(kind).generation
    }

    /// A request that started at `seen` was asked to slow down.
    pub(crate) fn slow_down(&self, lane: usize, kind: SlotKind, seen: u64) {
        {
            let mut st = self.lock();
            if !st.settings.adaptive {
                return;
            }
            let slots = st.lanes[lane].slots_mut(kind);
            if slots.generation != seen {
                return;
            }
            slots.limiter.on_slow_down();
            slots.generation += 1;
        }
        self.bump();
    }

    /// `bytes` more of a job in flight went (or arrived).
    pub(crate) fn progress(&self, unit: UnitId, id: JobId, bytes: u64) {
        let now = Instant::now();
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get_mut(id) else {
                return;
            };
            if entry.status != Status::Active(unit) {
                return;
            }
            let counted = bytes.min(entry.size.saturating_sub(entry.done_bytes));
            entry.done_bytes += counted;
            st.run.bytes_in_flight += counted;
            st.rates.record(now, 0, bytes);
        }
        self.bump();
    }

    /// `bytes` of an upload in flight were stored by an earlier attempt (the parts a resumed
    /// multipart upload skips): counted done, but not as sent now, so they make no speed (and
    /// aren't counted at once when the job is done either).
    pub(crate) fn already_stored(&self, unit: UnitId, id: JobId, bytes: u64) {
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get_mut(id) else {
                return;
            };
            if entry.status != Status::Active(unit) {
                return;
            }
            let counted = bytes.min(entry.size.saturating_sub(entry.done_bytes));
            entry.done_bytes += counted;
            st.run.bytes_in_flight += counted;
        }
        self.bump();
    }

    /// A new attempt at a job starts from nothing.
    pub(crate) fn restart_progress(&self, unit: UnitId, id: JobId) {
        let mut guard = self.lock();
        let st = &mut *guard;
        if let Some(entry) = st.jobs.get_mut(id)
            && entry.status == Status::Active(unit)
        {
            st.run.bytes_in_flight -= entry.done_bytes;
            entry.done_bytes = 0;
        }
    }

    /// The file to send turned out to be `size` bytes.
    pub(crate) fn set_size(&self, unit: UnitId, id: JobId, size: u64) {
        let mut guard = self.lock();
        let st = &mut *guard;
        if let Some(entry) = st.jobs.get_mut(id)
            && entry.status == Status::Active(unit)
            && entry.size != size
        {
            st.run.bytes_total = st.run.bytes_total - entry.size + size;
            entry.size = size;
        }
    }

    /// An attempt at this upload was cut off by a network error and a later one in the same
    /// unit succeeded: the server may still end the cut-off request and delete the path, so the
    /// upload is checked as a re-sent one is (ruling Q55, the S5 report's first concern).
    pub(crate) fn recheck_after_cut(&self, unit: UnitId, id: JobId) {
        let mut guard = self.lock();
        let st = &mut *guard;
        if let Some(entry) = st.jobs.get_mut(id)
            && entry.status == Status::Active(unit)
            && entry.sent
            && st.lanes[entry.lane]
                .provider
                .capabilities()
                .cut_uploads_delete_late
        {
            entry.recheck = true;
        }
    }

    /// A job of `unit` ended: done, or failed with `result`'s error. A failure of a job being
    /// cancelled cancels it, whatever the error; otherwise an auth failure stops the job's site
    /// and puts the job back in the queue, an unreachable site puts it back in the queue as it
    /// was, and a full disk puts it back and stops every lane.
    pub(crate) fn finish(&self, unit: UnitId, id: JobId, result: Result<(), Failure>) {
        self.end(unit, id, result, true);
    }

    /// As [`finish`](Self::finish), for one path of a `delete_batch` request: the request's
    /// success is counted once, by [`batch_succeeded`](Self::batch_succeeded).
    pub(crate) fn finish_in_batch(&self, unit: UnitId, id: JobId, result: Result<(), Failure>) {
        self.end(unit, id, result, false);
    }

    /// A `delete_batch` request succeeded: one success for the lane's limiter, however many
    /// paths it carried (ruling Q37).
    pub(crate) fn batch_succeeded(&self, lane: usize, kind: SlotKind) {
        self.lock().lanes[lane].slots_mut(kind).limiter.on_success();
        self.poke();
    }

    fn end(&self, unit: UnitId, id: JobId, result: Result<(), Failure>, own_request: bool) {
        let now = Instant::now();
        let checking = {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get(id) else {
                return;
            };
            if entry.status != Status::Active(unit) {
                return;
            }
            let checks = st.checks.len();
            let (lane, stop) = (entry.lane, entry.stop);
            let drive = match &entry.job {
                Job::Download { local, .. } => drive_of(local),
                _ => String::new(),
            };
            match result {
                Ok(()) => st.mark_done(id, &self.writer, now, own_request),
                // A pending cancel wins over whatever the request answered, an auth failure
                // included (ruling Q37).
                Err(_) if stop == Some(Stop::Cancel) => st.mark_cancelled(id, &self.writer),
                Err(Failure::Provider(ProviderError::Auth { .. })) => {
                    st.requeue_front(lane, vec![id]);
                    st.stop_lane(lane, LaneState::AuthStopped);
                }
                // Every request to the site would be refused the same way until the clock is
                // set: one stop, not a failure per job (review I4).
                Err(Failure::Provider(ProviderError::ClockSkew)) => {
                    st.requeue_front(lane, vec![id]);
                    st.stop_lane(lane, LaneState::ClockSkew);
                }
                Err(Failure::Offline) => st.requeue_front(lane, vec![id]),
                Err(Failure::DiskFull) => {
                    st.requeue_front(lane, vec![id]);
                    st.hold_disk_full(drive);
                }
                Err(Failure::Provider(error)) => {
                    st.mark_failed(id, (self.describe)(&error), &self.writer);
                }
                Err(Failure::Reason(reason)) => st.mark_failed(id, reason, &self.writer),
            }
            st.checks.len() > checks
        };
        if checking {
            self.check_wake.notify_one();
        }
        self.poke();
    }

    /// What the listing of a folder of uploads due for their check found (ruling Q55). Each one
    /// there with the size it was sent with is done in the journal. One missing, of another size,
    /// or in a folder that is gone is queued again, or fails once it has been found missing
    /// [`MAX_CHECKS`] times. A listing that failed (after the retry policy's tries) tells nothing:
    /// those checks wait [`CHECK_RETRY`] and are made again, uncounted.
    fn checked(&self, folder: CheckFolder, listed: Result<Listing, ProviderError>) {
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            if st.shut_down {
                return;
            }
            let now = Instant::now();
            for id in folder.ids {
                // Gone meanwhile: its site was removed.
                let Some(mut check) = st.checks.remove(&id) else {
                    continue;
                };
                let remote = crate::lane::remote_of(&check.job);
                let there = match &listed {
                    Ok(listing) => listing.entries.iter().any(|entry| {
                        !entry.is_folder && entry.path == *remote && entry.size == check.size
                    }),
                    Err(ProviderError::NotFound) => false,
                    Err(_) => {
                        check.due = now + CHECK_RETRY;
                        st.checks.insert(id, check);
                        continue;
                    }
                };
                st.rechecked.checked += 1;
                if there {
                    self.writer.set_state(id, JobState::Done, None);
                    st.emit(check.lane, &check.job, check.size, JobEventOutcome::Done);
                    continue;
                }
                st.reopen(id, &check);
                if check.round >= MAX_CHECKS {
                    let reason = messages::LOST_AFTER_UPLOAD.to_owned();
                    st.mark_failed(id, reason, &self.writer);
                } else {
                    st.rechecked.resent += 1;
                    st.lanes[check.lane].queue.push_back(id);
                }
            }
        }
        self.poke();
    }

    /// The lane is offline: nothing should be sent on it now.
    pub(crate) fn lane_offline(&self, lane: usize) -> bool {
        self.lock().lanes[lane].state == LaneState::Offline
    }

    /// One request of `unit` on `lane`, for the jobs `ids` (one, or the files of a
    /// `delete_batch`), got no answer; `exhausted`: the jobs have no retries left. Says, for
    /// each job, whether to try again, to go back to the queue as it was, or to fail.
    ///
    /// The lane goes offline when this makes [`crate::offline::OFFLINE_AFTER`] errors within
    /// the window, or when the jobs have used up their retries. A job whose error takes the lane
    /// offline, or meets it offline, gets a strike, one per offline cycle (ruling Q50); at
    /// [`MAX_STRIKES`] it fails with the network error instead of sending the lane offline once
    /// more. A site that answers its probes but never this job's requests would otherwise hold
    /// it forever, with the offline banner blinking while the network is fine.
    pub(crate) fn network_error(
        self: &Arc<Self>,
        lane: usize,
        unit: UnitId,
        ids: &[JobId],
        exhausted: bool,
    ) -> Vec<Net> {
        let now = Instant::now();
        let (outcomes, take_offline) = {
            let mut guard = self.lock();
            let st = &mut *guard;
            let lane_ref = &mut st.lanes[lane];
            let (offline, cycle) = match lane_ref.state {
                LaneState::Offline => (true, lane_ref.cycle),
                LaneState::Running => (false, lane_ref.cycle + 1),
                // A stopped lane takes nothing until it is resumed: the jobs wait for that.
                LaneState::AuthStopped | LaneState::ClockSkew => {
                    let net = if exhausted { Net::Offline } else { Net::Retry };
                    return vec![net; ids.len()];
                }
            };
            let crowded = !offline && lane_ref.network.record(now);
            let goes_offline = !offline && (crowded || exhausted);
            if !offline && !goes_offline {
                return vec![Net::Retry; ids.len()];
            }
            let outcomes: Vec<Net> = ids
                .iter()
                .map(|&id| match st.jobs.get_mut(id) {
                    Some(entry) if entry.status == Status::Active(unit) => {
                        if entry.strike(cycle) {
                            Net::Fail
                        } else {
                            Net::Offline
                        }
                    }
                    _ => Net::Offline,
                })
                .collect();
            // Errors only of jobs that now fail don't take the lane offline: the site answers.
            let take_offline = goes_offline && outcomes.contains(&Net::Offline);
            if goes_offline && !take_offline {
                st.lanes[lane].network.clear();
            }
            (outcomes, take_offline)
        };
        if take_offline {
            self.go_offline(lane);
        }
        outcomes
    }

    /// Takes a running lane offline and starts probing its site. Nothing new starts on it; the
    /// work in flight is left alone until the first probe gets no answer either
    /// ([`Self::outage`]). A lane taken offline by one job whose requests a proxy keeps
    /// resetting comes back at that probe, and the transfers in flight beside the job were never
    /// touched (review N1): sending them back to the queue would restart each from byte 0, twice
    /// for every such job.
    fn go_offline(self: &Arc<Self>, lane: usize) {
        let start_probe = {
            let mut guard = self.lock();
            let st = &mut *guard;
            if st.shut_down {
                return;
            }
            let lane_ref = &mut st.lanes[lane];
            if lane_ref.state != LaneState::Running {
                return;
            }
            lane_ref.state = LaneState::Offline;
            lane_ref.network.clear();
            lane_ref.cycle += 1;
            // One probe per lane: one still waiting from a cycle that ended within its last
            // sleep takes this cycle on.
            !std::mem::replace(&mut lane_ref.probing, true)
        };
        if start_probe {
            crate::task::spawn_on(&self.rt, offline::probe(self.clone(), lane));
        }
        self.bump();
    }

    /// The first probe of the lane's offline cycle `cycle` got no answer either: the site is out
    /// of reach, not one job's requests. The work in flight on the lane goes back to the queue at
    /// once, its attempts unused (review M1), rather than wait on dead connections until they
    /// time out. Nothing changes when the lane has come back, or gone offline again, since.
    pub(crate) fn outage(&self, lane: usize, cycle: u64) {
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            let lane_ref = &st.lanes[lane];
            if st.shut_down || lane_ref.state != LaneState::Offline || lane_ref.cycle != cycle {
                return;
            }
            for (&unit, active) in &lane_ref.units {
                interrupt(&mut st.jobs, unit, active, Stop::Requeue);
            }
        }
        self.bump();
    }

    /// Whether `lane` is stopped (a refused sign-in, a clock that is off) until resumed.
    pub(crate) fn lane_stopped(&self, lane: usize) -> bool {
        matches!(
            self.lock().lanes[lane].state,
            LaneState::AuthStopped | LaneState::ClockSkew
        )
    }

    /// The part journal for one attempt at upload `id` of `unit` (ruling Q49), for the file as
    /// the attempt found it.
    pub(crate) fn job_parts(
        self: &Arc<Self>,
        unit: UnitId,
        id: JobId,
        file: FileStamp,
        provider: Arc<dyn StorageProvider>,
        remote: RemotePath,
    ) -> Arc<dyn PartJournal> {
        Arc::new(JobParts {
            inner: self.clone(),
            unit,
            id,
            file,
            provider,
            remote,
        })
    }

    /// The client to probe an offline lane's site with; `None` once the lane isn't offline or
    /// the engine has stopped.
    /// With its offline cycle. `None` also ends the lane's probing: the next time it goes offline
    /// starts a new probe.
    pub(crate) fn offline_provider(&self, lane: usize) -> Option<(Arc<dyn StorageProvider>, u64)> {
        let mut guard = self.lock();
        let st = &mut *guard;
        let shut_down = st.shut_down;
        let lane = &mut st.lanes[lane];
        if shut_down || lane.state != LaneState::Offline {
            lane.probing = false;
            return None;
        }
        Some((lane.provider.clone(), lane.cycle))
    }

    /// An offline lane's site answered: back to work. Its probe ends.
    pub(crate) fn back_online(&self, lane: usize) {
        {
            let mut st = self.lock();
            let lane = &mut st.lanes[lane];
            lane.probing = false;
            if lane.state != LaneState::Offline {
                return;
            }
            lane.state = LaneState::Running;
            lane.network.clear();
        }
        self.poke();
    }

    /// A unit's task has ended. Its slot is free; jobs it didn't finish are cancelled when
    /// asked to be, failed if the unit panicked, and otherwise go back to the front of the
    /// queue.
    pub(crate) fn unit_ended(&self, lane: usize, unit: UnitId, panicked: bool) {
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(active) = st.lanes[lane].units.remove(&unit) else {
                return;
            };
            st.lanes[lane].slots_mut(active.kind).in_use -= 1;
            let mut back = Vec::new();
            for id in active.jobs {
                let Some(entry) = st.jobs.get(id) else {
                    continue;
                };
                if entry.status != Status::Active(unit) {
                    continue;
                }
                if panicked {
                    let reason = messages::STOPPED_UNEXPECTEDLY.to_owned();
                    st.mark_failed(id, reason, &self.writer);
                } else if entry.stop == Some(Stop::Cancel) {
                    st.mark_cancelled(id, &self.writer);
                } else {
                    back.push(id);
                }
            }
            st.requeue_front(lane, back);
        }
        self.poke();
    }
}

/// What a job's request that got no answer leads to (see [`Inner::network_error`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Net {
    /// Try again, after the policy's wait.
    Retry,
    /// Back to the queue as it was, its attempts unused: the lane is offline.
    Offline,
    /// Fail with the network error: the job has struck out (ruling Q50).
    Fail,
}

/// The part journal of one attempt at an upload (ruling Q49): the job's multipart progress,
/// kept on its entry and written to the journal's multipart column through the ordered writer,
/// so the next attempt (after a failed one, a pause, an offline spell, or a crash and a resume)
/// carries on from the parts already stored.
struct JobParts {
    inner: Arc<Inner>,
    unit: UnitId,
    id: JobId,
    /// The file as this attempt found it. Recorded when the upload starts; parts recorded for a
    /// file that has changed since are not reused.
    file: FileStamp,
    /// Where an upload found stale is given up.
    provider: Arc<dyn StorageProvider>,
    remote: RemotePath,
}

impl JobParts {
    /// Changes the job's progress and journals it, while this attempt is the job's. Only an
    /// upload's progress is ever written: a delete's column holds its "possibly sent" mark,
    /// which an upload id must never overwrite (review C2 b).
    fn update(&self, change: impl FnOnce(&mut Option<MultipartProgress>)) {
        let mut st = self.inner.lock();
        let Some(entry) = st.jobs.get_mut(self.id) else {
            return;
        };
        if entry.status != Status::Active(self.unit) || !matches!(entry.job, Job::Upload { .. }) {
            return;
        }
        change(&mut entry.multipart);
        self.inner
            .writer
            .set_multipart(self.id, entry.multipart.clone());
    }
}

impl PartJournal for JobParts {
    /// The progress of an earlier attempt, if the file is still the one its parts were cut
    /// from. Progress for a file that changed since is forgotten, and its upload given up.
    fn load(&self) -> Option<MultipartProgress> {
        let stale = {
            let mut st = self.inner.lock();
            let entry = st.jobs.get_mut(self.id)?;
            if !matches!(entry.job, Job::Upload { .. }) {
                return None;
            }
            let progress = entry.multipart.as_ref()?;
            if progress.file == Some(self.file) {
                return Some(progress.clone());
            }
            let stale = entry.multipart.take()?;
            self.inner.writer.set_multipart(self.id, None);
            stale
        };
        if is_upload(&stale) {
            self.inner
                .aborts
                .send(self.provider.clone(), self.remote.clone(), stale.upload_id);
        }
        None
    }

    fn started(&self, upload_id: &str, part_size: u64) {
        let progress = MultipartProgress {
            upload_id: upload_id.to_owned(),
            part_size,
            parts: Vec::new(),
            file: Some(self.file),
        };
        self.update(|kept| *kept = Some(progress));
    }

    fn part_done(&self, number: u32, etag: &str) {
        self.update(|kept| {
            if let Some(progress) = kept {
                progress.parts.retain(|(done, _)| *done != number);
                progress.parts.push((number, etag.to_owned()));
            }
        });
    }

    fn finished(&self) {
        self.update(|kept| *kept = None);
    }

    /// The user cancelled the job: its upload is not kept for a resume.
    fn cancelled(&self) -> bool {
        let st = self.inner.lock();
        st.jobs
            .get(self.id)
            .is_none_or(|entry| entry.stop == Some(Stop::Cancel))
    }
}

/// Writes job states to the journal from one task, in the order they were made, off the
/// runtime's worker threads, and counts the writes that fail.
struct JournalWriter {
    tx: mpsc::UnboundedSender<JournalUpdate>,
    pending: Arc<AtomicUsize>,
    errors: Arc<AtomicU64>,
    /// Updates queued since the engine started, and how many of them the task has dealt with
    /// (written, or counted as failed). They are dealt with in the order they were queued, so a
    /// mark taken from the first is reached once the second gets to it.
    queued: AtomicU64,
    dealt_with: Arc<AtomicU64>,
}

impl JournalWriter {
    fn spawn(rt: &Handle, journal: Arc<dyn Journal>, changes: Arc<watch::Sender<u64>>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let dealt_with = Arc::new(AtomicU64::new(0));
        crate::task::spawn_on(
            rt,
            write_journal(
                rx,
                journal,
                pending.clone(),
                errors.clone(),
                dealt_with.clone(),
                changes,
            ),
        );
        Self {
            tx,
            pending,
            errors,
            queued: AtomicU64::new(0),
            dealt_with,
        }
    }

    /// Queues an update. Called with the state locked, so updates queue in the order the
    /// changes were made.
    fn set_state(&self, id: JobId, state: JobState, error: Option<&str>) {
        self.send(JournalUpdate::State {
            id: record_id(id),
            state,
            error: error.map(str::to_owned),
        });
    }

    /// Queues a change to a job's multipart column (see the module's notes).
    fn set_multipart(&self, id: JobId, progress: Option<MultipartProgress>) {
        self.send(JournalUpdate::Multipart {
            id: record_id(id),
            progress,
        });
    }

    fn send(&self, write: JournalUpdate) {
        self.queued.fetch_add(1, Ordering::SeqCst);
        self.pending.fetch_add(1, Ordering::SeqCst);
        if self.tx.send(write).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            self.dealt_with.fetch_add(1, Ordering::SeqCst);
            self.count_error();
        }
    }

    /// Updates queued and not yet written.
    fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Updates queued since the engine started.
    fn queued(&self) -> u64 {
        self.queued.load(Ordering::SeqCst)
    }

    /// Updates written, or failed, since the engine started.
    fn dealt_with(&self) -> u64 {
        self.dealt_with.load(Ordering::SeqCst)
    }

    /// A journal read or write failed.
    fn count_error(&self) {
        self.errors.fetch_add(1, Ordering::SeqCst);
    }

    /// The journal reads and writes that failed since the engine started.
    fn errors(&self) -> u64 {
        self.errors.load(Ordering::SeqCst)
    }
}

/// The updates waiting are taken up to [`JOURNAL_CHUNK`] at a time and written with one
/// [`Journal::apply`]: one transaction, one disk sync, however many there are (review C1:
/// cancelling 13,793 jobs one commit at a time took about 9 s, longer than closing waited).
///
/// A chunk that fails to be written stays at the head and is tried again after each of the
/// [`JOURNAL_RETRIES`] waits (a busy disk, a locked file); the updates queued meanwhile wait
/// behind it, so the order holds. Only once every try failed is it given up: the job's older
/// record stays, so after a crash a finished job may be offered again, or a failed one
/// forgotten. The job's state in memory is unaffected; the failure is counted (every update of
/// the chunk, since the journal can't say which were written), and the snapshot's
/// `journal_errors` lets the app warn.
async fn write_journal(
    mut rx: mpsc::UnboundedReceiver<JournalUpdate>,
    journal: Arc<dyn Journal>,
    pending: Arc<AtomicUsize>,
    errors: Arc<AtomicU64>,
    dealt_with: Arc<AtomicU64>,
    changes: Arc<watch::Sender<u64>>,
) {
    while let Some(first) = rx.recv().await {
        let mut updates = vec![first];
        while updates.len() < JOURNAL_CHUNK {
            match rx.try_recv() {
                Ok(update) => updates.push(update),
                Err(_) => break,
            }
        }
        let n = updates.len();
        let updates = Arc::new(updates);
        let mut waits = JOURNAL_RETRIES.iter();
        let written = loop {
            let (journal, chunk) = (journal.clone(), updates.clone());
            let written = crate::task::spawn_blocking(move || journal.apply(&chunk).is_ok())
                .await
                .unwrap_or(false);
            match waits.next() {
                Some(&wait) if !written => tokio::time::sleep(wait).await,
                _ => break written,
            }
        };
        if !written {
            errors.fetch_add(n as u64, Ordering::SeqCst);
        }
        dealt_with.fetch_add(n as u64, Ordering::SeqCst);
        pending.fetch_sub(n, Ordering::SeqCst);
        changes.send_modify(|n| *n = n.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use httporter_core::{Capabilities, MemoryJournal, MockOp, MockProvider};

    use super::*;

    fn progress(upload_id: &str) -> MultipartProgress {
        MultipartProgress {
            upload_id: upload_id.into(),
            part_size: 5 << 20,
            parts: vec![(1, "\"e1\"".into())],
            file: None,
        }
    }

    /// Review C2 (b), ruling Q55: the multipart column holds an upload's progress, a delete's
    /// "possibly sent" mark, or an upload's. A record's progress is read as an upload's only for
    /// an upload and never from a mark; a delete was possibly sent with anything there, an upload
    /// only with its own mark.
    #[test]
    fn only_an_upload_has_progress_and_each_reads_only_its_own_mark() {
        let upload = Job::Upload {
            local: "big.bin".into(),
            remote: RemotePath::parse("big.bin").unwrap(),
            size: 1,
        };
        let delete = Job::Delete {
            remote: RemotePath::parse("old.txt").unwrap(),
            folder: false,
        };
        let real = progress("u-1");
        let mark = delete_sent();
        let sent = upload_sent();
        assert!(is_upload(&real) && !is_upload(&mark) && !is_upload(&progress("")));
        assert!(!is_upload(&sent));
        assert_eq!(
            upload_progress(&upload, Some(real.clone())),
            Some(real.clone())
        );
        assert_eq!(upload_progress(&upload, Some(mark.clone())), None);
        assert_eq!(upload_progress(&upload, Some(sent.clone())), None);
        assert_eq!(upload_progress(&delete, Some(real.clone())), None);
        assert_eq!(upload_progress(&delete, Some(mark.clone())), None);
        assert_eq!(upload_progress(&upload, None), None);
        assert!(possibly_sent(&delete, Some(&mark)));
        assert!(possibly_sent(&delete, Some(&real)));
        assert!(!possibly_sent(&delete, None));
        assert!(!possibly_sent(&upload, Some(&real)));
        assert!(!possibly_sent(&upload, Some(&mark)));
        assert!(!possibly_sent(&upload, None));
        assert!(possibly_sent(&upload, Some(&sent)));
        // The words are part of the journal's file format.
        assert_eq!(UPLOAD_SENT, "httporter:upload-sent");
        assert_eq!(DELETE_SENT, "httporter:delete-sent");
    }

    /// Review C2 (b): a part journal told about an upload never writes its id onto a delete's
    /// record, whose column holds the "possibly sent" mark, even while the delete is the one in
    /// flight.
    #[tokio::test(start_paused = true)]
    async fn a_part_journal_never_writes_onto_a_delete() {
        let journal = Arc::new(MemoryJournal::new());
        let engine = Engine::new(
            Handle::current(),
            journal.clone(),
            TransferSettings::default(),
            |e| e.to_string(),
        );
        let mock = Arc::new(MockProvider::new(Capabilities::default()).with_file("old.txt", b"o"));
        mock.set_latency(MockOp::DeleteFile, Duration::from_secs(60));
        let remote = RemotePath::parse("old.txt").unwrap();
        engine
            .submit(Batch {
                site: SiteId("s".into()),
                site_name: "S".into(),
                host: "h".into(),
                provider: mock.clone(),
                jobs: vec![Job::Delete {
                    remote: remote.clone(),
                    folder: false,
                }],
                checksums: HashMap::new(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let id = JobId(1);
        let Status::Active(unit) = engine.inner.lock().jobs.get(id).unwrap().status else {
            panic!("the delete is not in flight");
        };
        let parts = engine
            .inner
            .job_parts(unit, id, FileStamp::new(1, None), mock.clone(), remote);
        parts.started("u-1", 5 << 20);
        parts.part_done(1, "\"e1\"");
        assert_eq!(parts.load(), None);
        tokio::time::sleep(Duration::from_secs(1)).await;

        let record = journal.records()[0].clone();
        assert_eq!(record.multipart, Some(delete_sent()));
        parts.finished();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(journal.records()[0].multipart, Some(delete_sent()));
        assert!(
            engine
                .inner
                .lock()
                .jobs
                .get(id)
                .unwrap()
                .multipart
                .is_none()
        );
    }

    #[test]
    fn directions() {
        let of = |kinds: &[JobKind]| {
            let mut seen = Kinds::default();
            for &kind in kinds {
                seen.note(kind);
            }
            seen.direction()
        };
        assert_eq!(of(&[]), Direction::Upload);
        assert_eq!(
            of(&[JobKind::Upload, JobKind::CreateFolder]),
            Direction::Upload
        );
        assert_eq!(of(&[JobKind::Download]), Direction::Download);
        assert_eq!(of(&[JobKind::Delete]), Direction::Delete);
        assert_eq!(of(&[JobKind::Upload, JobKind::Delete]), Direction::Mixed);
        assert_eq!(of(&[JobKind::Download, JobKind::Delete]), Direction::Mixed);
    }

    #[test]
    fn job_ids_are_record_ids_from_1() {
        assert_eq!(job_id(1), Some(JobId(1)));
        assert_eq!(job_id(0), None);
        assert_eq!(job_id(-3), None);
        assert_eq!(record_id(JobId(42)), 42);
    }

    #[test]
    fn the_slab_reuses_freed_slots() {
        let mut slab = Slab::default();
        let entry = || {
            Entry::new(
                Job::CreateFolder {
                    remote: httporter_core::RemotePath::root(),
                },
                0,
                0,
                0,
            )
        };
        slab.insert(JobId(1), entry());
        slab.insert(JobId(2), entry());
        assert!(slab.remove(JobId(1)).is_some());
        assert!(slab.get(JobId(1)).is_none());
        slab.insert(JobId(3), entry());
        assert_eq!(slab.slots.len(), 2);
        assert!(slab.get(JobId(2)).is_some() && slab.get(JobId(3)).is_some());
        assert!(slab.remove(JobId(9)).is_none());
    }
}
