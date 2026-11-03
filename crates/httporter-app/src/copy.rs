//! The words the app says, when Rust is the one saying them.
//!
//! Every user-facing string that Rust builds lives here: the item counts, the plain-words version
//! of a failed request, the Add Site form's field errors, the status line for a drop, what the
//! window shows while a site connects, and what a failed start says. Strings in the `.slint`
//! files go through `@tr()` instead; Rust cannot, so they are kept in one module where they can
//! be read, reviewed and later translated together.
//!
//! Nothing here touches a window or the session, so each text is a plain function of its inputs.
//! `lint_tests.rs` keeps it that way: a scan fails the build of the tests if a string literal or a
//! `format!` ends up feeding a `set_*` call anywhere else.
//!
//! No text here is built from a password. Errors that come from outside are shown only when their
//! own text is safe to show, and the error types of the credential store and the storage clients
//! never carry the secret.

use std::error::Error;
use std::fmt::Display;
use std::path::Path;
use std::time::Duration;

use httporter_core::{ProviderError, RemotePath};

use crate::drop_hook::{DropBatch, PaneSide};

// ---- Counts ----

/// "1 item", "2 items".
pub fn items(n: usize) -> String {
    if n == 1 {
        "1 item".to_owned()
    } else {
        format!("{n} items")
    }
}

/// The status line for a local listing: "{n} items", and " · {m} couldn't be read" when some
/// entries were skipped.
pub fn items_status(n: usize, skipped: usize) -> String {
    if skipped == 0 {
        items(n)
    } else {
        format!("{} · {skipped} couldn't be read", items(n))
    }
}

// ---- A request that failed ----

/// What a failed attempt to talk to a zone was for, which decides how the failure is told.
#[derive(Clone, Copy, Debug)]
pub enum Attempt<'a> {
    /// Making the storage client, before anything was sent.
    Setup,
    /// Listing a folder.
    List(&'a RemotePath),
}

/// The most of a client's own explanation that goes into a message.
const DETAIL_MAX: usize = 120;

/// What to say when the PC's clock is too far off for a request to be signed.
const CLOCK_OFF: &str =
    "Your PC's clock is off. Turn on “Set time automatically” in Windows Settings.";

/// What to tell the user about a failed attempt to talk to `zone` on `host`: plain words for each
/// kind of error, without the labels the errors carry for the code ("network error: …", "server
/// error (HTTP 503): …"), and never the password or a request URL. Only a client that could not
/// be set up adds its own explanation, cut short: no request was made, so there is nothing else
/// to go on.
pub fn describe(error: &ProviderError, zone: &str, host: &str, attempt: Attempt<'_>) -> String {
    match (error, attempt) {
        // Bunny never says which part was wrong; S3's hints get their own words later.
        (ProviderError::Auth { .. }, _) => {
            format!("Couldn't sign in to {zone}. Check the password, zone name and region.")
        }
        (ProviderError::NotFound, Attempt::List(dir)) => {
            format!("Couldn't find /{} in {zone}.", dir.as_str())
        }
        (ProviderError::NotFound, Attempt::Setup) => format!("Couldn't find {zone}."),
        (ProviderError::RateLimited { .. }, _) => format!("{host} is busy. Try again in a moment."),
        // No answer came back: the connection failed, was cut or timed out.
        (ProviderError::Network(_), _) => {
            format!("Couldn't reach {host}. Check your internet connection and try again.")
        }
        // The server answered, with a failure of its own (a 5xx, or Bunny's busy 400), through
        // every retry.
        (ProviderError::Server { status, .. }, _) => {
            format!("{host} had a problem (HTTP {status}). Try again in a moment.")
        }
        (ProviderError::ClockSkew, _) => CLOCK_OFF.to_owned(),
        (ProviderError::ChecksumMismatch, _) => {
            format!("{host} received a damaged copy. Try again.")
        }
        // Said by this app, in plain words, about a request it would not make.
        (ProviderError::Refused(reason), _) => sentence(reason),
        (ProviderError::Permanent(detail), Attempt::Setup) => {
            format!(
                "Couldn't set up a connection to {host}. {}",
                sentence(detail)
            )
        }
        // The server answered, but not with anything that makes sense.
        (ProviderError::Permanent(_), Attempt::List(_)) => {
            format!("{host} sent an unexpected response.")
        }
    }
}

