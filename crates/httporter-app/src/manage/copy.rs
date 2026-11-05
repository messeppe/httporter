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

use crate::format::format_count;
use httporter_transfer::{SEARCH_LIMIT, SearchProgress};

// The palette's sections.
pub const SECTION_ACTIONS: &str = "Actions";
pub const SECTION_SITES: &str = "Sites";
pub const SECTION_PROFILES: &str = "Profiles";
pub const SECTION_FAVORITES: &str = "Favorites";
pub const SECTION_FOLDERS: &str = "Folders";
pub const SECTION_SHORTCUTS: &str = "Shortcuts";

/// The palette's Publish: the profile it publishes, or a new one.
pub fn palette_publish(profile: Option<&str>) -> String {
    match profile {
        Some(name) => format!("Publish \u{201c}{name}\u{201d}"),
        None => "Publish\u{2026}".to_owned(),
    }
}

/// Where a profile publishes from and to: "D:\site → exampleshop/blog".
pub fn palette_publish_route(local: &str, container: &str, remote: &str) -> String {
    if remote.is_empty() {
        format!("{local} \u{2192} {container}")
    } else {
        format!("{local} \u{2192} {container}/{remote}")
    }
}

/// Publish on a site with no profile yet.
pub fn palette_new_profile(site: &str) -> String {
    format!("Set up a publish profile for {site}")
}

pub const PALETTE_PURGE: &str = "Purge from CDN\u{2026}";
pub const PALETTE_UPLOAD: &str = "Upload Files\u{2026}";
pub const PALETTE_DOWNLOAD: &str = "Download";
pub const PALETTE_NEW_FOLDER: &str = "New Folder\u{2026}";
pub const PALETTE_RENAME: &str = "Rename\u{2026}";
pub const PALETTE_MOVE: &str = "Move To\u{2026}";
pub const PALETTE_COPY_LINK: &str = "Copy Public Link";
pub const PALETTE_COMPARE: &str = "Compare Panes";
pub const PALETTE_SINGLE_PANE: &str = "Single Pane with Inspector";
pub const PALETTE_TWO_PANES: &str = "Two Panes";
pub const PALETTE_NEW_TAB: &str = "New Tab";
pub const PALETTE_ACTIVITY: &str = "Activity Log";
pub const PALETTE_SETTINGS: &str = "Settings\u{2026}";
pub const PALETTE_ADD_SITE: &str = "Add Site\u{2026}";
pub const PALETTE_REFRESH: &str = "Refresh";

pub const PALETTE_ON: &str = "On";
pub const PALETTE_OFF: &str = "Off";
pub const PALETTE_VIEW_NOTE: &str = "Change the window\u{2019}s layout";
pub const PALETTE_NEW_TAB_NOTE: &str = "Open the same folders in another tab";
pub const PALETTE_ACTIVITY_NOTE: &str = "Uploads, deletes, publishes and purges";
pub const PALETTE_SETTINGS_NOTE: &str = "Transfers, sites, publishing and more";
pub const PALETTE_ADD_SITE_NOTE: &str = "Bunny Storage or an S3-compatible bucket";

/// "To example-shop.com / blog", "In D:\site": where an action acts.
pub fn palette_to(place: &str) -> String {
    format!("To {place}")
}

pub fn palette_in(place: &str) -> String {
    format!("In {place}")
}

/// A site's folder as the palette names it: "example-shop.com / blog".
pub fn palette_site_folder(site: &str, folder: Option<&str>) -> String {
    match folder {
        Some(folder) => format!("{site} / {folder}"),
        None => site.to_owned(),
    }
}

/// The selection a purge or a link acts on.
pub fn palette_selected_on(site: &str) -> String {
    format!("Selected items on {site}")
}

pub fn palette_profile_note(meta: &str) -> String {
    if meta.is_empty() {
        "Publish profile".to_owned()
    } else {
        format!("Publish profile \u{b7} {meta}")
    }
}

pub fn palette_folder_on(site: &str) -> String {
    format!("Folder on {site}")
}

// Why an action is off, in place of its subtitle.
pub const PALETTE_OPEN_A_SITE: &str = "Open a site first";
pub const PALETTE_WRITES_OFF: &str = "Open a site that isn\u{2019}t read-only";
pub const PALETTE_PICK_ON_SITE: &str = "Select items on the site first";
pub const PALETTE_PICK_ONE_ON_SITE: &str = "Select one item on the site first";
pub const PALETTE_PURGE_OFF: &str = "Select items on a Bunny site with a pull zone";
pub const PALETTE_DOWNLOAD_OFF: &str = "Select items in the cloud pane first";
pub const PALETTE_NEW_FOLDER_OFF: &str = "Open a folder first";
pub const PALETTE_COMPARE_OFF: &str = "Open a folder on This PC and a site folder first";
pub const PALETTE_LATER: &str = "Arrives in a later version";

