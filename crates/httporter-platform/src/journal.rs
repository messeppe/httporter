// SPDX-License-Identifier: MIT OR Apache-2.0

//! The transfer journal in SQLite: [`SqliteJournal`] implements core's [`Journal`] on the
//! `journal` table of the HTTPorter database file.
//!
//! It keeps its own connection, separate from the [`Store`](crate::Store)'s: WAL mode lets the
//! two share the file, so the engine's constant small writes never wait on the UI's reads of
//! sites and settings.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::Utc;
use httporter_core::{
    Job, JobRecord, JobState, Journal, JournalError, JournalUpdate, MultipartProgress, RemotePath,
    SiteId,
};
use rusqlite::{Connection, Row, TransactionBehavior, params};

use crate::store::{
    StoreError, is_row_decode_error, open_connection, open_in_memory_connection, stamp,
};

/// A [`Journal`] kept in the HTTPorter database file (or, for tests, in memory).
///
/// **Owners (I6).** The window and the CLI can run at once, each with its own journal on the
/// same file. A journal made [`with_owner`](Self::with_owner) writes its owner ([`process_owner`])
/// into every row it inserts, and leaves alone the unfinished rows of *another process that is
/// still running* ([`owner_alive`]): [`Journal::unfinished`] doesn't return them, and the two
/// clean-ups don't delete them. Rows without an owner (written before schema v5, or by a journal
/// without one) are "an old run, gone".
#[derive(Debug)]
pub struct SqliteJournal {
    conn: Mutex<Connection>,
    /// This process's owner text, written into every row inserted; `None` writes NULL.
    owner: Option<String>,
}

impl SqliteJournal {
    /// Marks every row this journal inserts as this process's (`owner`, normally
    /// [`process_owner`]). Rows of other owners still running are then left alone.
    pub fn with_owner(mut self, owner: String) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Opens the journal in the database file at `path`, creating the file and bringing its
    /// schema up to date if needed (the store may not have opened it yet).
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::with_connection(open_connection(path)?)
    }

    /// A journal that lives only as long as the returned value. It is not the same database
    /// as an in-memory [`Store`](crate::Store).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::with_connection(open_in_memory_connection()?)
    }

    fn with_connection(conn: Connection) -> Result<Self, StoreError> {
        // FULL: a commit in WAL mode waits for the log to reach the disk, so a state change
        // that was reported as saved survives a power cut, not only a crash of the app. The
        // journal is what lets a killed publish resume without re-uploading finished files
        // (spec S5), so it must not lose its last few changes. The cost is one disk sync per
        // commit, which is small next to the upload each commit follows.
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self {
            conn: Mutex::new(conn),
            owner: None,
        })
    }

    /// The owners in the journal that are another process still running: a JSON array, for
    /// `owner NOT IN (SELECT value FROM json_each(?))`. An owner that can't be read counts as gone.
    fn live_others(&self, conn: &Connection) -> Result<String, JournalError> {
        let mut stmt = conn
            .prepare_cached("SELECT DISTINCT owner FROM journal WHERE owner IS NOT NULL")
            .map_err(db)?;
        let owners = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db)?
            .filter_map(Result::ok)
            .filter(|owner| self.owner.as_deref() != Some(owner.as_str()) && owner_alive(owner))
            .collect::<Vec<String>>();
        serde_json::to_string(&owners).map_err(|e| JournalError(e.to_string()))
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // Every change here is one SQL statement or one transaction, which a panic part-way
        // rolls back, so a poisoned lock still guards a consistent database.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn db(e: rusqlite::Error) -> JournalError {
    JournalError(e.to_string())
}

/// The words stored for each state. They are part of the file format: never rename one.
fn state_to_text(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

fn state_from_text(text: &str) -> Option<JobState> {
    match text {
        "queued" => Some(JobState::Queued),
        "done" => Some(JobState::Done),
        "failed" => Some(JobState::Failed),
        "cancelled" => Some(JobState::Cancelled),
        _ => None,
    }
}

/// The columns a [`Job`] is stored in. The kind words are the ones core's `JobKind` uses in
/// JSON.
struct JobColumns<'a> {
    kind: &'static str,
    local: Option<&'a str>,
    remote: &'a str,
    size: i64,
    folder: bool,
}