/// Why a transfer failed, for its row in the queue's Failed tab. The row already names the site
/// and the file, so the words do not: this is the short reason on its own. A `Refused` or
/// `Permanent` error is the app's own explanation and is shown as it is.
// The transfer engine takes this as its `describe` hook (Task 23); until then only tests use it.
#[allow(dead_code)]
pub fn describe_transfer_error(error: &ProviderError) -> String {
    match error {
        ProviderError::Network(_) => "No connection".to_owned(),
        ProviderError::Server { status, .. } => {
            format!("The server had a problem (HTTP {status})")
        }
        ProviderError::RateLimited { .. } => "The server asked to slow down".to_owned(),
        ProviderError::ChecksumMismatch => {
            "The file arrived damaged twice (checksum mismatch).".to_owned()
        }
        ProviderError::Auth { .. } => "Sign-in failed".to_owned(),
        ProviderError::NotFound => "Not found".to_owned(),
        ProviderError::ClockSkew => CLOCK_OFF.to_owned(),
        ProviderError::Refused(message) | ProviderError::Permanent(message) => message.clone(),
    }
}

/// `text` as one short sentence: trimmed, at most [`DETAIL_MAX`] characters, starting with a
/// capital and ending with a full stop.
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.');
    let mut short: String = text.chars().take(DETAIL_MAX).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    let mut chars = short.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
    }
}

// ---- The Add Site sheet ----

/// Under the Storage zone field when it is empty.
pub const ZONE_MISSING: &str = "Enter the storage zone name.";
/// Under the Storage zone field when the name has a space in it.
pub const ZONE_HAS_SPACES: &str = "Zone names can't contain spaces.";
/// Under the Region picker, and the result of a test or save with no region chosen.
pub const CHOOSE_REGION: &str = "Choose a region.";
/// Under the Password field when it is empty.
pub const PASSWORD_MISSING: &str = "Enter the zone's password.";

/// The result box after a test that worked: "Connected over HTTPS · http/1.1 · 21 ms · 3 items
/// in /".
pub fn connected(http_version: &str, latency: Duration, entries: usize) -> String {
    format!(
        "Connected over HTTPS · {http_version} · {} ms · {} in /",
        latency.as_millis(),
        items(entries)
    )
}

/// The password could not be put in the credential store, so the site was not saved.
pub fn password_not_saved(reason: impl Display) -> String {
    format!("Couldn't save the password in Windows Credential Manager: {reason}")
}

/// The site database refused the new site, so it was not saved.
pub fn site_not_saved(reason: impl Display) -> String {
    format!("Couldn't save the site: {reason}")
}

/// The saved sites could not be read from the site database.
pub fn sites_unreadable(reason: impl Display) -> String {
    format!("Couldn't read the saved sites: {reason}")
}

// ---- The sidebar, the toolbar and the cloud pane ----

/// The sidebar's second line for a Bunny site: "Bunny · Singapore".
pub fn site_subtitle(service: &str, place: &str) -> String {
    format!("{service} · {place}")
}

/// The service name on a Bunny site's subtitle.
pub const BUNNY: &str = "Bunny";
/// The service name in the toolbar's subtitle for a Bunny site.
pub const BUNNY_STORAGE: &str = "Bunny Storage";

/// The toolbar subtitle: "Bunny Storage · {zone}".
pub fn endpoint(service: &str, container: &str) -> String {
    format!("{service} · {container}")
}

/// The cloud pane's header chip: the storage endpoint, "https://{host}".
pub fn chip(host: &str) -> String {
    format!("https://{host}")
}

/// The toolbar's connection pill once a listing has come back: "HTTPS · 21 ms".
pub fn connection(latency: Duration) -> String {
    format!("HTTPS · {} ms", latency.as_millis())
}

/// The pill while a site opens.
pub const CONNECTING: &str = "Connecting…";
/// The pill after a failed connection.
pub const NOT_CONNECTED: &str = "Not connected";

/// The right side of the status bar once a listing has come back: "{http_version} · {host}".
pub fn status_right(http_version: &str, host: &str) -> String {
    format!("{http_version} · {host}")
}

/// Opening a site needs its password, and none is saved.
pub fn no_password_saved(site: &str) -> String {
    format!("No password saved for {site}. Remove this site and add it again.")
}

/// Opening a site needs its password, and the credential store would not give it.
pub fn password_unreadable(site: &str, reason: impl Display) -> String {
    format!("Couldn't read the password for {site} from Windows Credential Manager: {reason}")
}

