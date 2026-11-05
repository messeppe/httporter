//! The words of Settings › Security, Reset, the update check and About (I5, T4): everything they
//! show that Rust builds.

use std::fmt::Display;

// ---- Security ----

/// Credential Manager could not be listed.
pub fn keys_unreadable(e: impl Display) -> String {
    format!("Couldn't read Windows Credential Manager: {e}")
}

/// Unused keys are not looked for while the sites or accounts may not all be known.
pub const KEYS_UNSURE: &str =
    "Unused keys can't be found while HTTPorter can't read all of its saved sites.";

/// Remove Unused Keys did its work.
pub fn keys_removed(n: usize) -> String {
    match n {
        1 => "Removed 1 unused key.".to_owned(),
        n => format!("Removed {n} unused keys."),
    }
}

/// Remove Unused Keys stopped at a key it could not remove.
pub fn keys_not_removed(removed: usize, e: impl Display) -> String {
    format!("Removed {removed}, then couldn't remove an unused key: {e}")
}

/// Open Credential Manager failed.
pub fn credential_manager_failed(e: impl Display) -> String {
    format!("Couldn't open Credential Manager: {e}")
}

// ---- Reset ----

/// Why Reset HTTPorter… is off.
pub const RESET_BUSY: &str = "Finish or cancel the transfers first.";

/// The marker of the reset could not be written.
pub fn reset_not_prepared(e: impl Display) -> String {
    format!("Couldn't prepare the reset: {e}")
}

/// The reset is set up, but HTTPorter could not start itself again.
pub fn restart_failed(e: impl Display) -> String {
    format!("Couldn't start HTTPorter again ({e}). It resets the next time it starts.")
}

/// The reset at start stopped part-way; it is tried again at the next start.
pub fn reset_failed(e: impl Display) -> String {
    format!("The reset didn't finish: {e}. HTTPorter tries again the next time it starts.")
}

/// What the reset at start says when the profile folders are not where they should be.
pub const RESET_NO_PROFILE: &str = "Windows didn't say where HTTPorter's folders are";

/// The reset found a name it may not delete in its folders.
pub fn reset_entry_failed(path: &std::path::Path, e: impl Display) -> String {
    format!("{}: {e}", path.display())
}

// ---- Updates ----

/// The update rows of a build made without a release address.
pub const NO_RELEASE_ADDRESS: &str = "This build has no release address.";

/// Check Now was pressed and the answer is not in yet.
pub const UPDATE_CHECKING: &str = "Checking…";

/// The toast for a newer release.
pub fn update_available(version: &str) -> String {
    format!("HTTPorter {version} is available")
}

/// The toast's button that opens the release page.
pub const VIEW_RELEASE: &str = "View Release";

/// The note under Check Now for a newer release: its version, then its notes.
pub fn update_note(version: &str, notes: &str) -> String {
    let notes = notes.trim();
    if notes.is_empty() {
        format!("HTTPorter {version} is available.")
    } else {
        format!("HTTPorter {version} is available.\n{notes}")
    }
}

/// The note under Check Now when this is the newest release.
pub fn up_to_date(version: &str) -> String {
    format!("HTTPorter {version} is the newest version.")
}

/// The note under Check Now when the check failed.
pub fn update_failed(e: &str) -> String {
    format!("Couldn't check for updates: {e}.")
}

/// The activity log's line for a failed check.
pub fn update_failed_log(e: &str) -> String {
    format!("Update check failed: {e}")
}

/// The release feed answered with an error status.
pub fn update_status(code: u16) -> String {
    match code {
        404 => "the release feed has no release yet (404)".to_owned(),
        403 | 429 => format!("the release feed asked to wait ({code})"),
        code => format!("the release feed answered {code}"),
    }
}

/// The release feed's answer was not a release.
pub const UPDATE_MALFORMED: &str = "the release feed's answer was not understood";

/// The release feed's answer was larger than any release.
pub const UPDATE_TOO_BIG: &str = "the release feed's answer was too large";

/// No answer in time.
pub const UPDATE_TIMEOUT: &str = "the release feed didn't answer within 5 seconds";

/// The release feed could not be reached.
pub const UPDATE_OFFLINE: &str = "the release feed couldn't be reached";

/// The repository name built in is not `owner/repo`.
pub const UPDATE_BAD_REPO: &str = "this build's release address is not valid";

/// The custom proxy of Settings › Transfers is not a URL.
pub const UPDATE_BAD_PROXY: &str = "the custom proxy address is not a valid URL";

/// Any other failure of the request, by its reason.
pub fn update_request_failed(e: impl Display) -> String {
    format!("the release feed couldn't be read ({e})")
}

// ---- Palette ----

pub const PALETTE_ABOUT: &str = "About HTTPorter";
pub const PALETTE_ABOUT_NOTE: &str = "Version, licenses and credits in Settings › General";
pub const PALETTE_CHECK_UPDATES: &str = "Check for Updates";
pub const PALETTE_CHECK_UPDATES_NOTE: &str = "Asks the release feed for a newer HTTPorter";
pub const PALETTE_SETTINGS_OFF: &str = "Arrives in a later version";
