//! The activity log's events and their filter (T4).
#![allow(unused_variables, dead_code)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What an event of the activity log is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Upload,
    Download,
    Delete,
    Move,
    Publish,
    Purge,
    Edit,
    Site,
}

/// One event of the activity log, kept as one JSON line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub at: DateTime<Utc>,
    pub kind: ActivityKind,
    pub site: String,
    pub path: String,
    pub ok: bool,
    pub detail: String,
    pub bytes: u64,
}

/// Which events the activity sheet shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityFilter {
    pub kind: Option<ActivityKind>,
    pub failed_only: bool,
    pub site: Option<String>,
    pub text: String,
}

impl ActivityFilter {
    /// Whether `e` passes the filter.
    pub fn matches(&self, e: &ActivityEvent) -> bool {
        todo!("T4")
    }
}

/// `text` with the values after Authorization, X-Amz-Signature/-Credential/-Security-Token,
/// AccessKey, password=, key=, token=, secret= (any case) masked.
pub fn redact(text: &str) -> String {
    todo!("T4")
}