/// Opening an S3 site, which nothing can save yet.
pub const S3_LATER: &str = "Opening S3 sites arrives later in this build.";

// ---- The This PC pane ----

/// What the pane's list of drives is called when a message has to name it.
pub const THIS_PC: &str = "This PC";

/// A folder (or the list of drives) could not be listed.
pub fn local_open_failed(place: &str, reason: impl Display) -> String {
    format!("Couldn't open {place}: {reason}")
}

// ---- The status line ----

/// The status line for a drop on a pane, naming the selected site (placeholder text until uploads
/// exist). With no site selected there is no cloud pane to point to, so the text says to add one.
pub fn drop_status(batch: &DropBatch, site: Option<&str>) -> String {
    let dropped = items(batch.paths.len());
    match (batch.side, site) {
        (PaneSide::Remote, Some(site)) => {
            format!("Uploading arrives in the next build (I2): {dropped} dropped on {site}.")
        }
        (PaneSide::Remote, None) => {
            format!("Uploading arrives in the next build (I2): {dropped} dropped.")
        }
        (PaneSide::Local, Some(site)) => format!("Drop files on the {site} pane to upload them."),
        (PaneSide::Local, None) => "Add a site, then drop files on its pane to upload them.".into(),
    }
}

// ---- Starting up ----

/// The banner when the site database could not be opened and the app runs on an empty one.
pub fn database_unavailable(path: &Path, reason: impl Display) -> String {
    format!(
        "Couldn't open the site database at {}: {reason}. Sites added now won't be kept.",
        path.display()
    )
}

/// The name on the message box that tells a failed start.
pub const APP_NAME: &str = "HTTPorter";

/// What the message box says when the app cannot start (no window, no site database, no runtime).
/// Release builds have no console to print the error to, so without this the app would close
/// without a word.
pub fn startup_error_text(error: &dyn Error) -> String {
    format!("HTTPorter couldn't start.\n\n{error}")
}

// ---- The transfer drawer ----

use chrono::{DateTime, Utc};
use httporter_core::{BatchSummary, Direction, EngineState};

use crate::format::{format_count, format_progress, format_size};

/// The collapsed drawer when nothing is running, and the expanded drawer's title then.
pub const NO_TRANSFERS: &str = "No transfers running";

/// What a queued row says when its job was paused on its own.
pub const ROW_PAUSED: &str = "Paused";

/// "Uploading", "Downloading", "Deleting" or "Transferring".
fn transfer_verb(direction: Direction) -> &'static str {
    match direction {
        Direction::Upload => "Uploading",
        Direction::Download => "Downloading",
        Direction::Delete => "Deleting",
        Direction::Mixed => "Transferring",
    }
}

/// The drawer's title: "Uploading to {site}", "Downloading from {site}", "Deleting from {site}" or
/// "Transferring with {site}". When the engine is not simply running it says why first: "Paused ·
/// {title}", "Waiting for the connection · {title}", "Not enough space on {drive} · {title}", and
/// "Sign-in failed · {site}" when a site refused its credentials.
pub fn drawer_title(state: &EngineState, direction: Direction, site: &str) -> String {
    let title = match direction {
        Direction::Upload => format!("Uploading to {site}"),
        Direction::Download => format!("Downloading from {site}"),
        Direction::Delete => format!("Deleting from {site}"),
        Direction::Mixed => format!("Transferring with {site}"),
    };
    match state {
        EngineState::Idle | EngineState::Running => title,
        EngineState::Paused => format!("Paused · {title}"),
        EngineState::Offline { .. } => format!("Waiting for the connection · {title}"),
        EngineState::AuthStopped { site } => format!("Sign-in failed · {site}"),
        EngineState::DiskFull { drive, .. } => format!("Not enough space on {drive} · {title}"),
    }
}

/// "1 file", "2 files", "13,793 files".
fn files(n: u64) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", format_count(n))
    }
}

/// The drawer's second line: "8,412 of 13,793 files · 527 of 865 MB · over HTTPS to {host}", or
/// for deletes "{done} of {total} items on {host}".
pub fn drawer_line(
    direction: Direction,
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
    host: &str,
) -> String {
    let (done, total) = (format_count(files_done), format_count(files_total));
    if direction == Direction::Delete {
        format!("{done} of {total} items on {host}")
    } else {
        let size = format_progress(bytes_done, bytes_total);
        format!("{done} of {total} files · {size} · over HTTPS to {host}")
    }
}