// The keys shown beside an action.
pub const KEYS_PUBLISH: &str = "Ctrl P";
pub const KEYS_UPLOAD: &str = "Ctrl U";
pub const KEYS_NEW_FOLDER: &str = "Ctrl Shift N";
pub const KEYS_RENAME: &str = "F2";
pub const KEYS_COPY_LINK: &str = "Ctrl Shift C";
pub const KEYS_NEW_TAB: &str = "Ctrl T";
pub const KEYS_REFRESH: &str = "F5";

/// The shortcuts the palette lists for a `?` query (§8.7.3): what, and the keys.
pub const SHORTCUTS: [(&str, &str); 23] = [
    ("Command palette", "Ctrl K"),
    ("Keyboard shortcuts", "?"),
    ("Quick Look", "Space"),
    ("Open", "Enter"),
    ("Enclosing folder", "Backspace"),
    ("Back", "Alt \u{2190}"),
    ("Forward", "Alt \u{2192}"),
    ("Refresh", "F5"),
    ("Rename", "F2"),
    ("Delete", "Del"),
    ("New folder", "Ctrl Shift N"),
    ("Upload", "Ctrl U"),
    ("Paste files to upload", "Ctrl V"),
    ("Select all", "Ctrl A"),
    ("Publish", "Ctrl P"),
    ("Copy public link", "Ctrl Shift C"),
    ("Search the site", "Ctrl F"),
    ("New tab", "Ctrl T"),
    ("Close tab", "Ctrl W"),
    ("Next tab", "Ctrl Tab"),
    ("Previous tab", "Ctrl Shift Tab"),
    ("This PC pane", "Ctrl 1"),
    ("Cloud pane", "Ctrl 2"),
];

/// The toolbar's search field on a site.
pub fn search_placeholder(site: Option<&str>) -> String {
    match site {
        Some(site) => format!("Search {site}"),
        None => "Search".to_owned(),
    }
}

/// The cloud pane's last crumb while it shows a search.
pub fn search_results_for(text: &str) -> String {
    format!("Results for \u{201c}{text}\u{201d}")
}

pub const SEARCH_NEEDS_SITE: &str = "Open a site to search it.";
pub const SEARCH_STARTING: &str = "Searching\u{2026}";

/// The status bar while a search runs: "Searching… 1,204 folders · 37 found".
pub fn search_progress(p: SearchProgress) -> String {
    format!(
        "Searching\u{2026} {} folders \u{b7} {} found",
        format_count(p.folders),
        format_count(p.found)
    )
}

/// The status bar once a search has listed everything (or found as many as it shows).
pub fn search_done(p: SearchProgress) -> String {
    if p.found >= SEARCH_LIMIT as u64 {
        format!(
            "The first {} found \u{b7} {} folders searched",
            format_count(p.found),
            format_count(p.folders)
        )
    } else if p.found == 1 {
        format!("1 found in {} folders", format_count(p.folders))
    } else {
        format!(
            "{} found in {} folders",
            format_count(p.found),
            format_count(p.folders)
        )
    }
}

/// The status bar when a search could not go on.
pub fn search_failed(reason: &str) -> String {
    format!("Search stopped: {reason}")
}

/// A tab's title: "example-shop.com · blog", the site alone at its root, else This PC's folder.
pub fn tab_title(site: Option<&str>, folder: Option<&str>, local: Option<&str>) -> String {
    match (site, folder) {
        (Some(site), Some(folder)) => format!("{site} \u{b7} {folder}"),
        (Some(site), None) => site.to_owned(),
        (None, _) => local.unwrap_or(crate::copy::THIS_PC).to_owned(),
    }
}

/// A tab's second line: This PC's folder.
pub fn tab_subtitle(local: Option<&str>) -> String {
    local.unwrap_or(crate::copy::THIS_PC).to_owned()
}

/// Synchronized browsing turned itself off: the other pane has no such folder.
pub fn sync_stopped(folder: &str, other: &str) -> String {
    format!("Synchronized browsing stopped: {folder} isn\u{2019}t in {other}")
}

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