impl<'a> JobColumns<'a> {
    fn of(job: &'a Job) -> Result<Self, JournalError> {
        Ok(match job {
            Job::Upload {
                local,
                remote,
                size,
            } => Self {
                kind: "upload",
                local: Some(local_text(local)?),
                remote: remote.as_str(),
                size: stored_size(*size)?,
                folder: false,
            },
            Job::Download {
                remote,
                local,
                size,
            } => Self {
                kind: "download",
                local: Some(local_text(local)?),
                remote: remote.as_str(),
                size: stored_size(*size)?,
                folder: false,
            },
            Job::Delete { remote, folder } => Self {
                kind: "delete",
                local: None,
                remote: remote.as_str(),
                size: 0,
                folder: *folder,
            },
            Job::CreateFolder { remote } => Self {
                kind: "create_folder",
                local: None,
                remote: remote.as_str(),
                size: 0,
                folder: false,
            },
        })
    }
}

/// A path as text, refusing one that is not Unicode (a Windows file name can hold a lone
/// surrogate): saved lossily it would point at a different file on resume.
fn local_text(path: &Path) -> Result<&str, JournalError> {
    path.to_str().ok_or_else(|| {
        JournalError(format!(
            "the path {} is not valid Unicode and can't be saved",
            path.display()
        ))
    })
}

/// The multipart column's text: the progress as JSON, or NULL.
fn multipart_text(mp: Option<&MultipartProgress>) -> Result<Option<String>, JournalError> {
    mp.map(serde_json::to_string)
        .transpose()
        .map_err(|e| JournalError(e.to_string()))
}

fn stored_size(size: u64) -> Result<i64, JournalError> {
    i64::try_from(size)
        .map_err(|_| JournalError(format!("a size of {size} bytes is too large to save")))
}

/// A `journal` row as stored, before its text columns are interpreted.
struct RawJob {
    id: i64,
    batch: String,
    site: String,
    kind: String,
    local: Option<String>,
    remote: String,
    size: i64,
    folder: bool,
    state: String,
    error: Option<String>,
    multipart: Option<String>,
}

impl RawJob {
    const COLUMNS: &'static str =
        "id, batch, site, kind, local, remote, size, folder, state, error, multipart";

    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            batch: row.get(1)?,
            site: row.get(2)?,
            kind: row.get(3)?,
            local: row.get(4)?,
            remote: row.get(5)?,
            size: row.get(6)?,
            folder: row.get(7)?,
            state: row.get(8)?,
            error: row.get(9)?,
            multipart: row.get(10)?,
        })
    }

    /// `None` when the row is not one this build understands (an unknown kind or state, a
    /// remote path that is no longer valid, a missing local path). A multipart column that
    /// can't be read only loses the resume: the job itself is kept.
    fn into_record(self) -> Option<JobRecord> {
        let remote = RemotePath::parse(&self.remote).ok()?;
        let size = u64::try_from(self.size).ok()?;
        let job = match self.kind.as_str() {
            "upload" => Job::Upload {
                local: PathBuf::from(self.local?),
                remote,
                size,
            },
            "download" => Job::Download {
                remote,
                local: PathBuf::from(self.local?),
                size,
            },
            "delete" => Job::Delete {
                remote,
                folder: self.folder,
            },
            "create_folder" => Job::CreateFolder { remote },
            _ => return None,
        };
        Some(JobRecord {
            id: self.id,
            batch: self.batch,
            site: SiteId(self.site),
            job,
            state: state_from_text(&self.state)?,
            error: self.error,
            multipart: self
                .multipart
                .and_then(|text| serde_json::from_str::<MultipartProgress>(&text).ok()),
        })
    }
}

