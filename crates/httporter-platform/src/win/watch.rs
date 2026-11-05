//! Watching a folder for changes (T4): `ReadDirectoryChangesW` on a thread.
#![allow(unused_variables, dead_code)]

use std::io;
use std::path::{Path, PathBuf};

/// Watches a folder and everything under it until dropped.
pub struct DirWatcher {
    /// T4: the thread and the folder handle; Drop stops it.
    _private: (),
}

impl DirWatcher {
    /// Calls `on_change` with each file that changes under `dir` (with `dir` itself when the
    /// changes overflowed).
    pub fn start(dir: &Path, on_change: Box<dyn Fn(PathBuf) + Send>) -> io::Result<Self> {
        todo!("T4")
    }
}