/// The collapsed drawer while transfers run: "Uploading 8,412 of 13,793 files · 96 files/s".
pub fn drawer_collapsed_running(
    direction: Direction,
    files_done: u64,
    files_total: u64,
    files_per_s: &str,
) -> String {
    format!(
        "{} {} of {} files · {files_per_s} files/s",
        transfer_verb(direction),
        format_count(files_done),
        format_count(files_total)
    )
}

/// How long ago `then` was, in words: "just now", "5 min ago", "2 hours ago", "yesterday",
/// "3 days ago".
fn ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - then).num_seconds();
    let (minutes, hours, days) = (seconds / 60, seconds / 3600, seconds / 86_400);
    if seconds < 45 {
        "just now".to_owned()
    } else if hours < 1 {
        format!("{} min ago", minutes.max(1))
    } else if hours < 24 {
        if hours == 1 {
            "1 hour ago".to_owned()
        } else {
            format!("{hours} hours ago")
        }
    } else if days < 2 {
        "yesterday".to_owned()
    } else {
        format!("{days} days ago")
    }
}

/// "Last transfer 2 hours ago · 34 files · 4.2 MB".
pub fn drawer_last_transfer(batch: &BatchSummary, now: DateTime<Utc>) -> String {
    format!(
        "Last transfer {} · {} · {}",
        ago(batch.finished_at, now),
        files(batch.files),
        format_size(batch.bytes)
    )
}

/// The collapsed drawer when nothing runs: "No transfers running", and " · Last transfer …" when
/// there is a last batch.
pub fn drawer_idle(last: Option<&BatchSummary>, now: DateTime<Utc>) -> String {
    match last {
        Some(batch) => format!("{NO_TRANSFERS} · {}", drawer_last_transfer(batch, now)),
        None => NO_TRANSFERS.to_owned(),
    }
}

/// The time left, without "about" and "left": "56 s", "4 min", "1 h 20 min".
pub fn drawer_eta(eta: Duration) -> String {
    let seconds = eta.as_secs_f64().round() as u64;
    if seconds < 60 {
        return format!("{seconds} s");
    }
    let minutes = (seconds + 30) / 60;
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

/// The drawer's tabs: "Active 32", "Queued 5,349", "Failed 0", "Done 8,412".
pub fn drawer_tab_titles(active: u64, queued: u64, failed: u64, done: u64) -> [String; 4] {
    [
        format!("Active {}", format_count(active)),
        format!("Queued {}", format_count(queued)),
        format!("Failed {}", format_count(failed)),
        format!("Done {}", format_count(done)),
    ]
}

// ---- Sheets, alerts and banners ----

use httporter_core::{CompareBy, DeleteSummary, ProviderKind};

/// "1 folder", "3 folders".
fn folders(n: u64) -> String {
    if n == 1 {
        "1 folder".to_owned()
    } else {
        format!("{} folders", format_count(n))
    }
}

/// A size as the delete alert says it: [`format_size`] without a trailing ".0" ("54 MB", but
/// "12.3 MB").
fn size_in_words(bytes: u64) -> String {
    let size = format_size(bytes);
    match size.split_once(".0 ") {
        Some((whole, unit)) => format!("{whole} {unit}"),
        None => size,
    }
}

// The conflict sheet.

/// The conflict sheet's Contents column: both sides hold the same bytes, they differ, or that
/// could not be told.
pub const CONTENTS_IDENTICAL: &str = "Identical";
pub const CONTENTS_DIFFERENT: &str = "Different";
pub const CONTENTS_UNKNOWN: &str = "Unknown";

/// The conflict sheet's column for the files on this PC.
pub const ON_THIS_PC: &str = "On this PC";

/// "On Bunny": the conflict sheet's column for the files on the site.
pub fn on_site(short: &str) -> String {
    format!("On {short}")
}

/// A file's side of a conflict row: "357.2 KB · Today 09:58", or the size alone when the file has
/// no date.
pub fn size_and_date(size: &str, date: Option<&str>) -> String {
    match date {
        Some(date) => format!("{size} · {date}"),
        None => size.to_owned(),
    }
}

/// The conflict sheet's title. Uploads name the site, downloads the folder the files go to:
/// "12 files already exist on example-shop.com", "12 files already exist in Downloads".
pub fn conflict_title(direction: Direction, count: u64, site: &str, folder: &str) -> String {
    let what = if count == 1 {
        "1 file already exists".to_owned()
    } else {
        format!("{} already exist", files(count))
    };
    if direction == Direction::Download {
        format!("{what} in {folder}")
    } else {
        format!("{what} on {site}")
    }
}

/// Under the conflict sheet's title: what the batch is and where it goes.
pub fn conflict_body(direction: Direction, total_files: u64, target: &str) -> String {
    let verb = if direction == Direction::Download {
        "downloading"
    } else {
        "uploading"
    };
    format!(
        "You're {verb} {} to {target}. Choose once what happens to existing files; you can change \
         single files below.",
        files(total_files)
    )
}

/// Before the segmented control: "For all 12:".
pub fn conflict_for_all(count: u64) -> String {
    format!("For all {}:", format_count(count))
}

/// How a conflict compared two files, as the sheet says it: "SHA-256", "MD5" or "size".
fn compared_by_name(by: CompareBy) -> &'static str {
    match by {
        CompareBy::Sha256 => "SHA-256",
        CompareBy::Md5 => "MD5",
        CompareBy::Size => "size",
    }
}

