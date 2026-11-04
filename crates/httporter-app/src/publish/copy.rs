//! The words of Publish: everything it shows that Rust builds. One section per task, so that
//! parallel inserts merge: add below your own header only.

// ---- compare (T6) ----
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

/// "1 file", "13,793 files".
fn sheet_files(n: u64) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", crate::format::format_count(n))
    }
}

/// The sheet's title: "Publish to example-shop.com".
pub fn sheet_title(site: &str) -> String {
    format!("Publish to {site}")
}

/// The subtitle while the folders are compared.
pub fn sheet_comparing(site: &str) -> String {
    format!(
        "Comparing the folder with {site}. Nothing is uploaded or deleted until you click Publish."
    )
}

/// The subtitle once compared: "Compared 13,793 files by SHA-256 checksum. …".
pub fn sheet_compared(files: u64, by: &str) -> String {
    format!(
        "Compared {} by {by}. Nothing is uploaded or deleted until you click Publish.",
        sheet_files(files)
    )
}

/// How the files were told apart: the weakest way any of them was.
pub fn compared_by(by: httporter_core::CompareBy, by_date: bool) -> &'static str {
    match by {
        httporter_core::CompareBy::Sha256 => "SHA-256 checksum",
        httporter_core::CompareBy::Md5 => "MD5",
        httporter_core::CompareBy::Size if by_date => "size and date",
        httporter_core::CompareBy::Size => "size",
    }
}

/// The plan list's place while the site is reached.
pub fn sheet_connecting(site: &str) -> String {
    format!("Connecting to {site}…")
}

/// How far the compare is: "Comparing… 4,210 of 13,793 files", or while listing, "Listing… 9,120
/// files on this PC · 8,904 on Bunny".
pub fn sheet_progress(p: &httporter_transfer::ScanProgress, service: &str) -> String {
    use crate::format::format_count;
    if p.to_hash > 0 {
        format!(
            "Comparing… {} of {}",
            format_count(p.hashed),
            sheet_files(p.to_hash)
        )
    } else {
        format!(
            "Listing… {} on this PC · {} on {service}",
            sheet_files(p.local_files),
            format_count(p.remote_files)
        )
    }
}

/// A local file or folder of the compare couldn't be read.
pub fn compare_local_failed(path: &std::path::Path, message: &str) -> String {
    format!("Couldn't read {}: {message}", path.display())
}

/// No client could be made for the profile's site.
pub fn no_connection(site: &str) -> String {
    format!("Couldn't connect to {site}. Check the site with Edit Site, then try again.")
}

/// The destination row: the storage endpoint and the profile's folder on it.
pub fn sheet_destination(endpoint: &str, dir: &httporter_core::RemotePath) -> String {
    format!(
        "{}/{}",
        endpoint.trim_end_matches('/'),
        dir.to_url_path(true)
    )
}

/// "Destination · HTTPS · 32 parallel uploads".
pub fn destination_note(parallel: u32) -> String {
    format!("Destination · HTTPS · {parallel} parallel uploads")
}

/// The summary's right end: "Upload 36 files · 4.2 MB", with the deletes when there are any.
pub fn upload_text(files: u64, bytes: u64, deletes: u64) -> String {
    let size = crate::format::format_size(bytes);
    match (files, deletes) {
        (0, 0) => "Nothing to upload".to_owned(),
        (_, 0) => format!("Upload {} · {size}", sheet_files(files)),
        (0, _) => format!("Delete {}", sheet_files(deletes)),
        _ => format!(
            "Upload {} · {size} · delete {}",
            sheet_files(files),
            crate::format::format_count(deletes)
        ),
    }
}

/// "Remove files from Bunny that aren't in the folder" (S3: the site's name).
pub fn remove_label(service: &str) -> String {
    format!("Remove files from {service} that aren't in the folder")
}

/// The summary chip after its count: "only on Bunny".
pub fn sheet_remote_label(service: &str) -> String {
    format!("only on {service}")
}

