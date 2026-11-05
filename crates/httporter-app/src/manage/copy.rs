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

//
//
//
//
//
