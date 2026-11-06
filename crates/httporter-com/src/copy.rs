// SPDX-License-Identifier: MIT OR Apache-2.0

//! Every word `httporter.com` prints itself. Everything else it shows comes from `httporter.exe`.

use std::path::Path;

/// `httporter.exe` is not beside this program.
pub fn exe_missing(exe: &Path) -> String {
    format!(
        "httporter: {} is missing. httporter.com runs it, so it must be in the same folder. \
         Reinstall HTTPorter to fix this.",
        exe.display()
    )
}

/// `httporter.exe` is there but Windows wouldn't start it.
pub fn exe_not_started(exe: &Path, error: &std::io::Error) -> String {
    format!("httporter: couldn't start {}: {error}", exe.display())
}

/// Windows wouldn't say where this program is, so its `httporter.exe` can't be found.
pub fn no_own_path() -> String {
    "httporter: couldn't find the folder httporter.com is in, so httporter.exe can't be started."
        .to_owned()
}

/// Waiting for `httporter.exe` failed (it still may be running).
pub fn wait_failed(error: &std::io::Error) -> String {
    format!("httporter: lost track of httporter.exe: {error}")
}
