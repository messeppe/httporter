//! The words of Publish: everything it shows that Rust builds. One section per task, so that
//! parallel inserts merge: add below your own header only.

// ---- compare (T6) ----

use httporter_core::{ProviderKind, SiteRecord};
use httporter_transfer::{CompareError, ScanProgress};

use crate::format::format_count;

/// The site's side of a compare, as "Only on {}" and "{n} only on {}" name it: Bunny for a Bunny
/// site, the site's own name for an S3 one.
pub fn remote_label(site: &SiteRecord) -> String {
    match site.provider {
        ProviderKind::Bunny => crate::copy::BUNNY.to_owned(),
        ProviderKind::S3 => site.name.clone(),
    }
}

/// The status bar while a compare starts.
pub const COMPARING: &str = "Comparing…";

/// The status bar while a compare runs: the files listed so far on both sides, then the files
/// whose contents are being checked.
pub fn compare_progress(progress: ScanProgress) -> String {
    let listed = progress.local_files + progress.remote_files;
    if progress.to_hash > 0 {
        format!(
            "Comparing… checked {} of {} files",
            format_count(progress.hashed),
            format_count(progress.to_hash)
        )
    } else if listed > 0 {
        format!("Comparing… {} files listed", format_count(listed))
    } else {
        COMPARING.to_owned()
    }
}

/// Why a compare stopped, for the status bar.
pub fn compare_failed(error: &CompareError) -> String {
    match error {
        CompareError::Local { path, message } => {
            format!(
                "Couldn't compare: {} can't be read ({message})",
                path.display()
            )
        }
        CompareError::Remote(error) => format!(
            "Couldn't compare: {}",
            crate::copy::describe_transfer_error(error)
        ),
    }
}

//
//
//
//
//

// ---- sheet (T7) ----
//
//
//
//
//

// ---- profile (T8) ----

/// The sidebar's line under a profile: when it last published ("2h ago"), or "Never".
pub fn profile_meta(
    last: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let Some(last) = last else {
        return NEVER.to_owned();
    };
    let minutes = (now - last).num_minutes().max(0);
    match (minutes, minutes / 60, minutes / (60 * 24)) {
        (0, _, _) => "Just now".to_owned(),
        (m, 0, _) => format!("{m} min ago"),
        (_, h, 0) => format!("{h}h ago"),
        (_, _, 1) => "Yesterday".to_owned(),
        (_, _, d) => format!("{d} days ago"),
    }
}

/// A profile that never published, or a card with no publish yet.
pub const NEVER: &str = "Never";

/// A new profile's name: the site's, and the folder on it when that isn't the root
/// ("example-shop.com / blog").
pub fn new_profile_name(site: &str, remote: &httporter_core::RemotePath) -> String {
    if remote.is_root() {
        site.to_owned()
    } else {
        format!("{site} / {}", remote.as_str())
    }
}

/// Where a profile publishes to: "https://sg.storage.bunnycdn.com/exampleshop/blog/", or for S3
/// the bucket's address, in the path or in the host as the site has it.
pub fn destination(
    site: &httporter_core::SiteRecord,
    remote: &httporter_core::RemotePath,
) -> String {
    let folder = remote.to_url_path(true);
    let bucket = &site.container;
    match &site.s3 {
        Some(s3) if s3.path_style => format!("{}/{bucket}/{folder}", s3.endpoint),
        Some(s3) => match s3.endpoint.split_once("://") {
            Some((scheme, host)) => format!("{scheme}://{bucket}.{host}/{folder}"),
            None => format!("{}/{bucket}/{folder}", s3.endpoint),
        },
        None => format!("https://{}/{bucket}/{folder}", site.host),
    }
}

/// The profile page's chips: "12 skip rules".
pub fn skip_rules_chip(n: usize) -> String {
    match n {
        0 => "No skip rules".to_owned(),
        1 => "1 skip rule".to_owned(),
        n => format!("{n} skip rules"),
    }
}

/// "Remove extras: off".
pub fn remove_extras_chip(on: bool) -> String {
    format!("Remove extras: {}", if on { "on" } else { "off" })
}

/// "Purge: changed files · Pull Zone 7654321"; without the zone while the site has no CDN link.
pub fn purge_chip(mode: httporter_core::PurgeMode, zones: &[u64]) -> String {
    use httporter_core::PurgeMode;
    let mode = match mode {
        PurgeMode::Off => return "Purge: off".to_owned(),
        PurgeMode::Changed => "changed files",
        PurgeMode::Zone => "whole zone",
    };
    match pull_zones(zones) {
        Some(zones) => format!("Purge: {mode} · {zones}"),
        None => format!("Purge: {mode}"),
    }
}

/// "Pull Zone 7654321", "Pull Zones 1, 2"; `None` for none.
fn pull_zones(zones: &[u64]) -> Option<String> {
    let ids: Vec<String> = zones.iter().map(u64::to_string).collect();
    match ids.len() {
        0 => None,
        1 => Some(format!("Pull Zone {}", ids[0])),
        _ => Some(format!("Pull Zones {}", ids.join(", "))),
    }
}

