//! The words of Manage+ (I4): everything it shows that Rust builds. One section per task, so that
//! parallel inserts merge: add below your own header only.
#![allow(dead_code)]

// ---- cache (T4) ----

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

/// The site's side of a compare or an edit, as "Changes vs {}" and "Changed on {}" name it:
/// Bunny for a Bunny site, the site's own name for an S3 one.
pub fn ql_remote_label(site: &httporter_core::SiteRecord) -> String {
    match site.provider {
        httporter_core::ProviderKind::Bunny => crate::copy::BUNNY.to_owned(),
        httporter_core::ProviderKind::S3 => site.name.clone(),
    }
}

/// Quick Look's second view.
pub fn ql_changes_vs(label: &str) -> String {
    format!("Changes vs {label}")
}

/// Quick Look's footer, left: how to close it, and how to step when there is more than one file.
pub fn ql_hint(can_step: bool) -> String {
    if can_step {
        "Space or Esc to close · ← → previous / next file".to_owned()
    } else {
        "Space or Esc to close".to_owned()
    }
}

/// Quick Look's subtitle: "HTML · 383.4 KB · on this PC · changed since last publish".
pub fn ql_subtitle(kind: &str, bytes: u64, place: &str, note: Option<&str>) -> String {
    let kind = if kind.is_empty() { "File" } else { kind };
    let size = crate::format::format_size(bytes);
    match note {
        Some(note) => format!("{kind} · {size} · {place} · {note}"),
        None => format!("{kind} · {size} · {place}"),
    }
}

pub const QL_ON_THIS_PC: &str = "on this PC";

pub fn ql_on_site(site: &str) -> String {
    format!("on {site}")
}

/// What Compare says of a file, in Quick Look's subtitle.
pub const QL_CHANGED_SINCE_PUBLISH: &str = "changed since last publish";
pub const QL_NOT_PUBLISHED: &str = "not published yet";
pub const QL_DIFFERS_FROM_PC: &str = "different on this PC";
pub const QL_NOT_ON_PC: &str = "not on this PC";

/// Why "Changes vs …" is off.
pub fn ql_no_counterpart(label: &str) -> String {
    format!("There's no copy of this file on {label} to compare with.")
}
pub const QL_NO_LOCAL_COPY: &str = "There's no copy of this file on this PC to compare with.";
pub const QL_TOO_LARGE_TO_COMPARE: &str =
    "This file is too large to compare (over 2 MB or 20,000 lines).";
pub const QL_ONLY_TEXT_COMPARES: &str = "Only text files can be compared.";

/// Quick Look's footer, right, for text: "UTF-8 · LF · 1,204 lines", or what part is shown.
pub fn ql_text_footer(encoding: &str, eol: &str, lines: usize, cut: bool) -> String {
    let count = crate::format::format_count(lines as u64);
    let lines = match (cut, lines) {
        (true, _) => format!("first {count} lines"),
        (false, 1) => "1 line".to_owned(),
        (false, _) => format!("{count} lines"),
    };
    if eol.is_empty() {
        format!("{encoding} · {lines}")
    } else {
        format!("{encoding} · {eol} · {lines}")
    }
}

/// Quick Look's footer, right, for the changes: "UTF-8 · LF · 3 lines changed".
pub fn ql_diff_footer(encoding: &str, eol: &str, changed: usize) -> String {
    let changed = match changed {
        0 => "no changes".to_owned(),
        1 => "1 line changed".to_owned(),
        n => format!("{} lines changed", crate::format::format_count(n as u64)),
    };
    if eol.is_empty() {
        format!("{encoding} · {changed}")
    } else {
        format!("{encoding} · {eol} · {changed}")
    }
}

/// Quick Look's footer, right, for an image.
pub fn ql_image_footer(width: u32, height: u32) -> String {
    format!("{width} × {height} px")
}

