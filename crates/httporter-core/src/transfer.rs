//! What the transfer engine works on and what it tells the UI: jobs, the engine's state and
//! progress, conflicts and delete summaries. Plain data, so the UI can be tested without an
//! engine. Every stored enum is written in snake_case, so renaming a Rust variant can't break a
//! journal already written.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::RemotePath;

/// Identifies one job in the engine, the journal and the drawer's rows alike: it is the job's
/// journal record id (`JobRecord.id as u64`), at least 1, assigned by `Journal::insert`. The
/// default, 0, is no job's id: it only fills a [`JobView`] built in a test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// One thing to do on one site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Job {
    Upload {
        local: PathBuf,
        remote: RemotePath,
        size: u64,
    },
    Download {
        remote: RemotePath,
        local: PathBuf,
        size: u64,
    },
    /// Deletes a file, or a folder and everything in it.
    Delete {
        remote: RemotePath,
        folder: bool,
    },
    CreateFolder {
        remote: RemotePath,
    },
}

impl Job {
    pub fn kind(&self) -> JobKind {
        match self {
            Self::Upload { .. } => JobKind::Upload,
            Self::Download { .. } => JobKind::Download,
            Self::Delete { .. } => JobKind::Delete,
            Self::CreateFolder { .. } => JobKind::CreateFolder,
        }
    }
}

/// Which kind of [`Job`] a job is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Also the default, which only fills a [`JobView`] built in a test.
    #[default]
    Upload,
    Download,
    Delete,
    CreateFolder,
}

/// Which way the jobs running (or last run) go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Also what an engine that has run nothing yet says.
    #[default]
    Upload,
    Download,
    Delete,
    /// More than one of the above.
    Mixed,
}

/// What the engine as a whole is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EngineState {
    #[default]
    Idle,
    Running,
    /// The user paused everything.
    Paused,
    /// The connection dropped: everything waits for it to come back.
    Offline {
        site: String,
    },
    /// `site` refused its credentials: its transfers wait until the site is edited.
    AuthStopped {
        site: String,
    },
    /// `drive` is full: downloads need `needed` more bytes on it.
    DiskFull {
        drive: String,
        needed: u64,
    },
}

/// One row of the transfer drawer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobView {
    pub id: JobId,
    pub kind: JobKind,
    /// The remote path, as text.
    pub path: String,
    pub size: u64,
    pub done_bytes: u64,
    /// Why the job failed, in plain words.
    pub error: Option<String>,
    /// A queued job held back with `Engine::pause` until it is resumed.
    pub paused: bool,
}

/// The batch that finished last, for the collapsed drawer's "Last transfer …".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSummary {
    pub finished_at: DateTime<Utc>,
    pub files: u64,
    pub bytes: u64,
    pub direction: Direction,
    pub site: String,
}

/// Everything the transfer drawer shows, at one moment. The lists are windows onto the engine's
/// jobs, so a snapshot stays small however many jobs there are.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineSnapshot {
    pub state: EngineState,
    pub direction: Direction,
    /// The site name and host the jobs go to.
    pub site: String,
    pub host: String,
    pub files_total: u64,
    pub files_done: u64,
    pub files_failed: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub files_per_s: f64,
    pub bytes_per_s: f64,
    pub eta: Option<Duration>,
    /// The first 200 active jobs; `active_count` counts them all.
    pub active: Vec<JobView>,
    pub active_count: u64,
    /// The first 200 queued jobs; `queued_count` counts them all.
    pub queued: Vec<JobView>,
    pub queued_count: u64,
    pub failed: Vec<JobView>,
    /// The last 200 finished jobs; `done_count` counts them all.
    pub done: Vec<JobView>,
    pub done_count: u64,
    pub last_batch: Option<BatchSummary>,
}

/// How a conflict told whether two files have the same contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareBy {
    Sha256,
    Md5,
    /// Neither side had a digest to compare.
    Size,
}

/// What to do about one conflict, once chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resolution {
    Replace,
    ReplaceIfDifferent,
    Skip,
    KeepBoth,
}

/// A file that already exists where a transfer would put it: one row of the conflict sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The file's place among the batch's conflicts.
    pub index: usize,
    pub name: String,
    pub local: PathBuf,
    pub remote: RemotePath,
    pub local_size: u64,
    pub local_modified: Option<DateTime<Utc>>,
    pub remote_size: u64,
    pub remote_modified: Option<DateTime<Utc>>,
    /// Both sides hold the same contents; `None` when that could not be told.
    pub same: Option<bool>,
    pub compared_by: CompareBy,
}

