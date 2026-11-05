//! The words of Manage+ (I4): everything it shows that Rust builds. One section per task, so that
//! parallel inserts merge: add below your own header only.
#![allow(dead_code)]

// ---- cache (T4) ----

/// The warning sheet's title before a big file is fetched for a preview.
pub fn big_file_title(name: &str) -> String {
    format!("Download “{name}” to preview it?")
}

/// The warning sheet's text before a big file is fetched: its size.
pub fn big_file_body(bytes: u64) -> String {
    format!(
        "This file is {}. HTTPorter downloads it into its cache first.",
        crate::format::format_size(bytes)
    )
}

/// The warning sheet's button before a big file is fetched.
pub const BIG_FILE_BUTTON: &str = "Download";

/// A big file the user chose not to fetch.
pub const BIG_FILE_DECLINED: &str = "The file wasn't downloaded.";

/// A fetched file whose size or checksum isn't the listed one.
pub const FETCH_DAMAGED: &str =
    "The downloaded copy doesn't match the file on the site. Try again.";

//
//
//
//
//

// ---- inspector, thumbnails, links (T5) ----

//
//
//
//
//

// ---- Quick Look, edit, favorites (T6) ----

//
//
//
//
//

// ---- palette, search, tabs (T7) ----

//
//
//
//
//

// ---- settings tabs, read-only (T8) ----

//
//
//
//
//

// ---- activity, rename / move / presign (T9) ----

/// The activity sheet's site pop-up: every site.
pub const ALL_SITES: &str = "All sites";

/// When an activity event happened: "Today 10:02", "Yesterday 18:40", "4 Nov 2026 09:15".
pub fn activity_when(t: chrono::NaiveDateTime, now: chrono::NaiveDateTime) -> String {
    use chrono::Timelike;
    let day = crate::format::format_date(t, now);
    let recent = t.date() == now.date() || now.date().pred_opt() == Some(t.date());
    if recent {
        day
    } else {
        format!("{day} {:02}:{:02}", t.hour(), t.minute())
    }
}

/// An activity event's kind, as the sheet lists it.
pub fn activity_kind(kind: httporter_core::ActivityKind) -> &'static str {
    use httporter_core::ActivityKind as K;
    match kind {
        K::Upload => "Upload",
        K::Download => "Download",
        K::Delete => "Delete",
        K::Move => "Move",
        K::Publish => "Publish",
        K::Purge => "Purge",
        K::Edit => "Edit",
        K::Site => "Site",
    }
}

/// What the sheet says after an event's path: its own words, else its size.
pub fn activity_detail(e: &httporter_core::ActivityEvent) -> String {
    if !e.detail.is_empty() {
        e.detail.clone()
    } else if e.bytes > 0 {
        crate::format::format_size(e.bytes)
    } else {
        String::new()
    }
}

/// Export… of the activity sheet: the save dialog's title, the file's name and its type.
pub const EXPORT_ACTIVITY_TITLE: &str = "Export Activity";
pub const EXPORT_ACTIVITY_NAME: &str = "HTTPorter activity.txt";
pub const TEXT_FILES: &str = "Text";

/// The activity was exported.
pub fn activity_exported(n: u64) -> String {
    match n {
        1 => "Exported 1 event.".to_owned(),
        n => format!("Exported {} events.", crate::format::format_count(n)),
    }
}

/// The activity couldn't be exported.
pub fn activity_not_exported(reason: &str) -> String {
    format!("The activity couldn't be exported: {reason}")
}

/// A site added or removed, as the activity log says it.
pub const SITE_ADDED: &str = "Added";
pub const SITE_REMOVED: &str = "Removed";

/// Rename and move on a read-only site.
pub fn move_read_only(site: &str) -> String {
    format!("{site} is read-only. Renaming and moving are off.")
}

/// Rename with more than one item selected.
pub const RENAME_ONE: &str = "Select one item to rename.";

/// The Rename sheet's title.
pub fn rename_title(name: &str) -> String {
    format!("Rename “{name}”")
}

/// A new name that can't be used.
pub const NAME_EMPTY: &str = "Enter a name.";
pub const NAME_SLASH: &str = "A name can't contain “/” or “\\”.";
pub const NAME_INVALID: &str = "That name can't be used.";

/// The Move To… sheet's title.
pub fn move_title_one(name: &str) -> String {
    format!("Move “{name}”")
}

pub fn move_title(n: usize) -> String {
    format!("Move {} Items", crate::format::format_count(n as u64))
}

