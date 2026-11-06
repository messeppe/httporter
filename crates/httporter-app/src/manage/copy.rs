// SPDX-License-Identifier: MIT OR Apache-2.0

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

/// The kind of a folder. The inspector compares `kind-text` with it (inspector.slint) to show the
/// folder icon: keep the two the same.
pub const FOLDER_KIND: &str = "Folder";

/// The kind of a file, from its extension (any case): "JPEG image", "HTML document", … and
/// "{EXT} file" for one that isn't known, "Document" without one.
pub fn kind_of(name: &str) -> String {
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let known = match ext.as_str() {
        "jpg" | "jpeg" => "JPEG image",
        "png" => "PNG image",
        "webp" => "WebP image",
        "gif" => "GIF image",
        "svg" => "SVG image",
        "avif" => "AVIF image",
        "ico" => "Icon",
        "bmp" => "BMP image",
        "html" | "htm" => "HTML document",
        "css" => "CSS stylesheet",
        "js" | "mjs" => "JavaScript",
        "json" => "JSON",
        "xml" | "xsl" => "XML document",
        "txt" => "Plain text",
        "md" => "Markdown",
        "csv" => "CSV",
        "map" => "Source map",
        "pdf" => "PDF document",
        "zip" => "ZIP archive",
        "gz" | "tgz" => "Gzip archive",
        "woff" | "woff2" | "ttf" | "otf" => "Font",
        "mp4" | "webm" | "mov" => "Video",
        "mp3" | "wav" | "ogg" | "m4a" => "Audio",
        "" => "Document",
        _ => return format!("{} file", ext.to_ascii_uppercase()),
    };
    known.to_owned()
}

/// The line under the inspector's name: "JPEG image · 2048 × 1536 · 211.6 KB" (the dimensions
/// once the image is read, the size of a file).
pub fn kind_line(kind: &str, dimensions: Option<(u32, u32)>, size: Option<u64>) -> String {
    let mut line = kind.to_owned();
    if let Some((w, h)) = dimensions {
        line.push_str(&format!(" · {w} × {h}"));
    }
    if let Some(size) = size {
        line.push_str(&format!(" · {}", crate::format::format_size(size)));
    }
    line
}

/// Several items selected: "3 items · 1.2 MB" (the size of the files among them, when known).
pub fn several(count: usize, bytes: Option<u64>) -> String {
    let items = crate::copy::items(count);
    match bytes {
        Some(bytes) => format!("{items} · {}", crate::format::format_size(bytes)),
        None => items,
    }
}

/// A time in the inspector, in the PC's time zone: "28 Oct 2026, 17:02".
pub fn long_date(t: chrono::DateTime<chrono::Utc>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%-d %b %Y, %H:%M")
        .to_string()
}

/// A time that isn't known (yet, or at all).
pub const NOT_KNOWN: &str = "\u{2014}";

/// Where a Bunny file is kept: "Singapore → Los Angeles, Frankfurt" (the main region, then the
/// replicas). Codes are matched in any case (ledger 97); an unknown one is shown as it is.
pub fn regions(main: &str, replicas: &[String]) -> String {
    let city = |code: &str| {
        httporter_bunny::REGIONS
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(code))
            .map_or_else(|| code.to_ascii_uppercase(), |(_, city)| (*city).to_owned())
    };
    let others: Vec<String> = replicas
        .iter()
        .filter(|code| !code.eq_ignore_ascii_case(main))
        .map(|code| city(code))
        .collect();
    if others.is_empty() {
        city(main)
    } else {
        format!("{} → {}", city(main), others.join(", "))
    }
}

/// The names of the checksums the inspector shows.
pub const SHA256_LABEL: &str = "SHA-256";
pub const MD5_LABEL: &str = "MD5";
pub const ETAG_LABEL: &str = "ETag";

/// "Matches local": the file in the other pane's folder with the same name. The inspector
/// compares with the first two (inspector.slint) for their colour: keep them the same.
pub const MATCHES_YES: &str = "Yes · same checksum";
pub const MATCHES_NO: &str = "No · different";
pub const NO_LOCAL_COPY: &str = "No local copy";
pub const MATCHES_UNKNOWN: &str = "Can't tell · no checksum to compare";
pub const MATCHES_UNREADABLE: &str = "Can't tell · the local file can't be read";

/// The public link's note: "Public · Pull Zone 7654321", and how many other hostnames serve it.
pub fn public_zone_note(zone: u64, others: usize) -> String {
    match others {
        0 => format!("Public · Pull Zone {zone}"),
        1 => format!("Public · Pull Zone {zone} · 1 more hostname"),
        n => format!("Public · Pull Zone {zone} · {n} more hostnames"),
    }
}
/// The public link comes from the site's public base URL.
pub const PUBLIC_FROM_SETTINGS: &str = "Public · from Settings › Sites";
/// Neither a pull zone nor a public base URL says where the file is served.
pub const NO_PUBLIC_URL: &str = "Add a public URL in Settings › Sites";
/// The storage link's note: it opens only with the site's secret.
pub const STORAGE_NOTE_BUNNY: &str = "Storage API · needs password";
pub const STORAGE_NOTE_S3: &str = "S3 API · needs your access keys";
/// Why Purge from CDN is off for a site that isn't Bunny.
pub const PURGE_NEEDS_BUNNY: &str = "Purging needs a Bunny pull zone.";

