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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::Utc;
use httporter_core::{
    BatchSummary, Checksum, Direction, EngineSnapshot, EngineState, Job, JobId, JobKind, JobRecord,
    JobState, JobView, Journal, JournalError, MultipartProgress, ProviderError, RemotePath, SiteId,
    StorageProvider, TransferSettings,
};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc, watch};
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

/// A site as [`Engine::resume_from_journal`] needs it: its name, its host, and its client.
pub type SiteInfo = (String, String, Arc<dyn StorageProvider>);

/// What a site the journal names is now; `None` when the site no longer exists.
pub type SiteLookup<'a> = &'a dyn Fn(&SiteId) -> Option<SiteInfo>;

/// How many jobs each of a snapshot's lists shows at most.
pub const WINDOW: usize = 200;

/// The journal writer takes at most this many updates in one go.
const JOURNAL_CHUNK: usize = 512;

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
    describe: fn(&ProviderError) -> String,
    pub(crate) bucket: TokenBucket,
    state: Mutex<State>,
    /// Wakes the dispatcher.
    wake: Notify,
    changes: Arc<watch::Sender<u64>>,
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
    /// A delete request for it may have reached the server, so a later `NotFound` means done.
    sent: bool,
    /// What a download's contents must hash to, when its batch said.
    checksum: Option<Checksum>,
    /// How many times it used up its retries on network errors (see [`MAX_STRIKES`]).
    strikes: u8,
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
        let inner = Arc::new(Inner {
            rt: rt.clone(),
            journal,
            writer,
            describe,
            bucket: TokenBucket::new(settings.bandwidth_limit_bytes()),
            state: Mutex::new(State::new(settings)),
            wake: Notify::new(),
            changes,
        });
        rt.spawn(dispatch_loop(inner.clone()));
        Self {
            _owner: Arc::new(Owner(inner.clone())),
            inner,
        }
    }

    /// New settings, for work starting from now on (and the bandwidth limit at once).
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

    /// Journals every job of `batch` (one transaction), then queues them on the site's lane.
    /// Returns the batch id (a UUID). A batch the journal can't record is refused whole, and
    /// nothing of it runs (ruling Q21). A batch without jobs changes nothing.
    pub fn submit(&self, batch: Batch) -> Result<String, JournalError> {
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
            for (id, record) in ids.into_iter().zip(records) {
                let job = record.job;
                let size = size_of(&job);
                let checksum = match &job {
                    Job::Download { remote, .. } => checksums.get(remote).cloned(),
                    _ => None,
                };
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

    /// Restarts a site stopped by an auth failure, with the edited site's client.
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

    /// Cancels everything of `site` not yet done: its queued, paused, running and failed jobs
    /// (the site is being removed). The other sites carry on.
    pub fn cancel_site(&self, site: &SiteId) {
        {
            let mut guard = self.inner.lock();
            let st = &mut *guard;
            let Some(&lane) = st.lane_of.get(site) else {
                return;
            };
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
        }
        self.inner.poke();
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

    /// Something is queued (paused ones too), running, or not yet written to the journal.
    pub fn is_busy(&self) -> bool {
        self.inner.lock().has_work() || self.inner.writer.pending() > 0
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
        let records = match self.inner.journal.unfinished() {
            Ok(records) => records,
            Err(_) => {
                self.inner.writer.count_error();
                return 0;
            }
        };
        let st = self.inner.lock();
        records
            .iter()
            .filter(|record| job_id(record.id).is_some_and(|id| st.jobs.get(id).is_none()))
            .count()
    }

    /// Re-queues every unfinished journal record whose site `lookup` still knows, at the back
    /// of its site's queue, and returns how many. Failed records go back to the queue too: a
    /// resume is a retry. A delete marked as possibly sent treats `NotFound` as done.
    ///
    /// The records of a site that no longer exists are marked Cancelled, so they aren't
    /// offered again; records this engine already holds are left alone. Downloads resume
    /// unverified (their listed checksums aren't journalled).
    pub fn resume_from_journal(&self, lookup: SiteLookup<'_>) -> usize {
        let records = match self.inner.journal.unfinished() {
            Ok(records) => records,
            Err(_) => {
                self.inner.writer.count_error();
                return 0;
            }
        };
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
                if st.jobs.get(id).is_some() {
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
                let sent = matches!(job, Job::Delete { .. }) && record.multipart.is_some();
                let mut entry = Entry::new(job, lane, seq, size);
                entry.sent = sent;
                st.lanes[lane].queue.push_back(id);
                st.jobs.insert(id, entry);
                resumed += 1;
            }
        }
        self.inner.poke();
        resumed
    }

    /// Drops the unfinished jobs a crash left in the journal, when the user chooses not to
    /// resume them. The jobs this engine holds stay journalled.
    pub fn discard_journal(&self) {
        let holds_unfinished = {
            let st = self.inner.lock();
            st.has_work() || !st.failed.is_empty()
        };
        let journal = &self.inner.journal;
        let result = if holds_unfinished {
            journal.unfinished().and_then(|records| {
                let others: Vec<i64> = {
                    let st = self.inner.lock();
                    records
                        .iter()
                        .map(|record| record.id)
                        .filter(|&id| job_id(id).is_none_or(|id| st.jobs.get(id).is_none()))
                        .collect()
                };
                others
                    .into_iter()
                    .try_for_each(|id| journal.set_state(id, JobState::Cancelled, None))
            })
        } else {
            journal.discard_unfinished()
        };
        if result.is_err() {
            self.inner.writer.count_error();
        }
        self.inner.bump();
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
            inner.rt.spawn(run_unit(inner.clone(), spec));
        }
        inner.bump();
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
            checksum: None,
            strikes: 0,
        }
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
    fn new(settings: TransferSettings) -> Self {
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
        }
    }

    fn has_work(&self) -> bool {
        self.lanes.iter().any(Lane::has_work)
    }

    /// A new run: the done jobs are let go and the totals start again from the failed jobs,
    /// which carry over.
    fn start_run(&mut self, now: Instant) {
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
    /// for the first time is marked possibly sent, in memory and in the journal.
    fn dispatch(&mut self, writer: &JournalWriter) -> Vec<UnitSpec> {
        let mut units = Vec::new();
        if self.paused || self.shut_down || self.disk_full.is_some() {
            return units;
        }
        let policy = self.settings.retry_policy();
        let verify = self.settings.verify_checksums;
        for lane in 0..self.lanes.len() {
            while let Some((kind, ids)) = self.lanes[lane].next_unit(&self.jobs) {
                let unit = UnitId(self.next_unit);
                self.next_unit += 1;
                let mut items = Vec::with_capacity(ids.len());
                for &id in &ids {
                    if let Some(entry) = self.jobs.get_mut(id) {
                        entry.status = Status::Active(unit);
                        entry.stop = None;
                        let sent = entry.sent;
                        if matches!(entry.job, Job::Delete { .. }) && !sent {
                            entry.sent = true;
                            writer.set_multipart(id, Some(delete_sent()));
                        }
                        items.push((id, entry.job.clone(), sent, entry.checksum.clone()));
                    }
                }
                let work = match items.len() {
                    1 => {
                        let (id, job, sent, checksum) = items.remove(0);
                        work_for(id, job, sent, checksum)
                    }
                    _ => Work::DeleteFiles {
                        items: items
                            .into_iter()
                            .map(|(id, job, sent, _)| {
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
        writer.set_state(id, JobState::Done, None);
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
        entry.error = Some(reason);
        entry.stop = None;
        let (size, batch) = (entry.size, entry.batch);
        self.run.bytes_failed += size;
        self.failed.insert(id);
        if let Some(info) = self.batches.get_mut(&batch) {
            info.open -= 1;
            info.failed += 1;
        }
        self.batch_changed(batch, true);
    }

    /// Cancels a job that is queued (already out of its queue), active or failed, and lets it
    /// go.
    fn mark_cancelled(&mut self, id: JobId, writer: &JournalWriter) {
        self.leave_active(id);
        let Some(entry) = self.jobs.remove(id) else {
            return;
        };
        writer.set_state(id, JobState::Cancelled, None);
        let was_failed = entry.status == Status::Failed;
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
        let (lane, batch, size) = (entry.lane, entry.batch, entry.size);
        self.failed.remove(&id);
        self.run.bytes_failed -= size;
        if let Some(info) = self.batches.get_mut(&batch) {
            info.failed -= 1;
            info.open += 1;
        }
        self.lanes[lane].queue.push_back(id);
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

    /// The site refused its credentials: its lane stops and everything in flight on it goes
    /// back to the queue.
    fn auth_stop(&mut self, lane: usize) {
        self.lanes[lane].state = LaneState::AuthStopped;
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
            .find(|lane| lane.state == LaneState::Offline && lane.has_work())
        {
            return EngineState::Offline {
                site: lane.name.clone(),
            };
        }
        if self.paused && self.has_work() {
            EngineState::Paused
        } else if self.lanes.iter().any(Lane::is_moving) {
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
        }
    }
}

/// The journal's mark on a delete that may have been sent (see the module's notes).
fn delete_sent() -> MultipartProgress {
    MultipartProgress {
        upload_id: DELETE_SENT.to_owned(),
        part_size: 0,
        parts: Vec::new(),
    }
}

/// The work a single job does.
fn work_for(id: JobId, job: Job, sent: bool, checksum: Option<Checksum>) -> Work {
    match job {
        Job::Upload { local, remote, .. } => Work::Upload { id, local, remote },
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
        {
            let mut guard = self.lock();
            let st = &mut *guard;
            let Some(entry) = st.jobs.get(id) else {
                return;
            };
            if entry.status != Status::Active(unit) {
                return;
            }
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
                    st.auth_stop(lane);
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
        }
        self.poke();
    }

    /// The lane is offline: nothing should be sent on it now.
    pub(crate) fn lane_offline(&self, lane: usize) -> bool {
        self.lock().lanes[lane].state == LaneState::Offline
    }

    /// A request on `lane` got no answer. Returns whether the lane is offline now: it already
    /// was, or this error makes too many too close together.
    pub(crate) fn network_error(self: &Arc<Self>, lane: usize) -> bool {
        let now = Instant::now();
        {
            let mut st = self.lock();
            let lane_ref = &mut st.lanes[lane];
            match lane_ref.state {
                LaneState::Offline => return true,
                LaneState::AuthStopped => return false,
                LaneState::Running => {}
            }
            if !lane_ref.network.record(now) {
                return false;
            }
        }
        self.go_offline(lane);
        true
    }

    /// Job `id` of `unit` used up its retries on network errors. Its lane goes offline and the
    /// job goes back to the queue, unless it has done this [`MAX_STRIKES`] times: then it fails
    /// (returns true).
    pub(crate) fn network_exhausted(
        self: &Arc<Self>,
        lane: usize,
        unit: UnitId,
        id: JobId,
    ) -> bool {
        {
            let mut st = self.lock();
            if let Some(entry) = st.jobs.get_mut(id)
                && entry.status == Status::Active(unit)
            {
                entry.strikes = entry.strikes.saturating_add(1);
                if entry.strikes >= MAX_STRIKES {
                    return true;
                }
            }
        }
        self.go_offline(lane);
        false
    }

    /// Takes a running lane offline and starts probing its site.
    fn go_offline(self: &Arc<Self>, lane: usize) {
        {
            let mut st = self.lock();
            if st.shut_down {
                return;
            }
            let lane_ref = &mut st.lanes[lane];
            if lane_ref.state != LaneState::Running {
                return;
            }
            lane_ref.state = LaneState::Offline;
            lane_ref.network.clear();
        }
        self.rt.spawn(offline::probe(self.clone(), lane));
        self.bump();
    }

    /// The client to probe an offline lane's site with; `None` once the lane isn't offline or
    /// the engine has stopped.
    pub(crate) fn offline_provider(&self, lane: usize) -> Option<Arc<dyn StorageProvider>> {
        let st = self.lock();
        let lane = &st.lanes[lane];
        (!st.shut_down && lane.state == LaneState::Offline).then(|| lane.provider.clone())
    }

    /// An offline lane's site answered: back to work.
    pub(crate) fn back_online(&self, lane: usize) {
        {
            let mut st = self.lock();
            let lane = &mut st.lanes[lane];
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

/// One journal update.
enum Write {
    State {
        id: i64,
        state: JobState,
        error: Option<String>,
    },
    Multipart {
        id: i64,
        progress: Option<MultipartProgress>,
    },
}

impl Write {
    fn apply(&self, journal: &dyn Journal) -> Result<(), JournalError> {
        match self {
            Self::State { id, state, error } => journal.set_state(*id, *state, error.as_deref()),
            Self::Multipart { id, progress } => journal.set_multipart(*id, progress.as_ref()),
        }
    }
}

/// Writes job states to the journal from one task, in the order they were made, off the
/// runtime's worker threads, and counts the writes that fail.
struct JournalWriter {
    tx: mpsc::UnboundedSender<Write>,
    pending: Arc<AtomicUsize>,
    errors: Arc<AtomicU64>,
}

impl JournalWriter {
    fn spawn(rt: &Handle, journal: Arc<dyn Journal>, changes: Arc<watch::Sender<u64>>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        rt.spawn(write_journal(
            rx,
            journal,
            pending.clone(),
            errors.clone(),
            changes,
        ));
        Self {
            tx,
            pending,
            errors,
        }
    }

    /// Queues an update. Called with the state locked, so updates queue in the order the
    /// changes were made.
    fn set_state(&self, id: JobId, state: JobState, error: Option<&str>) {
        self.send(Write::State {
            id: record_id(id),
            state,
            error: error.map(str::to_owned),
        });
    }

    /// Queues a change to a job's multipart column (see the module's notes on deletes).
    fn set_multipart(&self, id: JobId, progress: Option<MultipartProgress>) {
        self.send(Write::Multipart {
            id: record_id(id),
            progress,
        });
    }

    fn send(&self, write: Write) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        if self.tx.send(write).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            self.count_error();
        }
    }

    /// Updates queued and not yet written.
    fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
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

/// A write that fails leaves the job's older record: after a crash a finished job may be
/// offered again, or a failed one forgotten. The job's state in memory is unaffected; the
/// failure is counted, and the snapshot's `journal_errors` lets the app warn.
async fn write_journal(
    mut rx: mpsc::UnboundedReceiver<Write>,
    journal: Arc<dyn Journal>,
    pending: Arc<AtomicUsize>,
    errors: Arc<AtomicU64>,
    changes: Arc<watch::Sender<u64>>,
) {
    while let Some(first) = rx.recv().await {
        let mut writes = vec![first];
        while writes.len() < JOURNAL_CHUNK {
            match rx.try_recv() {
                Ok(write) => writes.push(write),
                Err(_) => break,
            }
        }
        let n = writes.len();
        let journal = journal.clone();
        let failed = tokio::task::spawn_blocking(move || {
            writes
                .iter()
                .filter(|write| write.apply(&*journal).is_err())
                .count()
        })
        .await
        .unwrap_or(n);
        if failed > 0 {
            errors.fetch_add(failed as u64, Ordering::SeqCst);
        }
        pending.fetch_sub(n, Ordering::SeqCst);
        changes.send_modify(|n| *n = n.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
