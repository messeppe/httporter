//! Skip rules: the files a publish or a compare leaves out.
//!
//! Paths are relative, with '/' between segments, and matched ignoring case. A rule ending in '/'
//! (`x/`) is a folder of that name anywhere, and everything below it; a rule without '/' matches
//! the last segment; a rule with an inner '/' matches the whole relative path. '*' is a run of
//! characters without '/', '?' one character.

// T1 skeleton: drop this once T2 fills the bodies.
#![allow(unused_variables)]

/// The rules a new profile starts with (spec 8.6.11).
pub const DEFAULT_SKIP_RULES: &[&str] = &[
    ".env",
    ".env.*",
    "*.key",
    "*.pem",
    ".git/",
    ".svn/",
    "node_modules/",
    "Thumbs.db",
    "desktop.ini",
    ".DS_Store",
    "*.tmp",
    "~$*",
];

/// A set of skip rules, compiled. The default skips nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkipRules {}

impl SkipRules {
    pub fn new(patterns: &[String]) -> Self {
        todo!("T2")
    }

    /// [`DEFAULT_SKIP_RULES`], as a profile keeps them.
    pub fn defaults() -> Vec<String> {
        DEFAULT_SKIP_RULES.iter().map(|r| (*r).to_owned()).collect()
    }

    /// Whether `rel` (a relative '/' path; `is_folder` for a folder) is left out.
    pub fn is_skipped(&self, rel: &str, is_folder: bool) -> bool {
        todo!("T2")
    }
}

/// Whether `rel` looks like a file that holds a secret: `.env*`, `*.pem`, `*.key`, `id_rsa*`,
/// `*.p12`, `wp-config.php` (spec 8.6.12).
pub fn looks_secret(rel: &str) -> bool {
    todo!("T2")
}
