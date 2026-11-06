// SPDX-License-Identifier: MIT OR Apache-2.0

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
    /// Assigned by [`Journal::insert`]: at least 1, and never reused (see [`Journal`]). The
    /// engine uses it as the job's `JobId`.
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

/// One change to a journalled job, for [`Journal::apply`]: what [`Journal::set_state`] or
/// [`Journal::set_multipart`] would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalUpdate {
    State {
        id: i64,
        state: JobState,
        /// Why the job failed; `None` clears an earlier reason.
        error: Option<String>,
    },
    Multipart {
        id: i64,
        progress: Option<MultipartProgress>,
    },
}

/// Durable storage for the transfer engine's jobs.
///
/// Ids are unique for the journal's whole lifetime and never reused, even after their records
/// are cleared or discarded. Updates to an id the journal no longer holds are therefore ignored,
/// and can never land on a newer job. An SQLite journal needs `INTEGER PRIMARY KEY
/// AUTOINCREMENT` for this: without `AUTOINCREMENT`, SQLite hands out the highest ids again once
/// their rows are deleted.
pub trait Journal: Send + Sync {
    /// Writes `records` all at once (one transaction) and sets each one's `id`, in order. Each
    /// new id is greater than every id handed out before, including those of records since
    /// cleared or discarded.
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError>;
    /// Moves a job to `state`, with the reason when it failed (`None` clears an earlier one).
    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError>;
    /// Records how far a multipart upload got; `None` forgets it.
    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError>;
    /// Writes `updates`, in order. A journal that can should write them together, at the cost of
    /// one write (one transaction, one disk sync): cancelling 10,000 jobs must not take 10,000
    /// syncs. The default writes them one at a time and stops at the first that fails. On an
    /// error, any of them may be unwritten.
    fn apply(&self, updates: &[JournalUpdate]) -> Result<(), JournalError> {
        for update in updates {
            match update {
                JournalUpdate::State { id, state, error } => {
                    self.set_state(*id, *state, error.as_deref())?;
                }
                JournalUpdate::Multipart { id, progress } => {
                    self.set_multipart(*id, progress.as_ref())?;
                }
            }
        }
        Ok(())
    }
    /// The Queued and Failed jobs, in id order.
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError>;
    /// Drops the Done and Cancelled jobs.
    fn clear_finished(&self) -> Result<(), JournalError>;
    /// Drops the Queued and Failed jobs, when the user chooses not to resume them.
    fn discard_unfinished(&self) -> Result<(), JournalError>;
}

/// A [`Journal`] in memory, for tests. Ids start at 1 and only go up.
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
            file: None,
        };
        journal.set_multipart(1, Some(&progress)).unwrap();
        assert_eq!(journal.unfinished().unwrap()[0].multipart, Some(progress));
        journal.set_multipart(1, None).unwrap();
        assert_eq!(journal.unfinished().unwrap()[0].multipart, None);
    }

    /// Review finding: a cleared id never comes back, or a late update to the old job would
    /// land on a new one and two drawer rows could share a `JobId`.
    #[test]
    fn ids_are_never_reused_after_clear_or_discard() {
        let journal = MemoryJournal::new();
        let mut first = [upload("a"), upload("b"), upload("c")];
        journal.insert(&mut first).unwrap();
        journal.set_state(3, JobState::Done, None).unwrap();
        journal.clear_finished().unwrap();
        journal.discard_unfinished().unwrap();
        assert!(journal.records().is_empty());

        let mut second = [upload("d"), upload("e")];
        journal.insert(&mut second).unwrap();
        let earlier = first.iter().map(|r| r.id).max().unwrap();
        assert!(second.iter().all(|r| r.id > earlier), "{:?}", ids(&second));
    }

    /// The engine may report on a job after its record was cleared away.
    #[test]
    fn updates_to_an_unknown_id_are_ignored() {
        let journal = MemoryJournal::new();
        journal.set_state(7, JobState::Done, None).unwrap();
        journal.set_multipart(7, None).unwrap();
        assert!(journal.records().is_empty());
    }

    /// Review C1: the default `apply` writes each update in order, as the single calls would.
    #[test]
    fn apply_writes_every_update_in_order() {
        let journal = MemoryJournal::new();
        journal
            .insert(&mut [upload("a"), upload("b"), upload("c")])
            .unwrap();
        let progress = MultipartProgress {
            upload_id: "u-1".into(),
            part_size: 5 << 20,
            parts: Vec::new(),
            file: None,
        };
        journal
            .apply(&[
                JournalUpdate::State {
                    id: 1,
                    state: JobState::Failed,
                    error: Some("first".into()),
                },
                JournalUpdate::State {
                    id: 1,
                    state: JobState::Cancelled,
                    error: None,
                },
                JournalUpdate::Multipart {
                    id: 2,
                    progress: Some(progress.clone()),
                },
                JournalUpdate::State {
                    id: 3,
                    state: JobState::Done,
                    error: None,
                },
            ])
            .unwrap();
        let records = journal.records();
        assert_eq!(
            (records[0].state, records[0].error.clone()),
            (JobState::Cancelled, None)
        );
        assert_eq!(records[1].multipart, Some(progress));
        assert_eq!(records[2].state, JobState::Done);
        assert_eq!(ids(&journal.unfinished().unwrap()), [2]);
    }

    #[test]
    fn a_shared_journal_is_one_journal() {
        let journal: std::sync::Arc<dyn Journal> = std::sync::Arc::new(MemoryJournal::new());
        let other = journal.clone();
        journal.insert(&mut [upload("a")]).unwrap();
        assert_eq!(other.unfinished().unwrap().len(), 1);
    }
}