/// The conflict sheet's summary: "7 differ · 5 identical (same SHA-256)", and " · 2 unknown" when
/// some could not be compared. `identical_by` is how the identical ones were told apart; with
/// none identical there is nothing to say about it.
pub fn conflict_summary(
    differ: u64,
    identical: u64,
    unknown: u64,
    identical_by: Option<CompareBy>,
) -> String {
    let mut text = format!(
        "{} differ · {} identical",
        format_count(differ),
        format_count(identical)
    );
    if let Some(by) = identical_by.filter(|_| identical > 0) {
        text.push_str(&format!(" (same {})", compared_by_name(by)));
    }
    if unknown > 0 {
        text.push_str(&format!(" · {} unknown", format_count(unknown)));
    }
    text
}

/// The conflict sheet's checkbox: "Remember for uploads to this site".
pub fn conflict_remember(direction: Direction) -> String {
    if direction == Direction::Download {
        "Remember for downloads from this site".to_owned()
    } else {
        "Remember for uploads to this site".to_owned()
    }
}

/// The conflict sheet's cancel button: "Cancel Upload" or "Cancel Download".
pub fn conflict_cancel(direction: Direction) -> String {
    if direction == Direction::Download {
        "Cancel Download".to_owned()
    } else {
        "Cancel Upload".to_owned()
    }
}

// The delete alert.

/// The delete alert's last sentence: Bunny keeps nothing back, and neither does S3 here.
fn cant_be_undone(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Bunny => "Bunny Storage has no trash, so this can't be undone.",
        ProviderKind::S3 => "This can't be undone.",
    }
}

/// The delete alert's title: "Delete “old-promo-2025” from example-shop.com?" for one item, "Delete 3
/// folders and 12 files from example-shop.com?" for several.
pub fn delete_title(summary: &DeleteSummary, site: &str) -> String {
    if let Some((name, _)) = &summary.single {
        return format!("Delete “{name}” from {site}?");
    }
    let what = match (summary.folders, summary.files) {
        (folders_n, 0) => folders(folders_n),
        (0, files_n) => files(files_n),
        (folders_n, files_n) => format!("{} and {}", folders(folders_n), files(files_n)),
    };
    format!("Delete {what} from {site}?")
}

/// The delete alert's text: how much goes (when there is anything to count), then that it can't
/// be undone.
pub fn delete_body(summary: &DeleteSummary, provider: ProviderKind) -> String {
    let undone = cant_be_undone(provider);
    if summary.total_files == 0 || matches!(summary.single, Some((_, false))) {
        return undone.to_owned();
    }
    let amount = format!(
        "{} ({})",
        files(summary.total_files),
        size_in_words(summary.total_bytes)
    );
    if summary.single.is_some() {
        format!("This folder holds {amount}. {undone}")
    } else {
        format!("That's {amount} in all. {undone}")
    }
}

/// The delete alert's button: "Delete 48 Files", or "Delete" for one file or nothing to count.
pub fn delete_button(summary: &DeleteSummary) -> String {
    let one_file = matches!(summary.single, Some((_, false)));
    match summary.total_files {
        0 => "Delete".to_owned(),
        _ if one_file => "Delete".to_owned(),
        1 => "Delete 1 File".to_owned(),
        n => format!("Delete {} Files", format_count(n)),
    }
}