pub const QL_UTF8: &str = "UTF-8";
pub const QL_NOT_UTF8: &str = "Not UTF-8";
pub const QL_LF: &str = "LF";
pub const QL_CRLF: &str = "CRLF";
pub const QL_MIXED_EOL: &str = "Mixed line endings";

pub fn ql_read_failed(reason: impl std::fmt::Display) -> String {
    format!("Couldn't read this file: {reason}")
}

pub const QL_IMAGE_FAILED: &str = "Couldn't show this image.";

pub const QL_FILE_GONE: &str = "This file is no longer in the folder.";

pub fn ql_open_failed(name: &str, reason: impl std::fmt::Display) -> String {
    format!("Couldn't open {name}: {reason}")
}

/// Open in Browser on a site whose public address isn't known.
pub fn ql_no_public_url(site: &str) -> String {
    format!("{site} has no public address yet. Set one in Settings › Sites.")
}

/// The sidebar's Editing rows.
pub const EDIT_OPENING: &str = "Opening…";
pub const EDIT_EDITING: &str = "Editing";
pub const EDIT_UPLOADING: &str = "Uploading…";
pub const EDIT_FAILED: &str = "Failed";

pub fn edit_uploaded_at(time: &str) -> String {
    format!("Uploaded {time}")
}

pub fn edit_changed_on(label: &str) -> String {
    format!("Changed on {label}")
}

/// The status line after an edit's upload.
pub fn edit_uploaded(name: &str, site: &str) -> String {
    format!("Uploaded {name} to {site}")
}

pub fn edit_upload_failed(name: &str, reason: impl std::fmt::Display) -> String {
    format!("Couldn't upload {name}: {reason}")
}

pub fn edit_open_failed(name: &str, reason: impl std::fmt::Display) -> String {
    format!("Couldn't open {name} for editing: {reason}")
}

pub fn edit_cancelled(name: &str) -> String {
    format!("The upload of {name} was cancelled. Save it again to send it.")
}

pub fn edit_read_only(site: &str) -> String {
    format!("{site} is read-only: this file opens as a copy, and changes to it aren't uploaded.")
}

pub fn edit_watch_failed(reason: impl std::fmt::Display) -> String {
    format!("Edited files can't be watched ({reason}), so changes to them aren't uploaded.")
}

pub fn edit_no_connection(site: &str) -> String {
    format!("Couldn't connect to {site}")
}

/// The name "Save as Copy" gives the edited file: `index (edited).html`.
pub fn edit_copy_name(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} (edited).{ext}"),
        _ => format!("{name} (edited)"),
    }
}

/// The edit conflict sheet's text, under the file's name.
pub fn edit_conflict_body(label: &str, copy: &str) -> String {
    format!(
        "It changed on {label} since you opened it. Overwrite it with your version, save yours \
         as “{copy}”, or discard your edit."
    )
}

pub fn edit_discarded(name: &str) -> String {
    format!("Your edit to {name} was discarded; it now has the version from the site.")
}

pub fn edit_saved_copy(copy: &str, site: &str) -> String {
    format!("Saved your version as {copy} on {site}")
}

/// The activity log's detail of an edit's upload.
pub const EDIT_EVENT_DETAIL: &str = "Uploaded after an edit";

/// The sidebar's Favorites.
pub fn favorite_added(name: &str) -> String {
    format!("Added {name} to Favorites")
}

pub fn favorite_exists(name: &str) -> String {
    format!("{name} is in Favorites already")
}

pub fn favorite_failed(reason: impl std::fmt::Display) -> String {
    format!("Couldn't change Favorites: {reason}")
}

pub fn favorite_missing(name: &str) -> String {
    format!("Couldn't open {name}: it may have been moved or deleted.")
}

pub const FAVORITE_SITE_GONE: &str = "That favorite's site is no longer saved.";

/// A cloud favorite's row: "uploads / 2026" (its parent and itself).
pub fn favorite_remote_name(parent: Option<&str>, name: &str) -> String {
    match parent {
        Some(parent) => format!("{parent} / {name}"),
        None => name.to_owned(),
    }
}

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

//
//
//
//
//