/// Copy Public Link / Copy Path / the inspector's copy buttons put this many on the clipboard.
pub fn copied_links(n: usize) -> String {
    match n {
        1 => "Copied the link.".to_owned(),
        n => format!("Copied {} links.", crate::format::format_count(n as u64)),
    }
}
pub fn copied_paths(n: usize) -> String {
    match n {
        1 => "Copied the path.".to_owned(),
        n => format!("Copied {} paths.", crate::format::format_count(n as u64)),
    }
}
/// Copy Public Link on a site that has no public address.
pub fn no_public_url(site: &str) -> String {
    format!("{site} has no public address yet. Add a public URL in Settings › Sites.")
}
/// The clipboard couldn't be written.
pub fn not_copied(why: &str) -> String {
    format!("Couldn't copy to the clipboard: {why}")
}
/// A copy or purge with nothing selected.
pub const SELECT_FIRST: &str = "Select a file or folder first.";

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

/// The banner over the cloud pane while a read-only site is open.
pub fn read_only_banner(site: &str) -> String {
    format!("{site} is read-only. Uploads, deletes and edits are off.")
}

/// A write to a read-only site was asked for anyway (a drop, a paste, a publish): nothing went.
pub fn read_only_refused(site: &str) -> String {
    format!("Nothing was changed: {site} is read-only. Turn that off in Settings › Sites.")
}

/// Settings › General's preview cache: what it holds.
pub fn preview_cache(bytes: u64) -> String {
    if bytes == 0 {
        "No previews or thumbnails are kept.".to_owned()
    } else {
        format!(
            "{} of previews and thumbnails on this PC.",
            crate::format::format_size(bytes)
        )
    }
}

/// The preview cache could not be emptied.
pub fn cache_not_cleared(reason: impl std::fmt::Display) -> String {
    format!("Couldn't clear the cache: {reason}")
}

/// Settings › Advanced's checksum cache: how many files it remembers.
pub fn hash_cache(files: u64) -> String {
    match files {
        0 => "No files remembered yet.".to_owned(),
        1 => "1 file remembered.".to_owned(),
        n => format!("{} files remembered.", crate::format::format_count(n)),
    }
}

/// The checksum cache could not be read or emptied.
pub fn hash_cache_failed(reason: impl std::fmt::Display) -> String {
    format!("Couldn't reach the checksum cache: {reason}")
}

/// Open Log Folder failed.
pub fn log_folder_failed(reason: impl std::fmt::Display) -> String {
    format!("Couldn't open the log folder: {reason}")
}

/// Settings › Sites: the CDN of a Bunny site with a pull zone linked, "Pull Zone 7654321 · key
/// saved".
pub fn cdn_linked(zones: &[u64], key_saved: bool) -> String {
    let ids: Vec<String> = zones.iter().map(u64::to_string).collect();
    let zones = match ids.len() {
        1 => format!("Pull Zone {}", ids[0]),
        _ => format!("Pull Zones {}", ids.join(", ")),
    };
    if key_saved {
        format!("{zones} · key saved")
    } else {
        format!("{zones} · no account key kept")
    }
}

/// Settings › Sites: a Bunny site without a pull zone linked.
pub const NO_PULL_ZONE: &str = "No pull zone linked";

/// A conflict choice remembered for a site, as Settings › Sites names it.
pub fn conflict_choice(policy: httporter_core::ConflictPolicy) -> &'static str {
    use httporter_core::ConflictPolicy;
    match policy {
        ConflictPolicy::Ask => "Ask once per batch",
        ConflictPolicy::ReplaceIfDifferent => "Replace if different",
        ConflictPolicy::Skip => "Skip",
        ConflictPolicy::Replace => "Replace all",
        ConflictPolicy::KeepBoth => "Keep both",
    }
}

/// The public base URL is not an `https://…/` address.
pub const PUBLIC_URL_INVALID: &str =
    "Enter an address that starts with https://, such as https://cdn.example.com/.";

/// A site's settings could not be saved, or a remembered choice forgotten.
pub fn site_settings_not_saved(reason: impl std::fmt::Display) -> String {
    format!("Couldn't save: {reason}")
}

/// Export Sites… and Import Sites…: the pickers.
pub const EXPORT_SITES_TITLE: &str = "Export Sites";
pub const IMPORT_SITES_TITLE: &str = "Import Sites";
pub const SITES_FILE_TYPE: &str = "HTTPorter sites";
pub const SITES_FILE_NAME: &str = "httporter-sites.json";

/// Export Sites… wrote `n` sites to `file`.
pub fn sites_exported(n: usize, file: &str) -> String {
    let sites = if n == 1 {
        "1 site".to_owned()
    } else {
        format!("{n} sites")
    };
    format!("Exported {sites} to {file}. The file holds no passwords or keys.")
}

pub fn sites_not_exported(reason: impl std::fmt::Display) -> String {
    format!("Couldn't export the sites: {reason}")
}

pub fn sites_not_imported(reason: impl std::fmt::Display) -> String {
    format!("Couldn't import the sites: {reason}")
}

/// What Import Sites… did: "Added 2 sites. Enter the password for example-shop.com in Edit Site."
pub fn sites_imported(added: usize, skipped: usize, need_secret: &[String]) -> String {
    let mut text = match added {
        0 => "No sites were added.".to_owned(),
        1 => "Added 1 site.".to_owned(),
        n => format!("Added {n} sites."),
    };
    match skipped {
        0 => {}
        1 => text.push_str(" 1 was saved already."),
        n => text.push_str(&format!(" {n} were saved already.")),
    }
    match need_secret {
        [] => {}
        [one] => text.push_str(&format!(" Enter the password for {one} in Edit Site.")),
        [first @ .., last] => text.push_str(&format!(
            " Enter the passwords for {} and {last} in Edit Site.",
            first.join(", ")
        )),
    }
    text
}

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
