// SPDX-License-Identifier: MIT OR Apache-2.0

//! Opening files and links with other programs, and the monospaced font.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use windows::Win32::UI::Shell::{
    OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OAIF_REGISTER_EXT, OPENASINFO, SHOpenWithDialog,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

use super::with_com;

/// `text` as NUL-terminated UTF-16.
fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// Runs the shell's default verb on `target` (a file or a URL).
fn shell_open(target: &OsStr) -> Result<(), String> {
    let target = wide(target);
    let verb = wide(OsStr::new("open"));
    let result = with_com(|| {
        // SAFETY: both strings are NUL-terminated and outlive the call.
        let done = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(target.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        // Above 32 is success; anything else is an error code, with the reason in GetLastError.
        if done.0 as usize > 32 {
            Ok(())
        } else {
            Err(windows::core::Error::from_thread())
        }
    });
    result.map_err(|e| e.message())
}

/// Opens `path` in its default program.
pub fn open_default(path: &Path) -> Result<(), String> {
    shell_open(path.as_os_str())
}

/// Opens `url` in the default browser. Only `http` and `https` links are opened.
pub fn open_url(url: &str) -> Result<(), String> {
    let lower = url.trim_start().to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput).to_string());
    }
    shell_open(OsStr::new(url.trim_start()))
}

/// Shows Windows' Open With dialog for `path` (`SHOpenWithDialog`). Blocks until the dialog
/// closes: call it off the UI thread.
pub fn open_with(path: &Path) -> Result<(), String> {
    let file = wide(path.as_os_str());
    with_com(|| {
        let info = OPENASINFO {
            pcszFile: PCWSTR(file.as_ptr()),
            pcszClass: PCWSTR::null(),
            oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_REGISTER_EXT | OAIF_EXEC,
        };
        // SAFETY: `info` and the name it points to outlive the call.
        unsafe { SHOpenWithDialog(None, &info) }
    })
    .map_err(|e| e.message())
}

/// "Cascadia Mono" if installed, else "Consolas" (ledger 135).
pub fn mono_font_family() -> &'static str {
    static FAMILY: OnceLock<&'static str> = OnceLock::new();
    FAMILY.get_or_init(|| {
        if cascadia_installed() {
            "Cascadia Mono"
        } else {
            "Consolas"
        }
    })
}

/// Whether a `CascadiaMono*.ttf` is in the system's or the user's font folder.
fn cascadia_installed() -> bool {
    let mut dirs = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(Path::new(&windir).join("Fonts"));
    }
    if let Some(local) = dirs::data_local_dir() {
        dirs.push(local.join("Microsoft").join("Windows").join("Fonts"));
    }
    dirs.iter().any(|dir| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                name.starts_with("cascadiamono") && name.ends_with(".ttf")
            })
        })
    })
}
