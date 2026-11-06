// SPDX-License-Identifier: MIT OR Apache-2.0

//! Local file digests, both from one read.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use md5::Md5;
use sha2::{Digest, Sha256};

/// How much of a file is read at a time.
const READ_SIZE: usize = 256 * 1024;

/// A local file's digests, and the size and modification time they belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHashes {
    pub sha256: [u8; 32],
    pub md5: [u8; 16],
    /// The bytes read, which the digests cover.
    pub size: u64,
    pub modified: SystemTime,
}

/// Where local digests are kept across restarts (the app's site database). The
/// [`crate::HashCache`] asks it after its memory, and tells it what it hashed.
pub trait HashStore: Send + Sync {
    /// The digests kept for `path` at this size and modification time, if any.
    fn load(&self, path: &Path, size: u64, modified: SystemTime) -> Option<FileHashes>;
    /// Keeps `h` for `path`.
    fn save(&self, path: &Path, h: &FileHashes);
}

/// Reads `path` once and computes its SHA-256 and MD5 together. Paths longer than Windows'
/// classic limit are opened through the `\\?\` form.
pub fn hash_file(path: &Path) -> io::Result<FileHashes> {
    let mut file = File::open(extended(path))?;
    let modified = file.metadata()?.modified()?;
    let mut sha256 = Sha256::new();
    let mut md5 = Md5::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; READ_SIZE];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        sha256.update(&buf[..n]);
        md5.update(&buf[..n]);
        size += n as u64;
    }
    Ok(FileHashes {
        sha256: sha256.finalize().into(),
        md5: md5.finalize().into(),
        size,
        modified,
    })
}

/// The ETag S3 gives a file uploaded in parts of `part_size` bytes: the MD5 of the parts' MD5s,
/// then `-` and the number of parts, in lower-case hex. Reads the file once.
pub(crate) fn multipart_etag(path: &Path, part_size: u64) -> io::Result<String> {
    let part_size = part_size.max(1);
    let mut file = File::open(extended(path))?;
    let mut digests: Vec<u8> = Vec::new();
    let mut part = Md5::new();
    let mut in_part = 0u64;
    let mut buf = vec![0u8; READ_SIZE];
    loop {
        let want =
            usize::try_from((part_size - in_part).min(READ_SIZE as u64)).unwrap_or(READ_SIZE);
        let n = match file.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        part.update(&buf[..n]);
        in_part += n as u64;
        if in_part == part_size {
            digests.extend_from_slice(&part.finalize_reset());
            in_part = 0;
        }
    }
    if in_part > 0 || digests.is_empty() {
        digests.extend_from_slice(&part.finalize());
    }
    let parts = digests.len() / 16;
    Ok(format!("{}-{parts}", hex::encode(Md5::digest(&digests))))
}

/// Paths at least this long get the `\\?\` prefix (the classic limit is 260, and a folder's
/// path must leave room for an 8.3 name).
#[cfg(windows)]
const LONG_PATH: usize = 248;

/// `path` in the `\\?\` (or `\\?\UNC\`) form when it is a long absolute path on Windows, so it
/// opens whatever the length; any other path as it is.
pub(crate) fn extended(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use std::path::Component;

        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        let already = wide.starts_with(&[92, 92, 63, 92]); // \\?\
        let plain = path
            .components()
            .all(|c| !matches!(c, Component::CurDir | Component::ParentDir));
        if wide.len() < LONG_PATH || already || !path.is_absolute() || !plain {
            return path.to_owned();
        }
        // A `\\?\` path is taken literally: only backslashes separate.
        let wide: Vec<u16> = wide
            .into_iter()
            .map(|c| {
                if c == u16::from(b'/') {
                    u16::from(b'\\')
                } else {
                    c
                }
            })
            .collect();
        let mut out: Vec<u16> = Vec::with_capacity(wide.len() + 8);
        if wide.starts_with(&[92, 92]) {
            out.extend(r"\\?\UNC\".encode_utf16());
            out.extend_from_slice(&wide[2..]);
        } else {
            out.extend(r"\\?\".encode_utf16());
            out.extend_from_slice(&wide);
        }
        PathBuf::from(OsString::from_wide(&out))
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn long_absolute_paths_get_the_verbatim_prefix() {
        let short = Path::new(r"C:\site\index.html");
        assert_eq!(extended(short), short);

        let long = format!(r"C:\site\{}\index.html", "d".repeat(300));
        assert_eq!(
            extended(Path::new(&long)),
            PathBuf::from(format!(r"\\?\{long}"))
        );

        let slashes = format!("C:/site/{}/index.html", "d".repeat(300));
        assert_eq!(
            extended(Path::new(&slashes)),
            PathBuf::from(format!(r"\\?\C:\site\{}\index.html", "d".repeat(300)))
        );

        let unc = format!(r"\\server\share\{}", "d".repeat(300));
        assert_eq!(
            extended(Path::new(&unc)),
            PathBuf::from(format!(r"\\?\UNC\server\share\{}", "d".repeat(300)))
        );

        let verbatim = format!(r"\\?\C:\{}", "d".repeat(300));
        assert_eq!(extended(Path::new(&verbatim)), Path::new(&verbatim));

        // Relative paths and `..` can't be made verbatim; they are left to the system.
        let relative = format!(r"site\{}", "d".repeat(300));
        assert_eq!(extended(Path::new(&relative)), Path::new(&relative));
        let dotted = format!(r"C:\site\..\{}", "d".repeat(300));
        assert_eq!(extended(Path::new(&dotted)), Path::new(&dotted));
    }
}
