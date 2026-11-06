// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::ChecksumKind;

/// How a provider treats folders that contain no files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyFolders {
    /// Empty folders exist natively.
    Supported,
    /// Emulated with zero-byte `name/` marker objects.
    Markers,
    /// They vanish with their last file.
    Unsupported,
    /// Not yet determined for this provider.
    Unknown,
}

/// What a provider can do, so the engine can pick strategies and limits
/// without knowing which provider it is talking to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The checksum kind listings carry, if any.
    pub checksum_in_listing: Option<ChecksumKind>,
    pub server_side_copy: bool,
    /// `delete_folder` removes the whole subtree in one request.
    pub recursive_delete: bool,
    /// Largest number of files one delete request can take (1 = no batching).
    pub batch_delete_max: u32,
    pub empty_folders: EmptyFolders,
    pub list_concurrency: usize,
    pub delete_concurrency: usize,
    pub transfer_concurrency_max: usize,
    /// Pre-signed links can be generated.
    pub presign: bool,
    pub read_only: bool,
    /// An upload cut off part way (the app killed or closed, its connection lost) can still
    /// delete its path later, when the service gets round to ending it, even after a newer
    /// upload of the same path has been answered. Bunny Storage does, up to about a minute after
    /// the cut-off upload's last bytes (ruling Q55). The engine then checks such an upload on
    /// the site once it has been sent again, after a settle delay, and sends it once more if it
    /// has gone. Off for a service whose uploads replace a file whole or not at all (S3).
    pub cut_uploads_delete_late: bool,
}

impl Default for Capabilities {
    /// The most conservative assumptions: nothing optional is supported, and an upload that
    /// was answered stays.
    fn default() -> Self {
        Self {
            checksum_in_listing: None,
            server_side_copy: false,
            recursive_delete: false,
            batch_delete_max: 1,
            empty_folders: EmptyFolders::Unknown,
            list_concurrency: 4,
            delete_concurrency: 4,
            transfer_concurrency_max: 32,
            presign: false,
            read_only: false,
            cut_uploads_delete_late: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_conservative_contract_values() {
        let c = Capabilities::default();
        assert_eq!(c.checksum_in_listing, None);
        assert!(!c.server_side_copy);
        assert!(!c.recursive_delete);
        assert_eq!(c.batch_delete_max, 1);
        assert_eq!(c.empty_folders, EmptyFolders::Unknown);
        assert_eq!(c.list_concurrency, 4);
        assert_eq!(c.delete_concurrency, 4);
        assert_eq!(c.transfer_concurrency_max, 32);
        assert!(!c.presign);
        assert!(!c.read_only);
        assert!(!c.cut_uploads_delete_late);
    }
}