// The New Folder sheet.

/// The name the New Folder sheet starts with.
pub const NEW_FOLDER_NAME: &str = "untitled folder";
/// Under the New Folder sheet's field when it is empty.
pub const NAME_MISSING: &str = "Enter a name.";
/// Under the field when the name has a slash in it.
pub const NAME_HAS_SEPARATOR: &str = "Names can't contain / or \\.";

/// Under the field when the folder already has an item of that name.
pub fn name_exists(name: &str) -> String {
    format!("“{name}” already exists here.")
}

/// Under the field for `.` and `..`, which name no folder.
pub fn name_not_allowed(name: &str) -> String {
    format!("“{name}” can't be used as a name.")
}

// The Remove Site alert and the quit alert.

/// The Remove Site alert's title.
pub fn remove_site_title(site: &str) -> String {
    format!("Remove “{site}”?")
}

/// The Remove Site alert's text; `provider` is the service's name ("Bunny Storage").
pub fn remove_site_body(provider: &str) -> String {
    format!(
        "HTTPorter forgets this site and its saved password. Files on {provider} aren't touched."
    )
}

/// The quit alert's title.
pub const QUIT_TITLE: &str = "Transfers are still running";

/// The quit alert's text: "5 files haven't finished.".
pub fn quit_body(unfinished: u64) -> String {
    if unfinished == 1 {
        "1 file hasn't finished.".to_owned()
    } else {
        format!("{} files haven't finished.", format_count(unfinished))
    }
}

// The banners.

/// Above the panes after a crash or a quit with work left: "5 transfers didn't finish last time.".
pub fn resume_banner(n: usize) -> String {
    if n == 1 {
        "1 transfer didn't finish last time.".to_owned()
    } else {
        format!(
            "{} transfers didn't finish last time.",
            format_count(n as u64)
        )
    }
}

/// Above the panes while the connection is down.
pub fn offline_banner() -> String {
    "You're offline. Transfers continue when the connection is back.".to_owned()
}

/// Above the panes when `site` refused its credentials.
pub fn auth_banner(site: &str) -> String {
    format!("Couldn't sign in to {site}. Its transfers are paused.")
}

