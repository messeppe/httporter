//! The words the core writes itself (a CDN purge's results), kept in one place so the app's copy
//! layer can replace them.

/// Some of a purge's requests failed: `{failed}` of `{total}`, then the first one's `{error}`.
pub const PURGE_FAILED: &str = "{failed} of {total} failed: {error}";

/// [`PURGE_FAILED`] filled in.
pub fn purge_failed(failed: usize, total: usize, error: &str) -> String {
    PURGE_FAILED
        .replace("{failed}", &failed.to_string())
        .replace("{total}", &total.to_string())
        .replace("{error}", error)
}

/// A purge found no pull zone to purge.
pub const NO_PULL_ZONE: &str = "No pull zone serves this storage zone.";

/// The provider can't copy a file on the server (I4 moves then download and upload again).
pub const NO_COPY: &str = "This storage can't copy files on the server.";

/// The service refused a server-side copy because the file is larger than one copy request may
/// take (S3: 5 GiB).
pub const COPY_TOO_LARGE: &str =
    "This file is too large for the storage to copy on the server (the limit is 5 GB).";

/// The provider can't make a pre-signed link.
pub const NO_PRESIGN: &str = "This storage can't make pre-signed links.";

/// A file given to Import Sites is not a sites file (I4, T4).
pub const SITES_WRONG_FORMAT: &str = "This file isn't an HTTPorter sites file.";

/// A sites file written by a newer HTTPorter (I4, T4).
pub const SITES_NEWER_VERSION: &str =
    "This sites file is from a newer version of HTTPorter. Update HTTPorter to import it.";