/// The plan list's group headers.
pub const GROUP_CHANGED: &str = "Changed";
pub const GROUP_NEW: &str = "New";

/// "Only on Bunny".
pub fn group_remote(service: &str) -> String {
    format!("Only on {service}")
}

/// A header's count: "34 files", or "3 of 34 files" when only some are ticked.
pub fn group_count(included: u64, total: u64) -> String {
    if included == total {
        sheet_files(total)
    } else {
        format!(
            "{} of {}",
            crate::format::format_count(included),
            sheet_files(total)
        )
    }
}

/// A plan item's chip.
pub const ITEM_CHANGED: &str = "Changed";
pub const ITEM_NEW: &str = "New";
pub const WILL_BE_KEPT: &str = "Will be kept";
pub const WILL_BE_DELETED: &str = "Will be deleted";

/// Why the purge can't be chosen (an S3 site), in the sheet and the delete alert.
pub const PURGE_NEEDS_BUNNY: &str = "Purging needs a Bunny pull zone.";

/// Why the purge became Whole zone.
pub fn zone_is_faster(urls: usize) -> String {
    format!(
        "{} URLs to purge, purging the whole zone is faster.",
        crate::format::format_count(urls as u64)
    )
}

/// The warning line: files that look like secrets and will be uploaded.
pub fn secrets_warning(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut list = names
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        list.push_str(", …");
    }
    if names.len() == 1 {
        format!("{list} looks like a secret and will be uploaded. Untick it or add a skip rule.")
    } else {
        format!(
            "{} look like secrets and will be uploaded: {list}. Untick them or add skip rules.",
            sheet_files(names.len() as u64)
        )
    }
}

/// The publish button: "Publish 36 Files".
pub fn publish_button(n: u64) -> String {
    if n == 1 {
        "Publish 1 File".to_owned()
    } else {
        format!("Publish {} Files", crate::format::format_count(n))
    }
}

/// The publish button while there is nothing to do, or nothing compared yet.
pub const PUBLISH: &str = "Publish";
/// The publish button when the delete guard trips.
pub const REVIEW_DELETIONS: &str = "Review Deletions…";

/// Export Plan…'s save dialog.
pub const EXPORT_TITLE: &str = "Export Plan";
pub const EXPORT_FILTER: &str = "Text files";

/// The file name Export Plan… offers.
pub fn export_file_name(profile: &str) -> String {
    format!("{profile} publish plan.txt")
}

pub fn plan_saved(path: &std::path::Path) -> String {
    format!("Plan saved to {}.", path.display())
}

pub fn plan_not_saved(reason: impl std::fmt::Display) -> String {
    format!("Couldn't save the plan: {reason}")
}

/// Change Folder…'s folder picker.
pub const PICK_PUBLISH_FOLDER: &str = "Choose the Folder to Publish";

pub fn folder_not_saved(reason: &str) -> String {
    format!("Couldn't save the profile: {reason}")
}

pub const PROFILE_GONE: &str = "That publish profile no longer exists.";
pub const PROFILE_SITE_GONE: &str = "The site of that publish profile no longer exists.";
pub const OPEN_A_SITE: &str = "Open a site, then click Publish to make a publish profile for it.";
pub const REVIEW_NAME_WRONG: &str = "The name typed doesn't match, so nothing was published.";

/// The status line while a publish runs.
pub fn phase(site: &str, phase: httporter_transfer::PublishPhase) -> String {
    use crate::format::format_count;
    use httporter_transfer::PublishPhase;
    match phase {
        PublishPhase::Uploading => format!("Publishing to {site}: uploading…"),
        PublishPhase::Deleting => {
            format!("Publishing to {site}: deleting the files that aren't in the folder…")
        }
        PublishPhase::Purging { done, total } => format!(
            "Purging CDN… {} of {}",
            format_count(done as u64),
            format_count(total as u64)
        ),
        PublishPhase::Finished => format!("Publishing to {site}: finishing…"),
    }
}