/// Above the panes when a download has nowhere to go: `needed` bytes must be freed on `drive`.
pub fn disk_full_banner(drive: &str, needed: u64) -> String {
    format!(
        "Drive {drive} is full. Free up {} and click Resume.",
        format_size(needed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(side: PaneSide, n: usize) -> DropBatch {
        DropBatch {
            side,
            paths: (0..n).map(|i| format!(r"C:\f{i}").into()).collect(),
        }
    }

    #[test]
    fn item_counts() {
        assert_eq!(items(0), "0 items");
        assert_eq!(items(1), "1 item");
        assert_eq!(items(282), "282 items");
        assert_eq!(items_status(3, 0), "3 items");
        assert_eq!(items_status(1, 2), "1 item · 2 couldn't be read");
    }

    #[test]
    fn a_drop_on_the_cloud_pane_is_counted_and_one_on_this_pc_points_to_the_site() {
        let site = Some("example-shop.com");
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 1), site),
            "Uploading arrives in the next build (I2): 1 item dropped on example-shop.com."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 3), site),
            "Uploading arrives in the next build (I2): 3 items dropped on example-shop.com."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Local, 3), site),
            "Drop files on the example-shop.com pane to upload them."
        );
    }

    #[test]
    fn with_no_site_the_drop_status_names_none() {
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 2), None),
            "Uploading arrives in the next build (I2): 2 items dropped."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Local, 2), None),
            "Add a site, then drop files on its pane to upload them."
        );
    }

    /// Ruling Q5: the Failed tab's reason is site-free plain wording, one text per kind of error.
    #[test]
    fn a_failed_transfer_says_why_in_plain_site_free_words() {
        use httporter_core::AuthHint;

        let said = |error: ProviderError| describe_transfer_error(&error);
        assert_eq!(
            said(ProviderError::Network("reset".into())),
            "No connection"
        );
        assert_eq!(
            said(ProviderError::Server {
                status: 503,
                detail: "Service Unavailable".into()
            }),
            "The server had a problem (HTTP 503)"
        );
        assert_eq!(
            said(ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(30))
            }),
            "The server asked to slow down"
        );
        assert_eq!(
            said(ProviderError::ChecksumMismatch),
            "The file arrived damaged twice (checksum mismatch)."
        );
        for hint in [
            AuthHint::Unknown,
            AuthHint::AccessKey,
            AuthHint::Secret,
            AuthHint::Region,
        ] {
            assert_eq!(said(ProviderError::Auth { hint }), "Sign-in failed");
        }
        assert_eq!(said(ProviderError::NotFound), "Not found");
        assert_eq!(
            said(ProviderError::ClockSkew),
            "Your PC's clock is off. Turn on “Set time automatically” in Windows Settings."
        );
        // The app's own explanations are passed on as they are.
        assert_eq!(
            said(ProviderError::Refused("the zone root is a folder".into())),
            "the zone root is a folder"
        );
        assert_eq!(
            said(ProviderError::Permanent("unexpected (418)".into())),
            "unexpected (418)"
        );
    }

    #[test]
    fn a_failed_transfer_never_names_a_site_or_a_label() {
        for error in [
            ProviderError::Network("tcp connect error to sg.storage.bunnycdn.com".into()),
            ProviderError::Server {
                status: 500,
                detail: "boom".into(),
            },
            ProviderError::NotFound,
        ] {
            let text = describe_transfer_error(&error).to_lowercase();
            for label in ["error:", "transient", "permanent", "refused:", "bunnycdn"] {
                assert!(!text.contains(label), "{text}");
            }
        }
    }

    #[test]
    fn the_add_site_field_errors_are_pinned() {
        assert_eq!(ZONE_MISSING, "Enter the storage zone name.");
        assert_eq!(ZONE_HAS_SPACES, "Zone names can't contain spaces.");
        assert_eq!(CHOOSE_REGION, "Choose a region.");
        assert_eq!(PASSWORD_MISSING, "Enter the zone's password.");
    }

    #[test]
    fn what_the_sheet_and_the_panes_say_is_pinned() {
        assert_eq!(
            connected("http/1.1", Duration::from_millis(21), 3),
            "Connected over HTTPS · http/1.1 · 21 ms · 3 items in /"
        );
        assert_eq!(
            password_not_saved("access denied"),
            "Couldn't save the password in Windows Credential Manager: access denied"
        );
        assert_eq!(
            site_not_saved("disk full"),
            "Couldn't save the site: disk full"
        );
        assert_eq!(
            sites_unreadable("locked"),
            "Couldn't read the saved sites: locked"
        );
        assert_eq!(site_subtitle(BUNNY, "Singapore"), "Bunny · Singapore");
        assert_eq!(
            endpoint(BUNNY_STORAGE, "testzone"),
            "Bunny Storage · testzone"
        );
        assert_eq!(
            chip("sg.storage.bunnycdn.com"),
            "https://sg.storage.bunnycdn.com"
        );
        assert_eq!(connection(Duration::from_millis(21)), "HTTPS · 21 ms");
        assert_eq!(
            status_right("http/1.1", "sg.storage.bunnycdn.com"),
            "http/1.1 · sg.storage.bunnycdn.com"
        );
        assert_eq!(
            (CONNECTING, NOT_CONNECTED),
            ("Connecting…", "Not connected")
        );
        assert_eq!(
            no_password_saved("example-shop.com"),
            "No password saved for example-shop.com. Remove this site and add it again."
        );
        assert_eq!(
            password_unreadable("example-shop.com", "locked"),
            "Couldn't read the password for example-shop.com from Windows Credential Manager: locked"
        );
        assert_eq!(S3_LATER, "Opening S3 sites arrives later in this build.");
        assert_eq!(
            local_open_failed(THIS_PC, "access denied"),
            "Couldn't open This PC: access denied"
        );
        assert_eq!(
            database_unavailable(Path::new(r"C:\data\sites.db"), "locked"),
            "Couldn't open the site database at C:\\data\\sites.db: locked. Sites added now won't be kept."
        );
    }

    /// Ledger 132: a release build has no console, so a failed start is told in a message box.
    /// The text is pinned here; `main.rs` shows it.
    #[test]
    fn startup_error_text_is_pinned() {
        let error: Box<dyn Error> = "no display found".into();
        assert_eq!(
            startup_error_text(error.as_ref()),
            "HTTPorter couldn't start.\n\nno display found"
        );
        let io = std::io::Error::other("out of threads");
        assert_eq!(
            startup_error_text(&io),
            "HTTPorter couldn't start.\n\nout of threads"
        );
    }
}
