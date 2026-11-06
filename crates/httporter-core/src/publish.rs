// SPDX-License-Identifier: MIT OR Apache-2.0

//! Publish profiles and their history: what a profile publishes where, how one run went, and the
//! delete guard that asks before a publish removes many files.

use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{RemotePath, SiteId};

/// Stable identifier of a publish profile (a random UUID, as text).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub String);

impl ProfileId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a publish purges from the CDN once its files are up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeMode {
    Off,
    /// The URLs of the files the publish uploaded or deleted.
    #[default]
    Changed,
    /// Every pull zone in front of the storage zone, whole.
    Zone,
}

/// A publish that would delete more than this asks first (Review Deletions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeleteGuard {
    pub files: u64,
    pub percent: u8,
}

impl Default for DeleteGuard {
    fn default() -> Self {
        Self {
            files: 100,
            percent: 10,
        }
    }
}

impl DeleteGuard {
    /// Whether `deletes` out of `remote_files` is too many: more than
    /// `min(files, remote_files * percent / 100)`.
    pub fn trips(&self, deletes: u64, remote_files: u64) -> bool {
        let share = remote_files.saturating_mul(u64::from(self.percent)) / 100;
        deletes > self.files.min(share)
    }
}

/// A saved publish profile: a local folder published to a folder of a site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRecord {
    pub id: ProfileId,
    pub site: SiteId,
    pub name: String,
    pub local_dir: PathBuf,
    pub remote_dir: RemotePath,
    pub skip_rules: Vec<String>,
    /// Delete files on the site that are not in the folder. Off by default.
    pub remove_extras: bool,
    pub purge: PurgeMode,
    pub guard: DeleteGuard,
    pub created_at: DateTime<Utc>,
    pub last_published_at: Option<DateTime<Utc>>,
    /// The `Cache-Control` the profile's uploads are sent with (S3), if any.
    pub cache_control: Option<String>,
    /// I6: watching, unattended deletes and the schedule (stored as JSON, `profiles.extras`).
    pub extras: ProfileExtras,
}

/// The I6 options of a profile, kept together as one JSON value so that a later option needs no
/// new column. Every field has a default, so an older or partial value still reads.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileExtras {
    /// Publish changes automatically while HTTPorter is open (watch folder, §8.8.3).
    pub watch: bool,
    /// A watched or scheduled publish may delete without a review. Off by default.
    pub unattended_deletes: bool,
    /// The scheduled publish (§8.8.2), if one was ever set up (`enabled` says whether it runs).
    pub schedule: Option<Schedule>,
}

/// How often a scheduled publish repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleRepeat {
    /// Every `every` hours (1..=24), from `at_minutes` each day.
    Hourly {
        every: u8,
    },
    Daily,
    /// On the days marked, Monday first.
    Weekly {
        days: [bool; 7],
    },
}

/// A scheduled publish: a Windows Task Scheduler task runs the CLI for the profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub enabled: bool,
    pub repeat: ScheduleRepeat,
    /// Local time of day, in minutes after midnight (0..1440).
    pub at_minutes: u16,
    /// What the scheduled publish purges; `None` = what the profile says.
    pub purge: Option<PurgeMode>,
}

/// Who started a publish (its history row says so).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
    /// The Publish sheet in the window.
    #[default]
    App,
    /// `httporter publish` in a terminal.
    Cli,
    /// The profile's scheduled task (`--scheduled`).
    Scheduled,
    /// The watch folder.
    Watch,
}

impl HistorySource {
    /// The stored form (`history.source`): `app`, `cli`, `scheduled` or `watch`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Cli => "cli",
            Self::Scheduled => "scheduled",
            Self::Watch => "watch",
        }
    }

    /// The stored form back; anything unknown reads as [`HistorySource::App`].
    pub fn parse(text: &str) -> Self {
        match text {
            "cli" => Self::Cli,
            "scheduled" => Self::Scheduled,
            "watch" => Self::Watch,
            _ => Self::App,
        }
    }
}

/// What one headless publish should do (the CLI's flags map onto it; the scheduler and the watch
/// folder fill it in themselves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessOptions {
    /// Compare and plan only; change nothing (`--dry-run`).
    pub dry_run: bool,
    /// Past the delete guard without a review (`--yes`).
    pub yes: bool,
    /// Upload only; delete nothing (`--no-delete`).
    pub no_delete: bool,
    /// The purge for this run; `None` = the profile's (`--purge changed|all|none`).
    pub purge: Option<PurgeMode>,
    pub source: HistorySource,
}

/// How a headless publish ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessOutcome {
    Done {
        uploaded: u64,
        deleted: u64,
        failed: u64,
        bytes: u64,
        purge: PurgeResult,
    },
    /// What a run would do (`--dry-run`).
    DryRun {
        new: u64,
        changed: u64,
        delete: u64,
        bytes: u64,
    },
    /// The delete guard tripped: the uploads ran, the deletes were held back.
    GuardTripped { would_delete: u64, uploaded: u64 },
    /// Another publish of this profile holds its lock.
    Busy,
}

/// The CLI's exit codes (§8.8.1), a stable promise from the release they first ship in.
pub mod exit {
    /// Everything went.
    pub const OK: i32 = 0;
    /// Some transfers failed.
    pub const FAILURES: i32 = 1;
    /// A configuration problem: an unknown or ambiguous site or profile, bad arguments.
    pub const CONFIG: i32 = 2;
    /// The site's credentials are missing or wrong.
    pub const AUTH: i32 = 3;
    /// The delete guard tripped; the deletes need `--yes`.
    pub const GUARD: i32 = 4;
}

/// How a file on one side compares with the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompareStatus {
    /// Only in the local folder.
    New,
    /// On both sides, with other contents.
    Changed,
    /// Only on the site.
    RemoteOnly,
    Same,
}

/// How a publish ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishResult {
    Ok,
    Partial,
    Failed,
    Cancelled,
}

/// What a publish did to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    Upload,
    Delete,
}

/// How that went.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOutcome {
    Done,
    Failed(String),
    Cancelled,
    /// Not tried, and why.
    NotRun(String),
}

/// One file of a publish, in its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFile {
    pub path: RemotePath,
    pub action: FileAction,
    pub outcome: FileOutcome,
    pub size: u64,
}

/// What the CDN purge after a publish did.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurgeResult {
    Off,
    /// This many URLs purged.
    Urls(u64),
    Zone,
    Failed(String),
    /// No purge could be tried (no pull zone, no key, an S3 site), and why.
    Unavailable(String),
}

/// One publish of a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRecord {
    /// 0 until stored.
    pub id: i64,
    pub profile: ProfileId,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub result: PublishResult,
    pub uploaded: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub failed: u64,
    pub bytes: u64,
    pub purge: PurgeResult,
    pub verified: bool,
    /// What the site held after the publish (the profile page's "On Bunny now" card).
    pub remote_files: u64,
    pub remote_folders: u64,
    pub regions: u32,
    /// Empty from `list_history`.
    pub files: Vec<HistoryFile>,
    /// Who started it (I6).
    pub source: HistorySource,
}

/// How many publishes of a profile the history keeps.
pub const HISTORY_KEEP: usize = 200;
