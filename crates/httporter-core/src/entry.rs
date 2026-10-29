use chrono::{DateTime, Utc};

use crate::{ProviderError, RemotePath};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChecksumKind {
    Sha256,
    Md5,
}

/// A content hash as reported by the provider; `hex` is always lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub kind: ChecksumKind,
    pub hex: String,
}

impl Checksum {
    /// Accepts a SHA-256 digest as 64 hex characters in either case and stores
    /// it lowercase.
    pub fn sha256_hex(hex: &str) -> Result<Self, ProviderError> {
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self {
                kind: ChecksumKind::Sha256,
                hex: hex.to_ascii_lowercase(),
            })
        } else {
            Err(ProviderError::Permanent(
                "a SHA-256 checksum must be 64 hexadecimal characters".into(),
            ))
        }
    }
}

/// One file or folder in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: RemotePath,
    pub is_folder: bool,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub checksum: Option<Checksum>,
    pub content_type: Option<String>,
    pub replicated_regions: Vec<String>,
}

impl Entry {
    /// The last path segment (empty for the root).
    pub fn name(&self) -> &str {
        self.path.name().unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RemotePath;

    #[test]
    fn checksum_normalizes_and_validates() {
        let c = Checksum::sha256_hex(
            "B952DEDC8ACB32567C6A1409077036C7D0F371C85A4D75BCD667E4740B75F991",
        )
        .unwrap();
        assert_eq!(c.kind, ChecksumKind::Sha256);
        assert_eq!(
            c.hex,
            "b952dedc8acb32567c6a1409077036c7d0f371c85a4d75bcd667e4740b75f991"
        );
        assert!(Checksum::sha256_hex("abc").is_err());
    }

    #[test]
    fn checksum_rejects_non_hex_of_the_right_length() {
        let not_hex = "g".repeat(64);
        assert!(Checksum::sha256_hex(&not_hex).is_err());
        assert!(Checksum::sha256_hex("").is_err());
        assert!(Checksum::sha256_hex(&"a".repeat(65)).is_err());
    }

    #[test]
    fn entry_name_is_the_last_path_segment() {
        let entry = |path: &str| Entry {
            path: RemotePath::parse(path).unwrap(),
            is_folder: false,
            size: 0,
            modified: None,
            checksum: None,
            content_type: None,
            replicated_regions: Vec::new(),
        };
        assert_eq!(entry("a/b/c.txt").name(), "c.txt");
        assert_eq!(entry("top.txt").name(), "top.txt");
        assert_eq!(entry("").name(), "");
    }
}