pub fn publish_not_started(reason: &str) -> String {
    format!("Couldn't start the publish: HTTPorter couldn't save its list of transfers ({reason}).")
}

pub fn history_not_saved(reason: &str) -> String {
    format!("The publish ran, but its history couldn't be saved: {reason}")
}

/// What a finished publish says in the status line.
pub fn published(site: &str, record: &httporter_core::HistoryRecord) -> String {
    use crate::format::format_count;
    use httporter_core::{PublishResult, PurgeResult};
    let purge = match &record.purge {
        PurgeResult::Off => String::new(),
        PurgeResult::Urls(n) => format!(" CDN purged ({} URLs).", format_count(*n)),
        PurgeResult::Zone => " CDN zone purged.".to_owned(),
        PurgeResult::Failed(_) => " CDN purge failed, Retry Purge on the profile page.".to_owned(),
        PurgeResult::Unavailable(why) => format!(" {why}"),
    };
    match record.result {
        PublishResult::Ok => format!(
            "Published to {site}: {} uploaded, {} deleted.{purge}",
            format_count(record.uploaded),
            format_count(record.deleted)
        ),
        PublishResult::Partial => format!(
            "Published to {site}, but {} failed. Retry them on the profile page.{purge}",
            sheet_files(record.failed)
        ),
        PublishResult::Failed => format!("Publishing to {site} failed. The profile page says why."),
        PublishResult::Cancelled => format!("Publishing to {site} was cancelled."),
    }
}

/// What a purge on its own (Retry Purge, the delete alert's) did.
pub fn purge_done(result: &httporter_core::PurgeResult) -> String {
    use httporter_core::PurgeResult;
    match result {
        PurgeResult::Off => "Nothing to purge.".to_owned(),
        PurgeResult::Urls(n) => format!(
            "Purged {} URLs from the CDN.",
            crate::format::format_count(*n)
        ),
        PurgeResult::Zone => "Purged the whole CDN zone.".to_owned(),
        PurgeResult::Failed(reason) => format!("CDN purge failed: {reason}"),
        PurgeResult::Unavailable(why) => why.clone(),
    }
}

pub const NOTHING_TO_RETRY: &str = "Nothing failed in that publish.";
pub const HISTORY_GONE: &str = "That publish is no longer in the history.";

/// The sidebar's note for a profile that never published.
pub const SIDEBAR_NEVER: &str = "Never";

/// How long ago a profile last published, short: "just now", "5m ago", "2h ago", "3d ago".
pub fn ago_short(
    then: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let seconds = (now - then).num_seconds();
    if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// The delete alert's note when ticking the purge will ask for the key.
pub const PURGE_ASKS_KEY: &str = "Asks for your Bunny account key";
/// The CDN key sheet was cancelled.
pub const PURGE_NO_KEY: &str = "No Bunny account key, so the CDN wasn't purged.";
/// Another site's key was being asked for already.
pub const KEY_SHEET_BUSY: &str =
    "The CDN wasn't purged: HTTPorter was already asking for another Bunny account key.";
pub const KEY_REFUSED: &str =
    "Bunny didn't accept that key. Copy the API key from your Bunny account settings.";

pub fn no_pull_zone(zone: &str) -> String {
    format!("That account has no pull zone in front of the storage zone {zone}.")
}

pub fn key_check_failed(reason: impl std::fmt::Display) -> String {
    format!("Couldn't check the key with Bunny: {reason}")
}

pub fn key_not_saved(reason: &str) -> String {
    format!("Couldn't save the key: {reason}")
}

pub const KEY_SAVED: &str = "Bunny account key saved in Windows Credential Manager.";

pub fn account_unreachable(reason: impl std::fmt::Display) -> String {
    format!("Couldn't reach the Bunny account to purge the CDN: {reason}")
}

/// The delete alert's purge couldn't run.
pub fn delete_not_purged(why: &str) -> String {
    format!("The files are deleted, but the CDN wasn't purged: {why}")
}

// ---- profile (T8) ----
//
//
//
//
//
