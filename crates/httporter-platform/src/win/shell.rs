//! Opening files and links with other programs, and the monospaced font (T4).
#![allow(unused_variables, dead_code)]

use std::path::Path;

/// Opens `path` in its default program.
pub fn open_default(path: &Path) -> Result<(), String> {
    todo!("T4")
}

/// Opens `url` in the default browser.
pub fn open_url(url: &str) -> Result<(), String> {
    todo!("T4")
}

/// Shows Windows' Open With dialog for `path` (`SHOpenWithDialog`).
pub fn open_with(path: &Path) -> Result<(), String> {
    todo!("T4")
}

/// "Cascadia Mono" if installed, else "Consolas" (ledger 135).
pub fn mono_font_family() -> &'static str {
    todo!("T4")
}
