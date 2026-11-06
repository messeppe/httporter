// SPDX-License-Identifier: MIT OR Apache-2.0

//! The activity log's events and their filter.

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
    /// Whether `e` passes the filter: its kind, failed (with `failed_only`), its site, and `text`
    /// (any case) in its path, detail or site.
    pub fn matches(&self, e: &ActivityEvent) -> bool {
        if self.kind.is_some_and(|kind| kind != e.kind) {
            return false;
        }
        if self.failed_only && e.ok {
            return false;
        }
        if self.site.as_ref().is_some_and(|site| *site != e.site) {
            return false;
        }
        let text = self.text.trim();
        if text.is_empty() {
            return true;
        }
        let text = text.to_lowercase();
        [&e.path, &e.detail, &e.site]
            .iter()
            .any(|field| field.to_lowercase().contains(&text))
    }
}

/// Keys whose value runs to the end of the line (a header that can hold several parts).
const LINE_KEYS: &[&str] = &["authorization"];
/// Keys whose value is one token.
const TOKEN_KEYS: &[&str] = &[
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "accesskey",
    "password=",
    "key=",
    "token=",
    "secret=",
];
/// What replaces a masked value.
const MASK: &str = "***";

/// `text` with the values after Authorization, X-Amz-Signature/-Credential/-Security-Token,
/// AccessKey, password=, key=, token=, secret= (any case) masked.
pub fn redact(text: &str) -> String {
    // ASCII lower-casing keeps every byte where it was, so positions carry over to `text`.
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (keys, to_line_end) in [(LINE_KEYS, true), (TOKEN_KEYS, false)] {
        for key in keys {
            let mut from = 0;
            while let Some(found) = lower[from..].find(key) {
                let after = from + found + key.len();
                from = after;
                let start = skip_separators(bytes, after);
                let end = value_end(bytes, start, to_line_end);
                if end > start {
                    spans.push((start, end));
                }
            }
        }
    }
    if spans.is_empty() {
        return text.to_owned();
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end) in spans {
        if end <= at {
            continue;
        }
        let start = start.max(at);
        out.push_str(&text[at..start]);
        out.push_str(MASK);
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// The first byte at or after `at` that is not a separator between a key and its value.
fn skip_separators(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && matches!(bytes[at], b':' | b'=' | b' ' | b'\t' | b'"' | b'\'') {
        at += 1;
    }
    at
}

/// Where the value that starts at `start` ends: at the end of the line, or (unless `to_line_end`)
/// at the first character that ends a token.
fn value_end(bytes: &[u8], start: usize, to_line_end: bool) -> usize {
    let mut end = start;
    while end < bytes.len() {
        let b = bytes[end];
        let stop = if to_line_end {
            matches!(b, b'\n' | b'\r')
        } else {
            matches!(
                b,
                b'\n' | b'\r' | b' ' | b'\t' | b'&' | b'"' | b'\'' | b',' | b';' | b'<' | b'>'
            )
        };
        if stop {
            break;
        }
        end += 1;
    }
    end
}
