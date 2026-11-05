//! The opt-in update check (§16): the release feed of `RELEASE_REPO` on GitHub, read at start
//! (at most once in 24 h) or on Check Now. No silent install, no telemetry.
#![allow(dead_code)] // T4 calls these.

/// The release repository (`owner/repo`) this build checks; None turns the check off.
pub const RELEASE_REPO: Option<&str> = option_env!("HTTPORTER_RELEASE_REPO");

/// The newest release: its version (a leading `v` stripped), its page and the start of its
/// notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
}

/// `GET https://api.github.com/repos/{repo}/releases/latest`; the error is for the activity log.
pub async fn latest(client: &reqwest::Client, repo: &str) -> Result<Release, String> {
    let _ = (client, repo);
    todo!("T4")
}

/// Whether `candidate` is a newer version than `current` (a pre-release never is).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let _ = (candidate, current);
    todo!("T4")
}
