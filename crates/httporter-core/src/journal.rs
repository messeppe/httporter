//! The transfer journal: every job is written down before it starts and marked when it ends, so
//! after a crash the unfinished ones can be offered again, and only those.

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::{Job, MultipartProgress, SiteId};

/// Where a journalled job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    Queued,
    Done,
    Failed,
    Cancelled,
}

/// One journalled job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// Assigned by [`Journal::insert`]: at least 1. The engine uses it as the job's `JobId`.
    pub id: i64,
    /// The batch the job was submitted in.
    pub batch: String,
    pub site: SiteId,
    pub job: Job,
    pub state: JobState,
    /// Why the job failed, in plain words.
    pub error: Option<String>,
    /// How far a multipart upload got, so it can resume.
    pub multipart: Option<MultipartProgress>,
}

/// The journal could not be read or written.
#[derive(Debug, thiserror::Error)]
#[error("journal: {0}")]
pub struct JournalError(pub String);

/// Durable storage for the transfer engine's jobs. Updates to an id the journal does not hold
/// (a record already cleared away) are ignored.
pub trait Journal: Send + Sync {
    /// Writes `records` all at once (one transaction) and sets each one's `id`, in order.
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError>;
    /// Moves a job to `state`, with the reason when it failed (`None` clears an earlier one).
    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError>;
    /// Records how far a multipart upload got; `None` forgets it.
    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError>;
    /// The Queued and Failed jobs, in id order.
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError>;
    /// Drops the Done and Cancelled jobs.
    fn clear_finished(&self) -> Result<(), JournalError>;
    /// Drops the Queued and Failed jobs, when the user chooses not to resume them.
    fn discard_unfinished(&self) -> Result<(), JournalError>;
}

/// A [`Journal`] in memory, for tests. Ids start at 1.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    inner: Mutex<Records>,
}

#[derive(Debug, Default)]
struct Records {
    /// In id order.
    records: Vec<JobRecord>,
    last_id: i64,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record held, finished or not, in id order.
    pub fn records(&self) -> Vec<JobRecord> {
        self.lock().records.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Records> {
        // No change here can stop part-way through a record, so a poisoned lock still holds
        // whole records.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, id: i64, change: impl FnOnce(&mut JobRecord)) {
        if let Some(record) = self.lock().records.iter_mut().find(|r| r.id == id) {
            change(record);
        }
    }
}

fn is_unfinished(record: &JobRecord) -> bool {
    matches!(record.state, JobState::Queued | JobState::Failed)
}

impl Journal for MemoryJournal {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError> {
        let mut inner = self.lock();
        for record in records {
            inner.last_id += 1;
            record.id = inner.last_id;
            inner.records.push(record.clone());
        }
        Ok(())
    }

    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError> {
        self.update(id, |record| {
            record.state = state;
            record.error = error.map(str::to_owned);
        });
        Ok(())
    }

    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError> {
        self.update(id, |record| record.multipart = mp.cloned());
        Ok(())
    }

    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError> {
        Ok(self
            .lock()
            .records
            .iter()
            .filter(|r| is_unfinished(r))
            .cloned()
            .collect())
    }

    fn clear_finished(&self) -> Result<(), JournalError> {
        self.lock().records.retain(is_unfinished);
        Ok(())
    }

    fn discard_unfinished(&self) -> Result<(), JournalError> {
        self.lock().records.retain(|r| !is_unfinished(r));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RemotePath;

    fn upload(name: &str) -> JobRecord {
        JobRecord {
            id: 0,
            batch: "batch-1".into(),
            site: SiteId("site-1".into()),
            job: Job::Upload {
                local: format!(r"C:\site\{name}").into(),
                remote: RemotePath::parse(name).unwrap(),
                size: 10,
            },
            state: JobState::Queued,
            error: None,
            multipart: None,
        }
    }

    fn ids(records: &[JobRecord]) -> Vec<i64> {
        records.iter().map(|r| r.id).collect()
    }

    #[test]
    fn memory_journal_insert_assigns_ids_and_unfinished_lists_queued_and_failed() {
        let journal = MemoryJournal::new();
        let mut first = [upload("a"), upload("b"), upload("c"), upload("d")];
        journal.insert(&mut first).unwrap();
        assert_eq!(ids(&first), [1, 2, 3, 4]);
        let mut second = [upload("e")];
        journal.insert(&mut second).unwrap();
        assert_eq!(ids(&second), [5]);

        journal.set_state(1, JobState::Done, None).unwrap();
        journal
            .set_state(2, JobState::Failed, Some("No connection"))
            .unwrap();
        journal.set_state(3, JobState::Cancelled, None).unwrap();

        let unfinished = journal.unfinished().unwrap();
        assert_eq!(ids(&unfinished), [2, 4, 5]);
        assert_eq!(unfinished[0].state, JobState::Failed);
        assert_eq!(unfinished[0].error.as_deref(), Some("No connection"));
        assert_eq!(unfinished[1], first[3]);
    }

    #[test]
    fn clear_finished_keeps_unfinished() {
        let journal = MemoryJournal::new();
        let mut records = [upload("a"), upload("b"), upload("c"), upload("d")];
        journal.insert(&mut records).unwrap();
        journal.set_state(1, JobState::Done, None).unwrap();
        journal.set_state(2, JobState::Cancelled, None).unwrap();
        journal.set_state(3, JobState::Failed, Some("x")).unwrap();

        journal.clear_finished().unwrap();
        assert_eq!(ids(&journal.unfinished().unwrap()), [3, 4]);
        assert_eq!(ids(&journal.records()), [3, 4]);

        journal.discard_unfinished().unwrap();
        assert!(journal.records().is_empty());
    }

    #[test]
    fn set_multipart_records_and_clears_progress() {
        let journal = MemoryJournal::new();
        let mut records = [upload("big.zip")];
        journal.insert(&mut records).unwrap();
        let progress = MultipartProgress {
            upload_id: "u-1".into(),
            part_size: 16 << 20,
            parts: vec![(1, "\"e1\"".into())],
        };
        journal.set_multipart(1, Some(&progress)).unwrap();
        assert_eq!(journal.unfinished().unwrap()[0].multipart, Some(progress));
        journal.set_multipart(1, None).unwrap();
        assert_eq!(journal.unfinished().unwrap()[0].multipart, None);
    }

    /// The engine may report on a job after its record was cleared away.
    #[test]
    fn updates_to_an_unknown_id_are_ignored() {
        let journal = MemoryJournal::new();
        journal.set_state(7, JobState::Done, None).unwrap();
        journal.set_multipart(7, None).unwrap();
        assert!(journal.records().is_empty());
    }

    #[test]
    fn a_shared_journal_is_one_journal() {
        let journal: std::sync::Arc<dyn Journal> = std::sync::Arc::new(MemoryJournal::new());
        let other = journal.clone();
        journal.insert(&mut [upload("a")]).unwrap();
        assert_eq!(other.unfinished().unwrap().len(), 1);
    }
}
