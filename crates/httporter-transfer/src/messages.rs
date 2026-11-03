//! The words the engine writes itself, kept in one place so the app's copy layer can replace
//! them (Task 23).
//!
//! Two kinds of text are not here: a site's errors, which the engine turns into words with the
//! `describe` function the app passes to `Engine::new`, and the system's own message about a
//! local file (a locked file, a missing folder), which is kept as Windows words it.

use std::io;
use std::path::Path;

/// A unit of work panicked.
pub const STOPPED_UNEXPECTEDLY: &str = "the transfer stopped unexpectedly";

/// Hashing a local file stopped before it finished (its blocking task panicked).
pub const HASHING_STOPPED: &str = "hashing stopped";

/// Other work on a local file (looking it up, writing, renaming) stopped before it finished.
pub const FILE_WORK_STOPPED: &str = "work on the local file stopped";

/// A `delete_batch` answer left this path out.
pub const NO_ANSWER_FOR_FILE: &str = "the server did not answer for this file";

/// A file failed its checksum twice: once, then again after a re-hash and a retry.
pub const DAMAGED_TWICE: &str = "The file arrived damaged twice (checksum mismatch).";

/// A local file can't be read or written: `{path}`, then the system's `{error}`.
pub const LOCAL_FILE: &str = "{path}: {error}";

/// [`LOCAL_FILE`] filled in.
pub fn local_file(path: &Path, error: &io::Error) -> String {
    LOCAL_FILE
        .replace("{path}", &path.display().to_string())
        .replace("{error}", &error.to_string())
}

/// The site refused one file while it still lets its folder be listed: a key or a bucket policy
/// that allows some paths only (review I5).
pub const ACCESS_DENIED: &str = "access to this file was denied";