/// "Delete guard: 100 files or 10%".
pub fn guard_chip(guard: &httporter_core::DeleteGuard) -> String {
    format!(
        "Delete guard: {} files or {}%",
        crate::format::format_count(guard.files),
        guard.percent
    )
}

/// The Last publish card's line: "36 files · 4.2 MB · all verified".
pub fn last_detail(h: &httporter_core::HistoryRecord) -> String {
    let files = crate::format::format_count(h.uploaded + h.deleted);
    let size = crate::format::format_size(h.bytes);
    let check = if h.failed > 0 {
        format!("{} failed", crate::format::format_count(h.failed))
    } else if h.verified {
        "all verified".to_owned()
    } else {
        "not verified".to_owned()
    };
    format!("{files} files · {size} · {check}")
}

/// The Last publish card's line before the first publish.
pub const LAST_NONE: &str = "Publish uploads the folder for the first time.";

/// The second card's label: "On Bunny now", or "On {site} now" for S3.
pub fn on_remote_now(site: Option<&httporter_core::SiteRecord>) -> String {
    match site {
        Some(site) if site.s3.is_some() => format!("On {} now", site.name),
        _ => "On Bunny now".to_owned(),
    }
}

/// "13,795 files".
pub fn remote_title(files: u64) -> String {
    format!("{} files", crate::format::format_count(files))
}

/// "1,011 folders · replicated to 3 regions".
pub fn remote_detail(folders: u64, regions: u32) -> String {
    let folders = format!("{} folders", crate::format::format_count(folders));
    match regions {
        0 => folders,
        1 => format!("{folders} · replicated to 1 region"),
        n => format!("{folders} · replicated to {n} regions"),
    }
}

/// The second card before the first publish.
pub const REMOTE_UNKNOWN: &str = "-";
pub const REMOTE_UNKNOWN_DETAIL: &str = "Counted at the first publish";

/// The Schedule card (scheduled publishing arrives in a later version).
pub const SCHEDULE_OFF: &str = "Off";

/// Under the history: "5 publishes · kept for the last 200"; while searching, how many match.
pub fn history_footer(total: usize, shown: usize, searching: bool) -> String {
    let keep = httporter_core::HISTORY_KEEP;
    if searching {
        format!("{shown} of {total} publishes match · kept for the last {keep}")
    } else if total == 1 {
        format!("1 publish · kept for the last {keep}")
    } else {
        format!("{total} publishes · kept for the last {keep}")
    }
}

/// The page's warning when the profile's folder is gone.
pub const FOLDER_GONE: &str =
    "This folder can't be found. Choose another with Change Folder… before publishing.";
/// The page's warning when the profile's site was removed.
pub const SITE_GONE: &str = "The site this profile publishes to has been removed.";

/// A history row's When: "Today 15:12", "Yesterday 17:02", "3 Nov 2026", in local time.
pub fn history_when(
    t: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    crate::format::format_date(
        t.with_timezone(&chrono::Local).naive_local(),
        now.naive_local(),
    )
}

/// A history row's Result: "Published", "Published · 2 failed", "First publish", "Nothing
/// changed", "Cancelled by you". `first`: the profile's first publish.
pub fn history_result(h: &httporter_core::HistoryRecord, first: bool) -> String {
    use httporter_core::{PublishResult, PurgeResult};
    let failed = crate::format::format_count(h.failed);
    match h.result {
        PublishResult::Ok if matches!(h.purge, PurgeResult::Failed(_)) => {
            "Published · CDN purge failed".to_owned()
        }
        PublishResult::Ok if first => "First publish".to_owned(),
        PublishResult::Ok if h.uploaded + h.deleted == 0 => "Nothing changed".to_owned(),
        PublishResult::Ok => "Published".to_owned(),
        PublishResult::Partial => format!("Published · {failed} failed"),
        PublishResult::Failed => format!("Not published · {failed} failed"),
        PublishResult::Cancelled => "Cancelled by you".to_owned(),
    }
}

/// How long a publish took: "7 s", "2 m 27 s", "1 h 5 m".
pub fn history_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, s) => format!("{m} m {s} s"),
        (h, m, _) => format!("{h} h {m} m"),
    }
}

/// A history row's CDN purge: "36 URLs", "Whole zone", "Off", "Failed", "Skipped".
pub fn history_purge(purge: &httporter_core::PurgeResult) -> String {
    use httporter_core::PurgeResult;
    match purge {
        PurgeResult::Off => "Off".to_owned(),
        PurgeResult::Urls(1) => "1 URL".to_owned(),
        PurgeResult::Urls(n) => format!("{} URLs", crate::format::format_count(*n)),
        PurgeResult::Zone => "Whole zone".to_owned(),
        PurgeResult::Failed(_) => "Failed".to_owned(),
        PurgeResult::Unavailable(_) => "Skipped".to_owned(),
    }
}

