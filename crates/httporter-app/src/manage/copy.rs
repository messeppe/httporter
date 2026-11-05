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

//
//
//
//
//