/// What a delete would remove, for the delete alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteSummary {
    /// The folders and files selected.
    pub folders: u64,
    pub files: u64,
    /// Every file that goes, including those inside the folders, and their size.
    pub total_files: u64,
    pub total_bytes: u64,
    /// The name of the one item selected, and whether it is a folder.
    pub single: Option<(String, bool)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(path: &str) -> RemotePath {
        RemotePath::parse(path).unwrap()
    }

    #[test]
    fn job_kind_matches_the_variant() {
        let jobs = [
            (
                Job::Upload {
                    local: PathBuf::from(r"C:\site\index.html"),
                    remote: remote("index.html"),
                    size: 10,
                },
                JobKind::Upload,
            ),
            (
                Job::Download {
                    remote: remote("a/b.txt"),
                    local: PathBuf::from(r"D:\b.txt"),
                    size: 3,
                },
                JobKind::Download,
            ),
            (
                Job::Delete {
                    remote: remote("old"),
                    folder: true,
                },
                JobKind::Delete,
            ),
            (
                Job::CreateFolder {
                    remote: remote("new"),
                },
                JobKind::CreateFolder,
            ),
        ];
        for (job, kind) in jobs {
            assert_eq!(job.kind(), kind, "{job:?}");
        }
    }

    #[test]
    fn jobs_round_trip_json() {
        let jobs = [
            Job::Upload {
                local: PathBuf::from(r"C:\site\naïve #1.html"),
                remote: remote("wp-content/naïve #1.html"),
                size: 12_345,
            },
            Job::Delete {
                remote: remote("old-promo-2025"),
                folder: true,
            },
        ];
        for job in jobs {
            let json = serde_json::to_string(&job).unwrap();
            assert_eq!(serde_json::from_str::<Job>(&json).unwrap(), job, "{json}");
        }
    }

    #[test]
    fn a_stored_job_with_a_bad_remote_path_is_refused() {
        let good = r#"{"create_folder":{"remote":"a/b"}}"#;
        assert!(serde_json::from_str::<Job>(good).is_ok());
        let bad = r#"{"create_folder":{"remote":"a/../b"}}"#;
        assert!(serde_json::from_str::<Job>(bad).is_err());
    }

    /// Ruling Q20: the stored names are pinned, so renaming a Rust variant can't break a
    /// journal already written.
    #[test]
    fn stored_names_are_snake_case() {
        let json = |job: Job| serde_json::to_string(&job).unwrap();
        assert_eq!(
            json(Job::Upload {
                local: PathBuf::from(r"C:\site\index.html"),
                remote: remote("index.html"),
                size: 10,
            }),
            r#"{"upload":{"local":"C:\\site\\index.html","remote":"index.html","size":10}}"#
        );
        assert_eq!(
            json(Job::Download {
                remote: remote("a/b.txt"),
                local: PathBuf::from("b.txt"),
                size: 3,
            }),
            r#"{"download":{"remote":"a/b.txt","local":"b.txt","size":3}}"#
        );
        assert_eq!(
            json(Job::Delete {
                remote: remote("old"),
                folder: true,
            }),
            r#"{"delete":{"remote":"old","folder":true}}"#
        );
        assert_eq!(
            json(Job::CreateFolder {
                remote: remote("new"),
            }),
            r#"{"create_folder":{"remote":"new"}}"#
        );
        let kinds = [
            (JobKind::Upload, r#""upload""#),
            (JobKind::Download, r#""download""#),
            (JobKind::Delete, r#""delete""#),
            (JobKind::CreateFolder, r#""create_folder""#),
        ];
        for (kind, text) in kinds {
            assert_eq!(serde_json::to_string(&kind).unwrap(), text);
        }
        let directions = [
            (Direction::Upload, r#""upload""#),
            (Direction::Download, r#""download""#),
            (Direction::Delete, r#""delete""#),
            (Direction::Mixed, r#""mixed""#),
        ];
        for (direction, text) in directions {
            assert_eq!(serde_json::to_string(&direction).unwrap(), text);
        }
    }

    #[test]
    fn an_idle_engine_snapshot_is_empty() {
        let s = EngineSnapshot::default();
        assert_eq!(s.state, EngineState::Idle);
        assert_eq!((s.files_total, s.bytes_total, s.queued_count), (0, 0, 0));
        assert!(s.active.is_empty() && s.last_batch.is_none() && s.eta.is_none());
    }
}