/// The line over a publish's detail: "Today 10:40 · 2 files failed, the first: HTTP 503 after 5
/// attempts", and the reason when the purge failed.
pub fn history_summary(
    when: &str,
    h: &httporter_core::HistoryRecord,
    files: &[httporter_core::HistoryFile],
) -> String {
    use httporter_core::{FileOutcome, PurgeResult};
    let first_error = files.iter().find_map(|f| match &f.outcome {
        FileOutcome::Failed(error) => Some(error.as_str()),
        _ => None,
    });
    let mut text = match (h.failed, first_error) {
        (0, _) => format!(
            "{when} · {} uploaded, {} deleted, {}",
            crate::format::format_count(h.uploaded),
            crate::format::format_count(h.deleted),
            crate::format::format_size(h.bytes)
        ),
        (1, Some(error)) => format!("{when} · 1 file failed: {error}"),
        (1, None) => format!("{when} · 1 file failed"),
        (n, Some(error)) => format!(
            "{when} · {} files failed, the first: {error}",
            crate::format::format_count(n)
        ),
        (n, None) => format!("{when} · {} files failed", crate::format::format_count(n)),
    };
    if let PurgeResult::Failed(reason) = &h.purge {
        text = format!("{text} · CDN purge failed: {reason}");
    }
    text
}

/// One file of a publish once Show All was pressed: "Uploaded  blog/index.html".
pub fn history_file_line(f: &httporter_core::HistoryFile) -> String {
    use httporter_core::{FileAction, FileOutcome};
    let what = match (&f.outcome, f.action) {
        (FileOutcome::Done, FileAction::Upload) => "Uploaded",
        (FileOutcome::Done, FileAction::Delete) => "Deleted",
        (FileOutcome::Failed(_), _) => "Failed",
        (FileOutcome::Cancelled, _) => "Cancelled",
        (FileOutcome::NotRun(_), FileAction::Upload) => "Not uploaded",
        (FileOutcome::NotRun(_), FileAction::Delete) => "Not deleted",
    };
    format!("{what}  {}", f.path.as_str())
}

/// The profile sheet's line under the purge choice: the pull zone and that the key is kept.
pub fn cdn_note(zones: &[u64]) -> String {
    match pull_zones(zones) {
        Some(zones) => format!("{zones} · key saved"),
        None => "Key saved · no pull zone found yet".to_owned(),
    }
}

/// The line on an S3 site, which has no pull zone to purge.
pub const CDN_NOTE_S3: &str = "Purging needs a Bunny pull zone.";
/// The line on a Bunny site without a CDN link while an account key is kept: the publish finds
/// the pull zone with it.
pub const CDN_NOTE_ACCOUNT: &str =
    "Your saved Bunny account key finds the pull zone when you publish.";
/// The line while the sheet looks the pull zone up.
pub const CDN_NOTE_CHECKING: &str = "Looking for the pull zone…";

/// The profile sheet's messages.
pub const PROFILE_NAME_MISSING: &str = "Give the profile a name.";
pub const PROFILE_FOLDER_MISSING: &str = "Choose the folder to publish.";
pub const PROFILE_FOLDER_NOT_FOUND: &str = "That folder can't be found. Choose another.";
pub const PROFILE_REMOTE_INVALID: &str =
    "A folder on the site can't be named \".\" or \"..\". Check the folder to publish to.";
pub const PROFILE_GUARD_FILES_INVALID: &str =
    "Enter the number of files as a whole number, such as 100.";
pub const PROFILE_GUARD_PERCENT_INVALID: &str = "Enter a percentage from 0 to 100.";

/// Saving a profile failed.
pub fn profile_not_saved(reason: impl std::fmt::Display) -> String {
    format!("The profile couldn't be saved: {reason}")
}

/// Deleting a profile failed.
pub fn profile_not_deleted(reason: impl std::fmt::Display) -> String {
    format!("The profile couldn't be deleted: {reason}")
}

/// The profile a `.env` import asked for couldn't be saved (the site was).
pub fn imported_profile_not_saved(reason: impl std::fmt::Display) -> String {
    format!("The site was added, but its publish profile couldn't be saved: {reason}")
}

/// The title of the folder picker of Change Folder….
pub const PICK_PROFILE_FOLDER: &str = "Choose the folder to publish";

/// Settings › Publish: the line under a kept account key ("Purges the CDN · added 4 Nov 2026").
pub fn key_meta(created: chrono::DateTime<chrono::Utc>) -> String {
    let added = created.with_timezone(&chrono::Local);
    format!("Purges the CDN · added {}", added.format("%-d %b %Y"))
}

/// Settings › Publish: forgetting a key failed.
pub fn key_not_forgotten(reason: impl std::fmt::Display) -> String {
    format!("The key couldn't be forgotten: {reason}")
}
