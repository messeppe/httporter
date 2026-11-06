// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reset HTTPorter (§11): confirming writes the marker `%APPDATA%\HTTPorter\reset-pending`,
//! starts `httporter.exe --after-reset` and quits; the next start wipes everything HTTPorter
//! keeps on this PC before anything opens it.
//!
//! The wipe, in this order (each step only once the one before it worked):
//! 1. take the single-instance mutex (`Local\HTTPorter.Running`), waiting ≤ 10 s for the
//!    HTTPorter that asked for the reset to let go of it (only with `--after-reset`; a plain
//!    start doesn't wait, and leaves the reset to the instance that is running). Holding it means
//!    no second HTTPorter starts, or wipes, meanwhile;
//! 2. delete every credential [`CredentialStore::keys`] lists (HTTPorter's own targets only);
//! 3. delete what is in `%APPDATA%\HTTPorter` (the database, the logs) and
//!    `%LOCALAPPDATA%\HTTPorter` (the cache), except the marker and the running program;
//! 4. remove the marker, last.
//!
//! On any error the marker stays, and the next start tries again.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use httporter_platform::{CredentialStore, KeyringStore, cache_dir, default_db_path};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

use super::copy;
use crate::single_instance::MUTEX_NAME;

/// The marker's name, in `%APPDATA%\HTTPorter`.
const MARKER: &str = "reset-pending";
/// The argument the relaunch after confirming gets: wait for the instance that quits.
pub const AFTER_RESET: &str = "--after-reset";
/// How long the relaunch waits for the instance that asked for the reset to quit.
const WAIT: Duration = Duration::from_secs(10);
/// How often it looks while waiting.
const POLL: Duration = Duration::from_millis(100);

/// Why the reset at start stopped, for Settings › Security and the activity log.
static FAILED: OnceLock<String> = OnceLock::new();

/// The folders a reset empties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Profile {
    /// `%APPDATA%\HTTPorter`: the database, the logs, and the marker.
    roaming: PathBuf,
    /// `%LOCALAPPDATA%\HTTPorter`: the cache.
    local: PathBuf,
}

impl Profile {
    /// This user's folders, as the store and the cache name them. None when Windows didn't say
    /// where they are: the paths would then be relative, and a reset could empty the wrong
    /// folder.
    pub(super) fn of_this_user() -> Option<Self> {
        let roaming = default_db_path().parent()?.to_path_buf();
        let local = cache_dir("reset").parent()?.parent()?.to_path_buf();
        Self::new(roaming, local)
    }

    /// The two folders, if each is an absolute path named `HTTPorter` inside a folder of its own,
    /// and they are not the same folder.
    fn new(roaming: PathBuf, local: PathBuf) -> Option<Self> {
        let ours = |dir: &Path| {
            dir.is_absolute()
                && dir.file_name() == Some(std::ffi::OsStr::new("HTTPorter"))
                && dir.parent().and_then(Path::parent).is_some()
        };
        (ours(&roaming) && ours(&local) && roaming != local).then_some(Self { roaming, local })
    }

    fn marker(&self) -> PathBuf {
        self.roaming.join(MARKER)
    }
}

/// What [`reset`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// There was no marker: nothing to do.
    NoMarker,
    /// Another HTTPorter holds the mutex (still quitting, or running): it is left for later.
    Busy,
    /// Everything is gone, the marker last.
    Done,
}

/// The wipe the marker asks for, before the store opens: see the module docs. Without a marker
/// it does nothing.
pub fn run_pending_reset() {
    let Some(profile) = Profile::of_this_user() else {
        return;
    };
    // The common case, without a look at the arguments.
    if !exists(&profile.marker()) {
        return;
    }
    let wait = if std::env::args_os().any(|arg| arg == AFTER_RESET) {
        WAIT
    } else {
        Duration::ZERO
    };
    let keep: Vec<PathBuf> = std::env::current_exe().into_iter().collect();
    if let Err(e) = reset(&profile, &KeyringStore, MUTEX_NAME, wait, &keep) {
        let _ = FAILED.set(e);
    }
}

/// Why the reset at this start stopped, if it did.
pub(super) fn failure() -> Option<&'static str> {
    FAILED.get().map(String::as_str)
}

/// Writes the marker, so that the next start resets.
pub(super) fn schedule() -> Result<(), String> {
    let profile = Profile::of_this_user().ok_or_else(|| copy::RESET_NO_PROFILE.to_owned())?;
    std::fs::create_dir_all(&profile.roaming).map_err(|e| e.to_string())?;
    std::fs::write(profile.marker(), b"HTTPorter resets at its next start.\r\n")
        .map_err(|e| e.to_string())
}

