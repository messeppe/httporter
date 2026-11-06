// SPDX-License-Identifier: MIT OR Apache-2.0

//! One publish per profile at a time, across processes: the window, the CLI, a scheduled run and
//! the watch folder all take the profile's named mutex `Local\HTTPorter.Profile.<id>` first.
//!
//! A Windows mutex belongs to the thread that took it, and only that thread may release it. A
//! publish's lock is taken and dropped wherever its task happens to run, so each lock keeps a
//! small thread of its own that takes the mutex, holds it until the lock is dropped, then releases
//! it. A process that ends while holding one (a crash) leaves it abandoned, which the next one to
//! wait takes as free.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use httporter_core::ProfileId;
use windows::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::PCWSTR;

/// The prefix of a profile's mutex name; the profile id follows.
pub const PROFILE_LOCK_PREFIX: &str = "Local\\HTTPorter.Profile.";

/// A profile's publish lock, held until dropped.
pub struct ProfileLock {
    /// Dropping it tells the holding thread to release the mutex.
    release: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ProfileLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProfileLock")
    }
}

impl ProfileLock {
    /// The lock if no other publish of `profile` holds it now; `None` if one does (or Windows
    /// wouldn't make the mutex).
    pub fn try_acquire(profile: &ProfileId) -> Option<ProfileLock> {
        Self::acquire(profile, Duration::ZERO)
    }

    /// The lock, waiting up to `timeout` for another publish of `profile` to let it go; `None`
    /// if it didn't. Blocks the calling thread meanwhile: call it off the UI thread.
    pub fn acquire_wait(profile: &ProfileId, timeout: Duration) -> Option<ProfileLock> {
        Self::acquire(profile, timeout)
    }

    fn acquire(profile: &ProfileId, timeout: Duration) -> Option<ProfileLock> {
        let name: Vec<u16> = format!("{PROFILE_LOCK_PREFIX}{profile}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // Short of INFINITE (u32::MAX), which would wait for ever.
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        let (taken_tx, taken_rx) = mpsc::channel::<bool>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("profile-lock".into())
            .spawn(move || {
                // SAFETY: `name` is NUL-terminated and outlives the call; the handle is closed
                // below, on this thread.
                let Ok(mutex) = (unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) })
                else {
                    let _ = taken_tx.send(false);
                    return;
                };
                // SAFETY: a mutex handle opened above.
                let woke = unsafe { WaitForSingleObject(mutex, millis) };
                let taken = woke == WAIT_OBJECT_0 || woke == WAIT_ABANDONED;
                let _ = taken_tx.send(taken);
                if taken {
                    // Held until the lock is dropped (its sender then hangs up).
                    let _ = release_rx.recv();
                    // SAFETY: this thread took the mutex above, so it may release it.
                    let _ = unsafe { ReleaseMutex(mutex) };
                }
                // SAFETY: opened above, closed once.
                let _ = unsafe { CloseHandle(mutex) };
            })
            .ok()?;
        if taken_rx.recv().unwrap_or(false) {
            Some(ProfileLock {
                release: Some(release_tx),
                thread: Some(thread),
            })
        } else {
            let _ = thread.join();
            None
        }
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
