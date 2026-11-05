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
