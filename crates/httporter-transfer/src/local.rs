//! The local file work transfers do, behind a trait so a test can stand in for the disk (a full
//! one, say). The engine makes every call on tokio's blocking threads, never on its workers.

use std::io::{self, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::hash::{FileHashes, extended, hash_file};

/// The local file operations the engine makes. Each has the disk as its default, so a stand-in
/// overrides only what it changes. Calls may block: the engine makes them off the runtime's
/// worker threads.
pub trait LocalFiles: Send + Sync {
    /// A file's size and, where the system keeps it, when it last changed.
    fn stat(&self, path: &Path) -> io::Result<(u64, Option<SystemTime>)> {
        let meta = std::fs::metadata(extended(path))?;
        Ok((meta.len(), meta.modified().ok()))
    }

    /// Reads a file about to be sent and hashes it.
    fn hash(&self, path: &Path) -> io::Result<FileHashes> {
        hash_file(path)
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(extended(dir))
    }

    /// Creates a file to write a download into, emptying one already there.
    fn create(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(std::fs::File::create(extended(path))?))
    }

    /// Moves a finished download onto its target, replacing what is there.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(extended(from), extended(to))
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(extended(path))
    }
}

/// The disk itself: what the engine uses unless told otherwise.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskFiles;

impl LocalFiles for DiskFiles {}

/// Local work that could not be done.
#[derive(Debug)]
pub(crate) enum LocalError {
    /// The system's error.
    Io(io::Error),
    /// The blocking task doing it panicked or was dropped.
    Stopped,
}

/// Runs `f` on a blocking thread and waits for it.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, LocalError>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(LocalError::Io(error)),
        Err(_) => Err(LocalError::Stopped),
    }
}