impl Journal for SqliteJournal {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError> {
        let now = stamp(Utc::now());
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let mut ids = Vec::with_capacity(records.len());
        {
            let mut insert = tx
                .prepare_cached(
                    "INSERT INTO journal (batch, site, kind, local, remote, size, folder,
                                          state, error, multipart, updated_at, owner)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                )
                .map_err(db)?;
            for record in records.iter() {
                let cols = JobColumns::of(&record.job)?;
                let multipart = record
                    .multipart
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| JournalError(e.to_string()))?;
                insert
                    .execute(params![
                        record.batch,
                        record.site.0,
                        cols.kind,
                        cols.local,
                        cols.remote,
                        cols.size,
                        cols.folder,
                        state_to_text(record.state),
                        record.error,
                        multipart,
                        now,
                        self.owner,
                    ])
                    .map_err(db)?;
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.commit().map_err(db)?;
        // Only now that every row is safe do the records learn their ids.
        for (record, id) in records.iter_mut().zip(ids) {
            record.id = id;
        }
        Ok(())
    }

    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError> {
        // An id that is no longer there changes nothing, as the contract asks.
        self.lock()
            .prepare_cached(
                "UPDATE journal SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            )
            .and_then(|mut update| {
                update.execute(params![id, state_to_text(state), error, stamp(Utc::now())])
            })
            .map(drop)
            .map_err(db)
    }

    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError> {
        let text = multipart_text(mp)?;
        self.lock()
            .prepare_cached("UPDATE journal SET multipart = ?2, updated_at = ?3 WHERE id = ?1")
            .and_then(|mut update| update.execute(params![id, text, stamp(Utc::now())]))
            .map(drop)
            .map_err(db)
    }

    /// Every update in one transaction: one disk sync for all of them, however many (review
    /// C1: 13,793 cancels one commit at a time took about 9 s at `synchronous=FULL`, more than
    /// closing waits). All of them are written, or none.
    fn apply(&self, updates: &[JournalUpdate]) -> Result<(), JournalError> {
        if updates.is_empty() {
            return Ok(());
        }
        let now = stamp(Utc::now());
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        {
            let mut set_state = tx
                .prepare_cached(
                    "UPDATE journal SET state = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
                )
                .map_err(db)?;
            let mut set_multipart = tx
                .prepare_cached("UPDATE journal SET multipart = ?2, updated_at = ?3 WHERE id = ?1")
                .map_err(db)?;
            for update in updates {
                match update {
                    JournalUpdate::State { id, state, error } => set_state
                        .execute(params![id, state_to_text(*state), error, now])
                        .map(drop),
                    JournalUpdate::Multipart { id, progress } => {
                        let text = multipart_text(progress.as_ref())?;
                        set_multipart.execute(params![id, text, now]).map(drop)
                    }
                }
                .map_err(db)?;
            }
        }
        tx.commit().map_err(db)
    }

    /// The Queued and Failed jobs, in id order, except those of another process still running
    /// (they are its to finish). A row this build can't make sense of is left out: it could
    /// never be resumed, and it must not hide the jobs around it.
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError> {
        let conn = self.lock();
        let others = self.live_others(&conn)?;
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {} FROM journal WHERE state IN (?1, ?2)
                     AND (owner IS NULL OR owner NOT IN (SELECT value FROM json_each(?3)))
                 ORDER BY id",
                RawJob::COLUMNS
            ))
            .map_err(db)?;
        let mut rows = stmt
            .query(params![
                state_to_text(JobState::Queued),
                state_to_text(JobState::Failed),
                others
            ])
            .map_err(db)?;
        let mut records = Vec::new();
        while let Some(row) = rows.next().map_err(db)? {
            match RawJob::read(row) {
                Ok(raw) => records.extend(raw.into_record()),
                Err(e) if is_row_decode_error(&e) => {}
                Err(e) => return Err(db(e)),
            }
        }
        Ok(records)
    }

    fn clear_finished(&self) -> Result<(), JournalError> {
        self.delete_where_state_is(JobState::Done, JobState::Cancelled)
    }

    fn discard_unfinished(&self) -> Result<(), JournalError> {
        self.delete_where_state_is(JobState::Queued, JobState::Failed)
    }
}

impl SqliteJournal {
    /// Deletes the rows in either of two states, but those of another process still running.
    /// `AUTOINCREMENT` keeps their ids from coming back.
    fn delete_where_state_is(&self, a: JobState, b: JobState) -> Result<(), JournalError> {
        let conn = self.lock();
        let others = self.live_others(&conn)?;
        conn.execute(
            "DELETE FROM journal WHERE state IN (?1, ?2)
                 AND (owner IS NULL OR owner NOT IN (SELECT value FROM json_each(?3)))",
            params![state_to_text(a), state_to_text(b), others],
        )
        .map(drop)
        .map_err(db)
    }
}

/// This process as a journal owner: `"<pid>:<creation time>"`, the creation time as the 64-bit
/// FILETIME Windows reports for it. A process id alone could be another process's later; with
/// its start time it names one run. `"<pid>:0"` if Windows won't say when this process started.
pub fn process_owner() -> String {
    use windows::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: the pseudo handle of this process needs no closing.
    let started = creation_time(unsafe { GetCurrentProcess() }).unwrap_or(0);
    format!("{}:{started}", std::process::id())
}

