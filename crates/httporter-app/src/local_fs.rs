// SPDX-License-Identifier: MIT OR Apache-2.0

//! Listing of a local folder and of the PC's drive letters for the "This PC" pane.

use chrono::{DateTime, Local, NaiveDateTime};
use std::fs::DirEntry;
use std::io;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

use crate::sort::Sortable;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalEntry {
    pub name: String,
    pub is_folder: bool,
    pub size: u64,
    pub modified: Option<NaiveDateTime>,
}

impl Sortable for LocalEntry {
    fn is_folder(&self) -> bool {
        self.is_folder
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn modified(&self) -> Option<NaiveDateTime> {
        self.modified
    }
}

#[derive(Debug)]
pub struct LocalListing {
    pub entries: Vec<LocalEntry>,
    /// Entries whose metadata could not be read; they are left out rather than failing the listing.
    pub skipped: usize,
}

/// Lists `dir` (unsorted). Failing to open `dir` itself is an error; an entry whose metadata cannot
/// be read is counted in `skipped`. A link to a folder (a junction or symlink) is listed as a folder;
/// a link whose target cannot be read is skipped.
pub fn list_local(dir: &Path) -> io::Result<LocalListing> {
    let mut entries = Vec::new();
    let mut skipped = 0;
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else {
            // The directory stream itself failed, so there is nothing sensible to read next.
            skipped += 1;
            break;
        };
        match read_entry(&entry) {
            Ok(e) => entries.push(e),
            Err(_) => skipped += 1,
        }
    }
    Ok(LocalListing { entries, skipped })
}

fn read_entry(entry: &DirEntry) -> io::Result<LocalEntry> {
    let mut meta = entry.metadata()?;
    if meta.file_type().is_symlink() {
        // `DirEntry::metadata` describes the link itself; the pane wants what it points at.
        meta = std::fs::metadata(entry.path())?;
    }
    let is_folder = meta.is_dir();
    Ok(LocalEntry {
        name: entry.file_name().to_string_lossy().into_owned(),
        is_folder,
        size: if is_folder { 0 } else { meta.len() },
        modified: meta
            .modified()
            .ok()
            .map(|t| DateTime::<Local>::from(t).naive_local()),
    })
}

/// The PC's drive roots (`C:\`, `D:\`, ...), in letter order.
pub fn list_drives() -> Vec<PathBuf> {
    // SAFETY: `GetLogicalDrives` takes no arguments and only reads system state.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|&i| mask & (1u32 << i) != 0)
        .map(|i| PathBuf::from(format!("{}:\\", char::from(b'A' + i))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_temp_folder() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"abc").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let listing = list_local(tmp.path()).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.skipped, 0);

        let a = listing.entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert!(!a.is_folder);
        assert_eq!(a.size, 3);
        let modified = a
            .modified
            .expect("a freshly written file has a modified time");
        let age = chrono::Local::now().naive_local() - modified;
        assert!(
            age.num_seconds().abs() < 300,
            "modified is in local time: {modified}"
        );

        let sub = listing.entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_folder);
        assert_eq!(sub.size, 0);
    }

    #[test]
    fn empty_folder_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let listing = list_local(tmp.path()).unwrap();
        assert!(listing.entries.is_empty());
        assert_eq!(listing.skipped, 0);
    }

    #[test]
    fn missing_folder_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list_local(&tmp.path().join("nope")).is_err());
    }

    /// Creates the junction `link` -> `target` (no elevation needed, unlike a symlink).
    fn make_junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed");
    }

    #[test]
    fn junction_to_a_folder_lists_as_a_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        make_junction(&sub.join("link"), &target);

        let listing = list_local(&sub).unwrap();
        assert_eq!(listing.skipped, 0);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "link");
        assert!(listing.entries[0].is_folder);
    }

    #[test]
    fn unreadable_entry_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("gone");
        std::fs::create_dir(&target).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("ok.txt"), b"x").unwrap();
        make_junction(&sub.join("dangling"), &target);
        std::fs::remove_dir(&target).unwrap();

        let listing = list_local(&sub).unwrap();
        assert_eq!(listing.skipped, 1);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "ok.txt");
    }

    #[test]
    fn drives_include_c() {
        assert!(list_drives().iter().any(|d| d.as_os_str() == "C:\\"));
    }
}
