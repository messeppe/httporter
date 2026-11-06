// SPDX-License-Identifier: MIT OR Apache-2.0

//! Watching a folder for changes: `ReadDirectoryChangesW` on a thread.
//!
//! The folder is opened for overlapped reads; the thread waits on the read's event and on a stop
//! event. Dropping the watcher sets the stop event; the thread then cancels the read
//! (`CancelIoEx`), waits for it to end, closes the handles and returns.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, ERROR_NOTIFY_ENUM_DIR, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::PCWSTR;

/// What the watcher reports: names, writes and sizes, of files and folders.
const FILTER: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE(
    FILE_NOTIFY_CHANGE_FILE_NAME.0
        | FILE_NOTIFY_CHANGE_DIR_NAME.0
        | FILE_NOTIFY_CHANGE_LAST_WRITE.0
        | FILE_NOTIFY_CHANGE_SIZE.0,
);

/// The read buffer, in `u32`s (64 KB): `FILE_NOTIFY_INFORMATION` records are DWORD-aligned.
const BUFFER_WORDS: usize = 16 * 1024;

/// A kernel handle, closed when dropped. Kernel handles may be used from any thread.
struct Owned(HANDLE);

// SAFETY: a kernel handle is a process-wide value; nothing about it is tied to a thread, and the
// calls made on it here (wait, set, cancel) are thread-safe.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module and is closed once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Watches a folder and everything under it until dropped.
pub struct DirWatcher {
    /// Set to stop the thread; shared with it, so it stays open while either needs it.
    stop: Arc<Owned>,
    thread: Option<JoinHandle<()>>,
}

impl DirWatcher {
    /// Calls `on_change` with each file that changes under `dir` (with `dir` itself when the
    /// changes overflowed).
    pub fn start(dir: &Path, on_change: Box<dyn Fn(PathBuf) + Send>) -> io::Result<Self> {
        let name: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `name` is NUL-terminated; the handle is checked and owned below.
        let folder = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(io::Error::from)?;
        let folder = Owned(folder);
        // SAFETY: plain calls; the handles are checked and owned below.
        let io_event = Owned(unsafe { CreateEventW(None, true, false, None) }?);
        let stop = Arc::new(Owned(unsafe { CreateEventW(None, true, false, None) }?));
        let dir = dir.to_path_buf();
        let thread = std::thread::Builder::new()
            .name("dir-watcher".into())
            .spawn({
                let stop = stop.clone();
                move || run(folder, io_event, &stop, &dir, &*on_change)
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Whether the thread still reads changes. It ends by itself when the folder can't be read
    /// any more (deleted, or its drive gone), without a last call to `on_change`.
    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        // SAFETY: the event is open while `self.stop` holds it.
        let _ = unsafe { SetEvent(self.stop.0) };
        if let Some(thread) = self.thread.take()
            // Dropped from its own callback: the thread ends by itself once the callback returns.
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

/// The watcher thread: reads changes until `stop` is set or the folder can't be read any more.
fn run(folder: Owned, io_event: Owned, stop: &Owned, dir: &Path, on_change: &dyn Fn(PathBuf)) {
    let mut buffer = vec![0u32; BUFFER_WORDS];
    let byte_len = (buffer.len() * 4) as u32;
    loop {
        let mut overlapped = OVERLAPPED {
            hEvent: io_event.0,
            ..Default::default()
        };
        // SAFETY: the buffer and `overlapped` live until the read has ended (completed, or
        // cancelled and waited for below).
        let started = unsafe {
            ReadDirectoryChangesW(
                folder.0,
                buffer.as_mut_ptr().cast(),
                byte_len,
                true,
                FILTER,
                None,
                Some(&mut overlapped),
                None,
            )
        };
        if started.is_err() {
            return;
        }
        // SAFETY: both handles are open.
        let woke = unsafe { WaitForMultipleObjects(&[io_event.0, stop.0], false, INFINITE) };
        let mut bytes = 0u32;
        if woke != WAIT_OBJECT_0 {
            // Stopped (or the wait failed): give the read up and wait until it has.
            // SAFETY: the read was started on this handle with this `overlapped`.
            unsafe {
                let _ = CancelIoEx(folder.0, Some(&overlapped));
                let _ = GetOverlappedResult(folder.0, &overlapped, &mut bytes, true);
            }
            return;
        }
        // SAFETY: the read has completed; this only collects its result.
        let done = unsafe { GetOverlappedResult(folder.0, &overlapped, &mut bytes, false) };
        match done {
            Ok(()) if bytes == 0 => on_change(dir.to_path_buf()),
            Ok(()) => report(&buffer, bytes as usize, dir, on_change),
            Err(e) if e.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => {
                on_change(dir.to_path_buf())
            }
            Err(_) => return,
        }
    }
}

/// Calls `on_change` with the path of each record in the first `len` bytes of `buffer`.
fn report(buffer: &[u32], len: usize, dir: &Path, on_change: &dyn Fn(PathBuf)) {
    let bytes = buffer.as_ptr().cast::<u8>();
    let mut offset = 0usize;
    // Each record: NextEntryOffset, Action, FileNameLength (bytes), then the UTF-16 name.
    while offset + 12 <= len {
        // SAFETY: `offset + 12 <= len` and the records are DWORD-aligned within the buffer.
        let (next, name_len) = unsafe {
            let record = bytes.add(offset).cast::<u32>();
            (*record as usize, *record.add(2) as usize)
        };
        let name_start = offset + 12;
        if name_start + name_len > len {
            break;
        }
        // SAFETY: the name lies within the first `len` bytes, at a 2-byte boundary.
        let name = unsafe {
            std::slice::from_raw_parts(bytes.add(name_start).cast::<u16>(), name_len / 2)
        };
        on_change(dir.join(OsString::from_wide(name)));
        if next == 0 {
            break;
        }
        offset += next;
    }
}