/// Starts this program again with [`AFTER_RESET`], to reset once this one has quit.
pub(super) fn restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg(AFTER_RESET)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// The wipe of `profile` and of `creds`' keys, holding the mutex `mutex` (waiting `wait` at
/// most for it). Nothing in `keep` (or a folder that holds it) is deleted. The error says what
/// stopped it; the marker then stays.
fn reset(
    profile: &Profile,
    creds: &dyn CredentialStore,
    mutex: &str,
    wait: Duration,
    keep: &[PathBuf],
) -> Result<Outcome, String> {
    let marker = profile.marker();
    if !exists(&marker) {
        return Ok(Outcome::NoMarker);
    }
    let Some(_held) = Held::take(mutex, wait) else {
        return Ok(Outcome::Busy);
    };
    // Another start may have finished it while this one waited.
    if !exists(&marker) {
        return Ok(Outcome::NoMarker);
    }
    let keys = creds.keys().map_err(|e| e.to_string())?;
    for key in &keys {
        creds.delete(key).map_err(|e| e.to_string())?;
    }
    let mut kept = keep.to_vec();
    kept.push(marker.clone());
    empty(&profile.roaming, &kept)?;
    empty(&profile.local, keep)?;
    remove_if_empty(&profile.local);
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(copy::reset_entry_failed(&marker, e)),
    }
    remove_if_empty(&profile.roaming);
    Ok(Outcome::Done)
}

/// Whether `path` is there (a broken link counts).
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Deletes everything in `dir` except `keep` and the folders that hold something of it. A
/// missing `dir` is empty already. Links are removed, never followed.
fn empty(dir: &Path, keep: &[PathBuf]) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(copy::reset_entry_failed(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| copy::reset_entry_failed(dir, e))?;
        let path = entry.path();
        if keep.iter().any(|kept| holds(&path, kept)) {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|e| copy::reset_entry_failed(&path, e))?;
        let removed = if kind.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            // A file, or a link: a link to a folder goes with `remove_dir`.
            std::fs::remove_file(&path).or_else(|e| {
                if kind.is_symlink() {
                    std::fs::remove_dir(&path)
                } else {
                    Err(e)
                }
            })
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(copy::reset_entry_failed(&path, e)),
        }
    }
    Ok(())
}

/// Whether `path` is `kept` or a folder that holds it, ignoring case as Windows does.
fn holds(path: &Path, kept: &Path) -> bool {
    let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
    lower(kept).starts_with(lower(path))
}

/// Removes `dir` if nothing is left in it (something kept stays, and so does the folder).
fn remove_if_empty(dir: &Path) {
    let _ = std::fs::remove_dir(dir);
}

/// The single-instance mutex, held by this process until dropped.
struct Held(HANDLE);

