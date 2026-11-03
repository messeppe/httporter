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

use httporter_core::{AuthHint, ProviderError, RemotePath};

use crate::drop_hook::{DropBatch, PaneSide};
use crate::format::format_size;
use crate::viewmodel::RowData;

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
pub const CLOCK_OFF: &str =
    "Your PC's clock is off. Turn on “Set time automatically” in Windows Settings.";

/// An S3 service did not recognise the access key ID.
pub const S3_ACCESS_KEY_UNKNOWN: &str = "The access key ID wasn't recognized.";
/// An S3 service found the signature wrong: the secret is not the access key ID's.
pub const S3_SECRET_WRONG: &str = "The secret access key doesn't match the access key ID.";
/// An S3 service wanted the request signed for another region.
pub const S3_REGION_WRONG: &str = "The region doesn't match the bucket. Check the region.";

/// What to tell the user about a failed attempt to talk to a site's `container` (a Bunny zone or
/// an S3 bucket) on `host`: [`describe`], with S3's sign-in failures named by the field that is
/// likely wrong (the access key ID, the secret or the region).
pub fn describe_for(
    provider: ProviderKind,
    error: &ProviderError,
    container: &str,
    host: &str,
    attempt: Attempt<'_>,
) -> String {
    match (provider, error) {
        (ProviderKind::S3, ProviderError::Auth { hint }) => match hint {
            AuthHint::AccessKey => S3_ACCESS_KEY_UNKNOWN.to_owned(),
            AuthHint::Secret => S3_SECRET_WRONG.to_owned(),
            AuthHint::Region => S3_REGION_WRONG.to_owned(),
            AuthHint::Unknown => format!(
                "Couldn't sign in to {container}. Check the access key ID, the secret access key \
                 and the bucket."
            ),
        },
        _ => describe(error, container, host, attempt),
    }
}

/// What to tell the user about a failed listing of `dir` of a site's `container` (a Bunny zone or
/// an S3 bucket) on `host`, in `region` (the city, or the S3 region): [`describe_for`], except
/// that a 404 on the root of the zone or bucket names the field that is wrong (Minor 4), since
/// there is no folder to blame.
pub fn describe_listing(
    provider: ProviderKind,
    error: &ProviderError,
    container: &str,
    host: &str,
    region: &str,
    dir: &RemotePath,
) -> String {
    match (provider, error, dir.is_root()) {
        (ProviderKind::Bunny, ProviderError::NotFound, true) => zone_not_found(container, region),
        (ProviderKind::S3, ProviderError::NotFound, true) => bucket_not_found(container, region),
        _ => describe_for(provider, error, container, host, Attempt::List(dir)),
    }
}

/// The zone's root is not there: the zone name or the region is wrong.
pub fn zone_not_found(zone: &str, region: &str) -> String {
    format!("Couldn't find zone “{zone}” in {region}. Check the zone name and region.")
}

/// The bucket's root is not there: the bucket name or the region is wrong.
pub fn bucket_not_found(bucket: &str, region: &str) -> String {
    if region.trim().is_empty() {
        format!("Couldn't find bucket “{bucket}”. Check the bucket name and region.")
    } else {
        format!("Couldn't find bucket “{bucket}” in {region}. Check the bucket name and region.")
    }
}