/// Whether the process an owner text ([`process_owner`]) names is still running: a process with
/// that id exists, hasn't exited, and started at that time. Text that isn't an owner, or a
/// process that can't be looked at, counts as gone.
pub fn owner_alive(owner: &str) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let Some((pid, started)) = owner
        .split_once(':')
        .and_then(|(pid, at)| Some((pid.parse::<u32>().ok()?, at.parse::<u64>().ok()?)))
    else {
        return false;
    };
    if pid == std::process::id() {
        return started == process_owner_time();
    }
    // SAFETY: plain call; the handle is closed below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return false;
    };
    let mut code = 0u32;
    // SAFETY: a handle opened above with the right to query it; `code` outlives the call.
    let running =
        unsafe { GetExitCodeProcess(process, &mut code) }.is_ok() && code == STILL_ACTIVE.0 as u32;
    let same = running && creation_time(process) == Some(started);
    // SAFETY: opened above, not used again.
    let _ = unsafe { CloseHandle(process) };
    same
}

/// This process's creation time, as [`process_owner`] writes it.
fn process_owner_time() -> u64 {
    use windows::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: the pseudo handle of this process needs no closing.
    creation_time(unsafe { GetCurrentProcess() }).unwrap_or(0)
}

/// When `process` started, as a 64-bit FILETIME.
fn creation_time(process: windows::Win32::Foundation::HANDLE) -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;

    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: a process handle with query rights; the four out-parameters outlive the call.
    unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use httporter_core::{Job, MultipartProgress, RemotePath, SiteId};

    use super::*;
    use crate::Store;

    fn remote(path: &str) -> RemotePath {
        RemotePath::parse(path).unwrap()
    }

    fn record(job: Job) -> JobRecord {
        JobRecord {
            id: 0,
            batch: "batch-1".into(),
            site: SiteId("site-1".into()),
            job,
            state: JobState::Queued,
            error: None,
            multipart: None,
        }
    }

    fn upload(name: &str) -> JobRecord {
        record(Job::Upload {
            local: PathBuf::from(format!(r"C:\site\{name}")),
            remote: remote(name),
            size: 10,
        })
    }

    fn ids(records: &[JobRecord]) -> Vec<i64> {
        records.iter().map(|r| r.id).collect()
    }

    fn journal() -> SqliteJournal {
        SqliteJournal::open_in_memory().unwrap()
    }

    fn raw(journal: &SqliteJournal, sql: &str) {
        journal.lock().execute(sql, []).unwrap();
    }

    #[test]
    fn insert_assigns_ids_and_unfinished_lists_queued_and_failed() {
        let journal = journal();
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

        // `None` clears an earlier reason.
        journal.set_state(2, JobState::Queued, None).unwrap();
        let unfinished = journal.unfinished().unwrap();
        assert_eq!(unfinished[0].state, JobState::Queued);
        assert_eq!(unfinished[0].error, None);
    }

    #[test]
    fn every_kind_of_job_round_trips() {
        let journal = journal();
        let mut jobs = [
            record(Job::Upload {
                local: PathBuf::from(r"C:\site\naïve #1 😀.html"),
                remote: remote("wp-content/naïve #1 😀.html"),
                size: 12_345,
            }),
            record(Job::Download {
                remote: remote("a/b.txt"),
                local: PathBuf::from(r"D:\Downloads\b.txt"),
                size: u64::from(u32::MAX) + 7,
            }),
            record(Job::Delete {
                remote: remote("old-promo-2025"),
                folder: true,
            }),
            record(Job::Delete {
                remote: remote("a/one.txt"),
                folder: false,
            }),
            record(Job::CreateFolder {
                remote: remote("new/folder"),
            }),
            record(Job::CreateFolder {
                remote: RemotePath::root(),
            }),
            record(Job::Upload {
                local: PathBuf::from(r"C:\empty"),
                remote: remote("empty"),
                size: 0,
            }),
        ];
        jobs[0].batch = "batch-é".into();
        jobs[1].site = SiteId("site-2".into());
        jobs[2].state = JobState::Failed;
        jobs[2].error = Some("Access denied: 'x' \"quoted\"".into());
        journal.insert(&mut jobs).unwrap();
        assert_eq!(ids(&jobs), [1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(journal.unfinished().unwrap(), jobs);
    }

    /// The stored names are part of the file format (Ruling Q20): pinned like core's JSON.
    #[test]
    fn stored_kind_and_state_names_are_pinned() {
        let journal = journal();
        let mut jobs = [
            upload("a"),
            record(Job::Download {
                remote: remote("b"),
                local: PathBuf::from("b"),
                size: 1,
            }),
            record(Job::Delete {
                remote: remote("c"),
                folder: true,
            }),
            record(Job::CreateFolder {
                remote: remote("d"),
            }),
        ];
        journal.insert(&mut jobs).unwrap();
        journal.set_state(1, JobState::Done, None).unwrap();
        journal.set_state(2, JobState::Failed, Some("x")).unwrap();
        journal.set_state(3, JobState::Cancelled, None).unwrap();
        let conn = journal.lock();
        let stored: Vec<(String, String)> = conn
            .prepare("SELECT kind, state FROM journal ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected = [
            ("upload", "done"),
            ("download", "failed"),
            ("delete", "cancelled"),
            ("create_folder", "queued"),
        ];
        assert_eq!(stored, expected.map(|(k, s)| (k.to_owned(), s.to_owned())));
        // The kind column uses the same words as core's `JobKind` JSON.
        for (job, (kind, _)) in jobs.iter().zip(expected) {
            let json = serde_json::to_string(&job.job.kind()).unwrap();
            assert_eq!(json, format!("\"{kind}\""));
        }
    }

    #[test]
    fn clear_finished_keeps_unfinished() {
        let journal = journal();
        let mut records = [upload("a"), upload("b"), upload("c"), upload("d")];
        journal.insert(&mut records).unwrap();
        journal.set_state(1, JobState::Done, None).unwrap();
        journal.set_state(2, JobState::Cancelled, None).unwrap();
        journal.set_state(3, JobState::Failed, Some("x")).unwrap();

        journal.clear_finished().unwrap();
        assert_eq!(ids(&journal.unfinished().unwrap()), [3, 4]);

        journal.discard_unfinished().unwrap();
        assert!(journal.unfinished().unwrap().is_empty());
        let rows: i64 = journal
            .lock()
            .query_row("SELECT count(*) FROM journal", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    /// The journal must survive a power cut (spec S5), so commits wait for the disk.
    #[test]
    fn journal_commits_with_synchronous_full() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SqliteJournal::open(&dir.path().join("httporter.db")).unwrap();
        let level: i64 = journal
            .lock()
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(level, 2, "PRAGMA synchronous: 2 is FULL");
    }

    #[test]
    fn multipart_progress_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        let journal = SqliteJournal::open(&path).unwrap();
        let mut records = [upload("big.zip"), upload("other.zip")];
        journal.insert(&mut records).unwrap();
        let progress = MultipartProgress {
            upload_id: "u-1/é".into(),
            part_size: 16 << 20,
            parts: vec![(1, "\"e1\"".into()), (2, "\"e2\"".into())],
            file: Some(httporter_core::FileStamp {
                len: 70 << 20,
                modified_ns: Some(1_700_000_000_000_000_300),
            }),
        };
        journal.set_multipart(1, Some(&progress)).unwrap();

        // Survives a reopen, and belongs to its own job only.
        drop(journal);
        let journal = SqliteJournal::open(&path).unwrap();
        let unfinished = journal.unfinished().unwrap();
        assert_eq!(unfinished[0].multipart, Some(progress));
        assert_eq!(unfinished[1].multipart, None);

        journal.set_multipart(1, None).unwrap();
        assert_eq!(journal.unfinished().unwrap()[0].multipart, None);
    }

    /// Progress that can't be read only costs the resume: the job itself is still offered.
    #[test]
    fn unreadable_multipart_text_is_forgotten_not_fatal() {
        let journal = journal();
        journal.insert(&mut [upload("a")]).unwrap();
        raw(&journal, "UPDATE journal SET multipart = '{broken'");
        let unfinished = journal.unfinished().unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].multipart, None);
    }

    /// The same promise the in-memory journal makes: a cleared id never comes back.
    #[test]
    fn ids_are_never_reused_after_clear_or_discard() {
        let journal = journal();
        let mut first = [upload("a"), upload("b"), upload("c")];
        journal.insert(&mut first).unwrap();
        journal.set_state(3, JobState::Done, None).unwrap();
        journal.clear_finished().unwrap();
        journal.discard_unfinished().unwrap();
        assert!(journal.unfinished().unwrap().is_empty());

        let mut second = [upload("d"), upload("e")];
        journal.insert(&mut second).unwrap();
        let earlier = first.iter().map(|r| r.id).max().unwrap();
        assert!(second.iter().all(|r| r.id > earlier), "{:?}", ids(&second));
    }

    /// Same, across a restart: the counter is in the file, not in the process.
    #[test]
    fn ids_continue_after_reopen_even_when_everything_was_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        let journal = SqliteJournal::open(&path).unwrap();
        journal
            .insert(&mut [upload("a"), upload("b"), upload("c")])
            .unwrap();
        journal.discard_unfinished().unwrap();
        drop(journal);

        let journal = SqliteJournal::open(&path).unwrap();
        let mut next = [upload("d")];
        journal.insert(&mut next).unwrap();
        assert_eq!(ids(&next), [4]);
    }

    /// The engine may report on a job after its record was cleared away.
    #[test]
    fn updates_to_an_unknown_id_are_ignored() {
        let journal = journal();
        journal.set_state(7, JobState::Done, None).unwrap();
        journal.set_multipart(7, None).unwrap();
        assert!(journal.unfinished().unwrap().is_empty());
        let mut later = [upload("a")];
        journal.insert(&mut later).unwrap();
        assert_eq!(journal.unfinished().unwrap(), later);
    }

    #[test]
    fn a_shared_journal_is_one_journal() {
        let journal: Arc<dyn Journal> = Arc::new(journal());
        let other = journal.clone();
        journal.insert(&mut [upload("a")]).unwrap();
        assert_eq!(other.unfinished().unwrap().len(), 1);
    }

    #[test]
    fn sqlite_journal_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("httporter.db");
        let journal = SqliteJournal::open(&path).unwrap();
        let mut records = [upload("a"), upload("b"), upload("c")];
        journal.insert(&mut records).unwrap();
        journal.set_state(2, JobState::Done, None).unwrap();
        drop(journal);

        let journal = SqliteJournal::open(&path).unwrap();
        let unfinished = journal.unfinished().unwrap();
        assert_eq!(unfinished.len(), 2);
        assert_eq!(unfinished, [records[0].clone(), records[2].clone()]);
    }

    #[test]
    fn journal_insert_10k_in_one_transaction_is_fast() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SqliteJournal::open(&dir.path().join("httporter.db")).unwrap();
        let mut records: Vec<JobRecord> = (0..10_000).map(|i| upload(&format!("f{i}"))).collect();

        let started = Instant::now();
        journal.insert(&mut records).unwrap();
        let took = started.elapsed();

        assert!(took.as_secs_f64() < 2.0, "10k inserts took {took:?}");
        assert_eq!(records[0].id, 1);
        assert_eq!(records[9_999].id, 10_000);
        assert_eq!(journal.unfinished().unwrap().len(), 10_000);
    }

    /// Review C1: a batch of updates is one transaction on the file, so 10,000 cancels cost one
    /// disk sync, not 10,000; and they land as the single calls would.
    #[test]
    fn apply_writes_10k_updates_in_one_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SqliteJournal::open(&dir.path().join("httporter.db")).unwrap();
        let mut records: Vec<JobRecord> = (0..10_000).map(|i| upload(&format!("f{i}"))).collect();
        journal.insert(&mut records).unwrap();
        let progress = MultipartProgress {
            upload_id: "u-2".into(),
            part_size: 5 << 20,
            parts: vec![(1, "\"e1\"".into())],
            file: None,
        };
        let mut updates: Vec<JournalUpdate> = records
            .iter()
            .map(|r| JournalUpdate::State {
                id: r.id,
                state: JobState::Cancelled,
                error: None,
            })
            .collect();
        updates[1] = JournalUpdate::State {
            id: 2,
            state: JobState::Failed,
            error: Some("No connection".into()),
        };
        updates.push(JournalUpdate::Multipart {
            id: 2,
            progress: Some(progress.clone()),
        });

        let started = Instant::now();
        journal.apply(&updates).unwrap();
        let took = started.elapsed();
        assert!(took.as_secs_f64() < 2.0, "10k updates took {took:?}");

        let unfinished = journal.unfinished().unwrap();
        assert_eq!(ids(&unfinished), [2]);
        assert_eq!(unfinished[0].error.as_deref(), Some("No connection"));
        assert_eq!(unfinished[0].multipart, Some(progress));
        journal.apply(&[]).unwrap();
    }

    /// A batch that can't be written leaves every record as it was.
    #[test]
    fn a_failed_apply_writes_nothing() {
        let journal = journal();
        journal.insert(&mut [upload("a"), upload("b")]).unwrap();
        raw(
            &journal,
            "CREATE TRIGGER refuse BEFORE UPDATE ON journal WHEN NEW.id = 2
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
        );
        let cancel = |id| JournalUpdate::State {
            id,
            state: JobState::Cancelled,
            error: None,
        };
        assert!(journal.apply(&[cancel(1), cancel(2)]).is_err());
        assert_eq!(ids(&journal.unfinished().unwrap()), [1, 2]);
    }

    /// One failing record fails the whole insert: nothing is written and no id is handed out.
    #[test]
    fn a_failed_insert_writes_nothing_and_assigns_no_ids() {
        let journal = journal();
        let mut records = [
            upload("fine"),
            // Past what SQLite's integers can hold.
            record(Job::Upload {
                local: PathBuf::from("big"),
                remote: remote("big"),
                size: u64::MAX,
            }),
        ];
        assert!(journal.insert(&mut records).is_err());
        assert_eq!(ids(&records), [0, 0]);
        assert!(journal.unfinished().unwrap().is_empty());

        // The failed attempt used no ids a later batch could notice.
        let mut next = [upload("later")];
        journal.insert(&mut next).unwrap();
        assert_eq!(ids(&next), [1]);
    }

    /// A Windows file name can hold a lone surrogate, which is not text. It can't be stored
    /// faithfully, so the insert says so rather than saving a path that points elsewhere.
    #[cfg(windows)]
    #[test]
    fn a_local_path_that_is_not_unicode_is_refused() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        let journal = journal();
        let odd = PathBuf::from(OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800]));
        let mut records = [record(Job::Upload {
            local: odd,
            remote: remote("odd"),
            size: 1,
        })];
        assert!(journal.insert(&mut records).is_err());
        assert!(journal.unfinished().unwrap().is_empty());
    }

    /// A row this build can't read is left out, not a reason to lose every other job.
    #[test]
    fn an_undecodable_row_is_skipped() {
        let journal = journal();
        let mut records = [
            upload("a"),
            upload("b"),
            upload("c"),
            upload("d"),
            upload("e"),
        ];
        journal.insert(&mut records).unwrap();
        raw(
            &journal,
            "UPDATE journal SET kind = 'teleport' WHERE id = 1",
        );
        raw(&journal, "UPDATE journal SET state = 'limbo' WHERE id = 2");
        raw(
            &journal,
            "UPDATE journal SET remote = 'a/../b' WHERE id = 3",
        );
        raw(&journal, "UPDATE journal SET local = NULL WHERE id = 4");
        raw(&journal, "UPDATE journal SET size = x'00ff' WHERE id = 5");
        assert!(journal.unfinished().unwrap().is_empty());

        let mut fine = [upload("f")];
        journal.insert(&mut fine).unwrap();
        assert_eq!(journal.unfinished().unwrap(), fine);
    }

    #[test]
    fn opens_with_wal_and_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let journal = SqliteJournal::open(&dir.path().join("httporter.db")).unwrap();
        let conn = journal.lock();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let timeout: i64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    /// The store and the journal are separate connections to one file: each sees the other's
    /// committed work, and neither locks the other out.
    #[test]
    fn store_and_journal_share_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        let store = Store::open(&path).unwrap();
        let journal = SqliteJournal::open(&path).unwrap();

        journal.insert(&mut [upload("a"), upload("b")]).unwrap();
        store.put_settings("k", &7_u32).unwrap();
        journal.insert(&mut [upload("c")]).unwrap();
        assert_eq!(store.settings::<u32>("k"), 7);
        assert_eq!(journal.unfinished().unwrap().len(), 3);

        // And what the journal wrote is still there for a store opened later.
        drop((store, journal));
        let store = Store::open(&path).unwrap();
        assert_eq!(store.settings::<u32>("k"), 7);
        let again = SqliteJournal::open(&path).unwrap();
        assert_eq!(again.unfinished().unwrap().len(), 3);
    }
}