impl Held {
    /// Opens the mutex `name` as its first holder, trying until `wait` has passed. None when
    /// another process still has it open (or Windows can't make it).
    fn take(name: &str, wait: Duration) -> Option<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: a NUL-terminated name that outlives the call; no security attributes.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
            if handle.is_null() {
                return None;
            }
            // SAFETY: a plain call, right after the one whose error it reads.
            if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                return Some(Self(handle));
            }
            // SAFETY: the handle was just opened and is not used again.
            unsafe { CloseHandle(handle) };
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: the handle `take` opened, closed once. The app takes the mutex again itself
        // (`single_instance::first`), right after the reset.
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httporter_core::{AccountId, SiteId};
    use httporter_platform::{CredentialKey, MemoryStore};

    /// A profile in a temporary folder, and a mutex name of its own, so that the test never
    /// touches the real profile, Credential Manager or the running HTTPorter's mutex.
    fn temp_profile() -> (tempfile::TempDir, Profile, String) {
        let temp = tempfile::tempdir().unwrap();
        let roaming = temp.path().join("Roaming").join("HTTPorter");
        let local = temp.path().join("Local").join("HTTPorter");
        let profile = Profile::new(roaming, local).unwrap();
        let mutex = format!(
            "Local\\HTTPorter.ResetTest.{}.{}",
            std::process::id(),
            SiteId::new().0
        );
        (temp, profile, mutex)
    }

    fn fill(profile: &Profile, creds: &MemoryStore) -> CredentialKey {
        std::fs::create_dir_all(profile.roaming.join("logs")).unwrap();
        std::fs::write(profile.roaming.join("httporter.db"), b"db").unwrap();
        std::fs::write(profile.roaming.join("logs").join("activity.log"), b"log").unwrap();
        std::fs::create_dir_all(profile.local.join("cache").join("thumbs")).unwrap();
        std::fs::write(
            profile.local.join("cache").join("thumbs").join("a.png"),
            b"",
        )
        .unwrap();
        let site = CredentialKey::site(&SiteId::new());
        creds.set(&site, "pw").unwrap();
        creds
            .set(&CredentialKey::account(&AccountId::new()), "key")
            .unwrap();
        site
    }

    #[test]
    fn a_profile_must_be_two_absolute_httporter_folders() {
        let base = std::env::temp_dir();
        let ok = Profile::new(
            base.join("Roaming").join("HTTPorter"),
            base.join("Local").join("HTTPorter"),
        );
        assert!(ok.is_some());
        for (roaming, local) in [
            (
                PathBuf::from("HTTPorter"),
                base.join("Local").join("HTTPorter"),
            ),
            (
                base.join("Roaming").join("Other"),
                base.join("Local").join("HTTPorter"),
            ),
            (base.join("HTTPorter"), base.join("HTTPorter")),
        ] {
            assert_eq!(Profile::new(roaming, local), None);
        }
    }

    #[test]
    fn without_a_marker_nothing_is_touched() {
        let (_temp, profile, mutex) = temp_profile();
        let creds = MemoryStore::default();
        let site = fill(&profile, &creds);
        let outcome = reset(&profile, &creds, &mutex, Duration::ZERO, &[]).unwrap();
        assert_eq!(outcome, Outcome::NoMarker);
        assert!(profile.roaming.join("httporter.db").exists());
        assert!(creds.get(&site).unwrap().is_some());
    }

    #[test]
    fn the_marker_wipes_the_keys_and_both_folders_marker_last() {
        let (_temp, profile, mutex) = temp_profile();
        let creds = MemoryStore::default();
        fill(&profile, &creds);
        std::fs::write(profile.marker(), b"").unwrap();
        // The running program, if it sat in a profile folder, is kept.
        let exe = profile.local.join("app").join("httporter.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"exe").unwrap();

        let outcome = reset(
            &profile,
            &creds,
            &mutex,
            Duration::ZERO,
            std::slice::from_ref(&exe),
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Done);
        assert!(creds.keys().unwrap().is_empty());
        assert!(!profile.roaming.exists());
        assert!(!profile.local.join("cache").exists());
        assert!(exe.exists());
        // Done once: a second start finds no marker.
        let again = reset(&profile, &creds, &mutex, Duration::ZERO, &[]).unwrap();
        assert_eq!(again, Outcome::NoMarker);
    }

    #[test]
    fn while_another_instance_holds_the_mutex_nothing_is_touched() {
        let (_temp, profile, mutex) = temp_profile();
        let creds = MemoryStore::default();
        let site = fill(&profile, &creds);
        std::fs::write(profile.marker(), b"").unwrap();
        let held = Held::take(&mutex, Duration::ZERO).unwrap();
        let started = Instant::now();
        let wait = Duration::from_millis(300);
        let outcome = reset(&profile, &creds, &mutex, wait, &[]).unwrap();
        assert_eq!(outcome, Outcome::Busy);
        assert!(started.elapsed() >= wait);
        assert!(profile.marker().exists());
        assert!(profile.roaming.join("httporter.db").exists());
        assert!(creds.get(&site).unwrap().is_some());
        // Once it lets go, the next start resets.
        drop(held);
        let outcome = reset(&profile, &creds, &mutex, Duration::ZERO, &[]).unwrap();
        assert_eq!(outcome, Outcome::Done);
    }

    #[test]
    fn an_error_keeps_the_marker_for_the_next_start() {
        use std::os::windows::fs::OpenOptionsExt;

        let (_temp, profile, mutex) = temp_profile();
        let creds = MemoryStore::default();
        fill(&profile, &creds);
        std::fs::write(profile.marker(), b"").unwrap();
        // A file another program has open without sharing: Windows won't delete it.
        let stuck = profile.roaming.join("stuck.txt");
        std::fs::write(&stuck, b"").unwrap();
        let open = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&stuck)
            .unwrap();

        let result = reset(&profile, &creds, &mutex, Duration::ZERO, &[]);
        assert!(result.is_err(), "{result:?}");
        assert!(profile.marker().exists(), "the marker stays after an error");

        // The next start, once the file is let go, finishes it.
        drop(open);
        let outcome = reset(&profile, &creds, &mutex, Duration::ZERO, &[]).unwrap();
        assert_eq!(outcome, Outcome::Done);
        assert!(!profile.marker().exists());
        assert!(!profile.roaming.exists());
    }
}