/// What to tell the user about a failed attempt to talk to `zone` on `host`: plain words for each
/// kind of error, without the labels the errors carry for the code ("network error: …", "server
/// error (HTTP 503): …"), and never the password or a request URL. Only a client that could not
/// be set up adds its own explanation, cut short: no request was made, so there is nothing else
/// to go on.
pub fn describe(error: &ProviderError, zone: &str, host: &str, attempt: Attempt<'_>) -> String {
    match (error, attempt) {
        // Bunny never says which part was wrong; S3's hints have their own words
        // ([`describe_for`]).
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
/// `Permanent` error is the app's own explanation and is shown as it is. The transfer engine takes
/// this as its `describe` hook.
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
/// Under the Account key field when it is empty.
pub const ACCOUNT_KEY_MISSING: &str = "Enter your Bunny account key.";
/// Under the Access key ID field when it is empty.
pub const KEY_ID_MISSING: &str = "Enter the access key ID.";
/// Under the Secret access key field when it is empty.
pub const SECRET_MISSING: &str = "Enter the secret access key.";
/// Under the Bucket field when it is empty.
pub const BUCKET_MISSING: &str = "Enter the bucket name.";

/// Importing a `.env` file that has no line setting `name` (for example `BUNNY_KEY`).
pub fn env_missing(name: &str) -> String {
    format!("The file has no {name} line.")
}

/// Importing a `.env` file whose line for `name` has nothing after the `=`.
pub fn env_empty(name: &str) -> String {
    format!("The {name} line is empty.")
}

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

/// The right side of the status bar once a listing has come back: "{http_version} · {host}", and
/// with `ready` (the connections open and idle) " · {n} connections ready". `None` until the
/// transfer engine can say (Task 23).
pub fn status_right(http_version: &str, host: &str, ready: Option<usize>) -> String {
    match ready {
        None => format!("{http_version} · {host}"),
        Some(1) => format!("{http_version} · {host} · 1 connection ready"),
        Some(n) => format!("{http_version} · {host} · {n} connections ready"),
    }
}

/// Opening a site needs its password, and none is saved. Edit… on the site's menu (or Remove…)
/// is the way out.
pub fn no_password_saved(site: &str) -> String {
    format!("No password saved for {site}. Edit the site to enter it again.")
}

/// Opening a site needs its password, and the credential store would not give it.
pub fn password_unreadable(site: &str, reason: impl Display) -> String {
    format!("Couldn't read the password for {site} from Windows Credential Manager: {reason}")
}

/// Why a saved secret can't be used: it is not the kind the site's service signs in with (read
/// after [`password_unreadable`]'s colon).
pub const SECRET_MISMATCH: &str = "it isn't the kind of key this site needs";
/// An S3 site saved without its S3 details (the database refuses such a site, so only a damaged
/// one has none).
pub const S3_DETAILS_MISSING: &str = "the site's S3 details are missing";
/// An S3 site whose saved address can't be read back.
pub const S3_ADDRESS_UNREADABLE: &str = "the site's saved address can't be read";

/// The sidebar's second line for an S3 site: the service and the region ("Cloudflare R2 ·
/// auto"), or the service alone when the site has no region.
pub fn s3_subtitle(service: &str, region: &str) -> String {
    if region.trim().is_empty() {
        service.to_owned()
    } else {
        site_subtitle(service, region)
    }
}

/// The banner when rows of the site database could not be read: "1 saved site couldn't be
/// read." (Ruling Q23).
pub fn sites_skipped(n: usize) -> String {
    if n == 1 {
        "1 saved site couldn't be read.".to_owned()
    } else {
        format!("{n} saved sites couldn't be read.")
    }
}

// ---- Editing, duplicating and removing a site ----

/// The name a duplicated site gets: "example-shop.com copy".
pub fn duplicate_name(name: &str) -> String {
    format!("{name} copy")
}

/// The site database refused the changed site, so nothing was changed.
pub fn site_not_updated(reason: impl Display) -> String {
    format!("Couldn't save the changes: {reason}")
}

/// The site could not be removed from the site database.
pub fn site_not_removed(reason: impl Display) -> String {
    format!("Couldn't remove the site: {reason}")
}

/// The site's secret could not be read to copy it, so the site was not duplicated.
pub fn site_not_duplicated(reason: impl Display) -> String {
    format!("Couldn't duplicate the site: {reason}")
}

/// The site to edit, duplicate or remove is no longer saved.
pub const SITE_GONE: &str = "That site is no longer saved.";

// ---- The Bunny account key and the S3 bucket list ----

/// The name a kept Bunny account key is saved under.
pub const BUNNY_ACCOUNT_NAME: &str = "Bunny account";
/// The host the Bunny account API answers on, for messages.
const BUNNY_API_HOST: &str = "api.bunny.net";
/// Find Zones found nothing that can be added.
pub const NO_ZONES: &str = "This account has no storage zones that can be added.";
/// An S3 key that may not list the buckets: the user types the name instead.
pub const BUCKETS_NOT_ALLOWED: &str = "This key can't list buckets. Type the bucket name instead.";

/// Find Zones failed: why, in plain words.
pub fn account_failed(error: &ProviderError) -> String {
    match error {
        ProviderError::Auth { .. } => "Bunny didn't accept that account key. Copy it again from \
             the Bunny dashboard → Account settings → API key."
            .to_owned(),
        other => describe(other, BUNNY_API_HOST, BUNNY_API_HOST, Attempt::Setup),
    }
}

/// The account key could not be kept in the credential store. The zones were added.
pub fn account_key_not_saved(reason: impl Display) -> String {
    format!("The sites were added, but the account key couldn't be kept: {reason}")
}

/// Choose… could not list the buckets: why, naming the field that is likely wrong.
pub fn buckets_failed(error: &ProviderError, host: &str) -> String {
    match error {
        ProviderError::Auth {
            hint: AuthHint::Unknown,
        } => BUCKETS_NOT_ALLOWED.to_owned(),
        other => describe_for(ProviderKind::S3, other, host, host, Attempt::Setup),
    }
}

// ---- Import ----

/// The title of the file picker Import opens.
pub const PICK_ENV_TITLE: &str = "Choose a .env file to import";
/// The picker's file types: `.env` files, and anything.
pub const ENV_FILES: &str = "Environment files";
pub const ALL_FILES: &str = "All files";
/// The `.env` file could not be read.
pub fn env_unreadable(reason: impl Display) -> String {
    format!("Couldn't read the file: {reason}")
}

/// An AWS CLI profile in Import's list: "default · eu-west-1", or the name alone.
pub fn aws_profile_label(name: &str, region: Option<&str>) -> String {
    match region {
        Some(region) => format!("{name} · {region}"),
        None => name.to_owned(),
    }
}

// ---- Selection, the toolbar and the context menus ----

/// The status bar's left side for the rows selected in a pane: "index.html selected · 383.4 KB"
/// for one file, "uploads selected" for one folder, "12 items selected · 4.2 MB" for several (the
/// size is that of the files; with only folders there is none). Empty when nothing is selected,
/// so that the item count shows instead.
pub fn selection_summary(rows: &[&RowData]) -> String {
    match rows {
        [] => String::new(),
        [row] if row.folder => format!("{} selected", row.name),
        [row] => format!("{} selected · {}", row.name, format_size(row.bytes)),
        rows => {
            let mut files = rows.iter().filter(|row| !row.folder).peekable();
            let has_files = files.peek().is_some();
            let bytes: u64 = files.map(|row| row.bytes).sum();
            if has_files {
                format!("{} selected · {}", items(rows.len()), format_size(bytes))
            } else {
                format!("{} selected", items(rows.len()))
            }
        }
    }
}

/// Why Delete is off while the This PC pane is the one in use (its tooltip).
pub const DELETE_ON_THIS_PC: &str =
    "Delete works in the cloud pane. Use Explorer for files on this PC.";

/// Why Upload, Download, New folder and Delete are off until the transfer engine is wired in (the
/// window's tests run without one).
pub const TRANSFERS_LATER: &str = "Transfers arrive in a later version";

// ---- The This PC pane ----

/// What the pane's list of drives is called when a message has to name it.
pub const THIS_PC: &str = "This PC";

/// A folder (or the list of drives) could not be listed.
pub fn local_open_failed(place: &str, reason: impl Display) -> String {
    format!("Couldn't open {place}: {reason}")
}

/// A folder of This PC is not there any more (deleted or renamed since the pane showed it).
pub fn local_not_found(place: &str) -> String {
    format!("Couldn't find {place}.")
}

// ---- The status line ----

/// The status line for a drop on a pane, naming the selected site and, when the files landed on a
/// folder row of the cloud pane, the folder (placeholder text until uploads exist). With no site
/// selected there is no cloud pane to point to, so the text says to add one.
pub fn drop_status(batch: &DropBatch, site: Option<&str>, folder: Option<&str>) -> String {
    let dropped = items(batch.paths.len());
    match (batch.side, site, folder) {
        (PaneSide::Remote, Some(site), Some(folder)) => format!(
            "Uploading arrives in the next build (I2): {dropped} dropped on the {folder} folder in {site}."
        ),
        (PaneSide::Remote, None, Some(folder)) => format!(
            "Uploading arrives in the next build (I2): {dropped} dropped on the {folder} folder."
        ),
        (PaneSide::Remote, Some(site), None) => {
            format!("Uploading arrives in the next build (I2): {dropped} dropped on {site}.")
        }
        (PaneSide::Remote, None, None) => {
            format!("Uploading arrives in the next build (I2): {dropped} dropped.")
        }
        (PaneSide::Local, Some(site), _) => {
            format!("Drop files on the {site} pane to upload them.")
        }
        (PaneSide::Local, None, _) => {
            "Add a site, then drop files on its pane to upload them.".into()
        }
    }
}

/// The status line when files were dropped on the window but where they landed could not be told
/// (the window or the system would not say where the cursor was). The files are ignored.
pub const DROP_NOT_LOCATED: &str = "Couldn't tell where the files were dropped. Try again.";

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

use crate::format::{format_count, format_progress};

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

// ---- The Settings window ----

/// Under the bandwidth limit when its number is not a speed above zero.
pub const BANDWIDTH_INVALID: &str = "Enter a speed above 0 MB/s.";
/// Under the Proxy row when Custom is chosen and no address is given.
pub const PROXY_URL_MISSING: &str = "Enter a proxy address such as http://proxy:8080.";
/// Under the Proxy row when the address has a user name or password in it: settings are stored
/// in a database that must never hold a secret.
pub const PROXY_HAS_PASSWORD: &str =
    "HTTPorter can't store a proxy password yet. Use a proxy without one, or Use Windows settings.";

// ---- Transfers ----

use httporter_transfer::messages;

/// The title of the file picker Upload opens when nothing is selected on This PC.
pub const PICK_FILES_TITLE: &str = "Choose files to upload";
/// The title of the folder picker of Download to…, and of Download at the list of drives.
pub const PICK_FOLDER_TITLE: &str = "Choose a folder to download to";

/// The status line while an upload, a download or a delete is being worked out (folders listed,
/// files compared).
pub const PREPARING_UPLOAD: &str = "Preparing the upload…";
pub const PREPARING_DOWNLOAD: &str = "Preparing the download…";
pub const PREPARING_DELETE: &str = "Counting what to delete…";

/// Ctrl+V in the cloud pane with no files on the clipboard.
pub const NOTHING_TO_PASTE: &str = "There are no files on the clipboard to upload.";

/// Files dropped on the cloud pane with no site open in it.
pub const DROP_NO_SITE: &str = "Add a site, then drop files on its pane to upload them.";

/// Files dropped on the cloud pane while its site is still opening (or couldn't be opened).
pub fn drop_not_ready(site: &str) -> String {
    format!("Wait until {site} has opened, then drop the files again.")
}

/// An upload or download whose every file is already there and was left alone.
pub const NOTHING_TO_TRANSFER: &str = "Nothing to transfer: every file is already there.";

/// The note on a folder made with New Folder that Bunny Storage won't list until it holds a file.
pub const VIRTUAL_FOLDER_NOTE: &str = "Appears on Bunny Storage when you add its first file.";

/// Where an upload goes, as the conflict sheet says it: the site, then each folder
/// ("example-shop.com / wp-content / uploads").
pub fn remote_folder_label(site: &str, dir: &RemotePath) -> String {
    std::iter::once(site)
        .chain(dir.segments())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// A folder on this PC as the conflict sheet names it: its name ("Downloads"), or the drive.
pub fn local_folder_label(dir: &Path) -> String {
    dir.file_name().map_or_else(
        || dir.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// A transfer the journal could not record, so it was not started (ruling Q21).
pub fn transfer_not_started(reason: impl Display) -> String {
    format!(
        "Couldn't start the transfer: HTTPorter couldn't save its list of transfers ({reason})."
    )
}

/// Items of an upload or download that won't be sent, with the first one and why.
pub fn skipped_items(n: usize, first: &str, reason: &str) -> String {
    if n == 1 {
        format!("1 item was skipped: {first}: {reason}")
    } else {
        format!("{n} items were skipped. {first}: {reason}")
    }
}

/// New Folder failed.
pub fn folder_not_created(name: &str, reason: impl Display) -> String {
    format!("Couldn't create the folder {name}: {reason}")
}

/// "Remember for this site" could not be saved; the transfer goes ahead.
pub fn choice_not_remembered(reason: impl Display) -> String {
    format!("Couldn't remember the choice for this site: {reason}")
}

/// The Transfers settings could not be saved; they apply until HTTPorter closes.
pub fn settings_not_saved(reason: impl Display) -> String {
    format!("Couldn't save the settings: {reason}. They apply until HTTPorter closes.")
}

/// The resume banner's Resume could not open a site the unfinished transfers go to.
pub fn resume_needs_site(site: &str, reason: &str) -> String {
    format!("Couldn't resume the transfers to {site}: {reason}")
}

/// The banner when the transfer journal has failed reads or writes (ruling Q21).
pub fn journal_errors(n: u64) -> String {
    let times = if n == 1 {
        "once".to_owned()
    } else {
        format!("{} times", format_count(n))
    };
    format!(
        "HTTPorter couldn't save its list of transfers ({times}). If it closes now, some \
         transfers may be offered again or forgotten."
    )
}

/// The banner when the transfer journal could not be opened and transfers are kept in memory.
pub fn journal_unavailable(path: &Path, reason: impl Display) -> String {
    format!(
        "Couldn't open the list of transfers in {}: {reason}. Transfers won't resume after \
         HTTPorter closes.",
        path.display()
    )
}

/// The notification when a long run of transfers ends while HTTPorter is in the background:
/// title and text, "Upload finished" and "Uploaded 1,204 files to example-shop.com.", and the
/// variants for downloads, deletes, a mix, and failures.
pub fn finished_notification(
    direction: Direction,
    done: u64,
    failed: u64,
    site: &str,
) -> (String, String) {
    let (noun, body) = match direction {
        Direction::Upload => ("Upload", format!("Uploaded {} to {site}.", files(done))),
        Direction::Download => (
            "Download",
            format!("Downloaded {} from {site}.", files(done)),
        ),
        Direction::Delete => ("Delete", format!("Deleted {} from {site}.", files(done))),
        Direction::Mixed => ("Transfer", format!("Finished {} with {site}.", files(done))),
    };
    if failed == 0 {
        (format!("{noun} finished"), body)
    } else {
        (
            format!("{noun} finished with problems"),
            format!("{body} {} failed. Open HTTPorter to retry.", files(failed)),
        )
    }
}

/// A transfer engine's own reason for a failure, in the app's words: the engine's fixed phrases
/// (`httporter_transfer::messages`) as full sentences. Anything else (the app's own
/// `describe_transfer_error`, a local file's system message) is shown as it is.
pub fn transfer_message(text: &str) -> String {
    let words = match text {
        messages::STOPPED_UNEXPECTEDLY => "The transfer stopped unexpectedly.",
        messages::HASHING_STOPPED => "Checking the file stopped before it finished.",
        messages::FILE_WORK_STOPPED => "Work on the file on this PC stopped before it finished.",
        messages::NO_ANSWER_FOR_FILE => "The server didn't answer for this file.",
        messages::DAMAGED_TWICE => "The file arrived damaged twice (checksum mismatch).",
        other => other,
    };
    words.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(side: PaneSide, n: usize) -> DropBatch {
        DropBatch {
            side,
            row: None,
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
            drop_status(&batch(PaneSide::Remote, 1), site, None),
            "Uploading arrives in the next build (I2): 1 item dropped on example-shop.com."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 3), site, None),
            "Uploading arrives in the next build (I2): 3 items dropped on example-shop.com."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Local, 3), site, None),
            "Drop files on the example-shop.com pane to upload them."
        );
    }

    #[test]
    fn with_no_site_the_drop_status_names_none() {
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 2), None, None),
            "Uploading arrives in the next build (I2): 2 items dropped."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Local, 2), None, None),
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
            status_right("http/1.1", "sg.storage.bunnycdn.com", None),
            "http/1.1 · sg.storage.bunnycdn.com"
        );
        assert_eq!(
            (CONNECTING, NOT_CONNECTED),
            ("Connecting…", "Not connected")
        );
        assert_eq!(
            no_password_saved("example-shop.com"),
            "No password saved for example-shop.com. Edit the site to enter it again."
        );
        assert_eq!(
            password_unreadable("example-shop.com", "locked"),
            "Couldn't read the password for example-shop.com from Windows Credential Manager: locked"
        );
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
