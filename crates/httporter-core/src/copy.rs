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