/// The line under the Move To… field.
pub const MOVE_NOTE: &str = "The folder on the site, from the top: /images/2026. It must exist.";

/// A destination that isn't a folder path.
pub const FOLDER_INVALID: &str = "That isn't a folder path.";

/// Why a move can't be done (a name taken, a folder into itself, the site unreachable).
pub fn move_refused(error: &httporter_core::ProviderError) -> String {
    crate::copy::describe_transfer_error(error)
}

/// The warning before a move that downloads and uploads again.
pub fn move_warning_title(files: u64) -> String {
    match files {
        1 => "Move 1 file through this PC?".to_owned(),
        n => format!(
            "Move {} files through this PC?",
            crate::format::format_count(n)
        ),
    }
}

pub fn move_warning_body(files: u64, bytes: u64, minutes: u64) -> String {
    let files = match files {
        1 => "1 file".to_owned(),
        n => format!("{} files", crate::format::format_count(n)),
    };
    let time = match minutes {
        0 | 1 => "about a minute".to_owned(),
        n => format!("about {n} min"),
    };
    format!(
        "Bunny can't copy on the server. HTTPorter downloads and re-uploads {files} ({}, {time}), \
         then deletes the originals, only if every copy succeeds.",
        crate::format::format_size(bytes)
    )
}

pub const MOVE_BUTTON: &str = "Move";

/// The site couldn't be reached to start a move.
pub fn move_failed_to_start(site: &str) -> String {
    format!("Couldn't reach {site} to move the files. Try again.")
}

/// A move under way: "Moving… 12 of 40".
pub fn moving(done: u64, total: u64) -> String {
    format!(
        "Moving… {} of {}",
        crate::format::format_count(done),
        crate::format::format_count(total)
    )
}

pub const MOVE_DELETING: &str = "Moving… deleting the originals";

/// A move that went through.
pub fn moved(items: usize, to: &httporter_core::RemotePath) -> String {
    let what = match items {
        1 => "1 item".to_owned(),
        n => format!("{} items", crate::format::format_count(n as u64)),
    };
    format!("Moved {what} to /{}.", to.as_str())
}

/// A move whose copies partly failed: nothing was deleted.
pub fn move_partly_failed(failed: usize, kept: &[httporter_core::RemotePath]) -> String {
    let failed = match failed {
        1 => "1 copy failed".to_owned(),
        n => format!("{} copies failed", crate::format::format_count(n as u64)),
    };
    if kept.is_empty() {
        return format!("{failed}; nothing was deleted.");
    }
    const SHOWN: usize = 3;
    let mut names: Vec<String> = kept
        .iter()
        .take(SHOWN)
        .map(|p| format!("/{}", p.as_str()))
        .collect();
    if kept.len() > SHOWN {
        names.push(format!("and {} more", kept.len() - SHOWN));
    }
    format!(
        "{failed}; nothing was deleted. Copies made: {}.",
        names.join(", ")
    )
}

/// A move that couldn't run.
pub fn move_failed(reason: &str) -> String {
    format!("The move couldn't run: {reason}")
}

/// A move as the activity log names it: "/a/b.txt → /c" or "/a/b.txt and 3 more → /c".
pub fn move_activity_path(first: &str, items: usize, to: &httporter_core::RemotePath) -> String {
    if items > 1 {
        format!("/{first} and {} more → /{}", items - 1, to.as_str())
    } else {
        format!("/{first} → /{}", to.as_str())
    }
}

/// Copy Presigned Link… with no file selected.
pub const PRESIGN_FILES_ONLY: &str = "Select one or more files to copy pre-signed links.";

/// The presign sheet's title.
pub fn presign_title(files: usize) -> String {
    match files {
        1 => "Copy Presigned Link".to_owned(),
        n => format!(
            "Copy Presigned Links for {} Files",
            crate::format::format_count(n as u64)
        ),
    }
}

/// Pre-signed links copied: "Copied 3 presigned links (they expire in 1 hour)."
pub fn presigned_copied(links: usize, secs: u64) -> String {
    let lasts = match secs {
        s if s <= 15 * 60 => "15 minutes",
        s if s <= 3600 => "1 hour",
        s if s <= 24 * 3600 => "1 day",
        _ => "7 days",
    };
    match links {
        1 => format!("Copied 1 presigned link (it expires in {lasts})."),
        n => format!(
            "Copied {} presigned links (they expire in {lasts}).",
            crate::format::format_count(n as u64)
        ),
    }
}

//
//
//
//
//
