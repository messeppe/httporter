use std::fmt;

use chrono::{DateTime, Utc};

/// Stable identifier of a saved site (a random UUID, as text).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SiteId(pub String);

impl SiteId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for SiteId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identifier of a saved provider account, such as a kept Bunny account key (a random
/// UUID, as text).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The API a site is reached through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Bunny,
    /// Any S3-compatible service; [`S3Site::preset`] says which.
    S3,
}

/// The S3-compatible services the Add Site sheet offers by name, in the order it shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum S3Preset {
    AwsS3,
    CloudflareR2,
    BackblazeB2,
    Wasabi,
    DigitalOceanSpaces,
    Minio,
    /// Any other service, at an endpoint the user types.
    Other,
}

impl S3Preset {
    /// The service's name: "Amazon S3", "Cloudflare R2", …, "Other S3".
    pub fn label(self) -> &'static str {
        match self {
            Self::AwsS3 => "Amazon S3",
            Self::CloudflareR2 => "Cloudflare R2",
            Self::BackblazeB2 => "Backblaze B2",
            Self::Wasabi => "Wasabi",
            Self::DigitalOceanSpaces => "DigitalOcean Spaces",
            Self::Minio => "MinIO",
            Self::Other => "Other S3",
        }
    }

    /// The name in a tight spot, such as the conflict sheet's "On {short}" column: "S3", "R2",
    /// "B2", "Wasabi", "Spaces", "MinIO", and "S3" for any other service.
    pub fn short(self) -> &'static str {
        match self {
            Self::AwsS3 | Self::Other => "S3",
            Self::CloudflareR2 => "R2",
            Self::BackblazeB2 => "B2",
            Self::Wasabi => "Wasabi",
            Self::DigitalOceanSpaces => "Spaces",
            Self::Minio => "MinIO",
        }
    }

    /// The letters on the service's tile: "S3", "R2", "B2", "W", "DO", "M", and "…" for any other
    /// service.
    pub fn mono(self) -> &'static str {
        match self {
            Self::AwsS3 => "S3",
            Self::CloudflareR2 => "R2",
            Self::BackblazeB2 => "B2",
            Self::Wasabi => "W",
            Self::DigitalOceanSpaces => "DO",
            Self::Minio => "M",
            Self::Other => "…",
        }
    }
}

/// What an S3 site needs beyond a Bunny site's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Site {
    pub preset: S3Preset,
    /// `scheme://host[:port]`, without the bucket.
    pub endpoint: String,
    /// The bucket goes in the path (`endpoint/bucket/key`) rather than the host name.
    pub path_style: bool,
}

/// A saved connection. It holds no secret (password / API key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteRecord {
    pub id: SiteId,
    pub name: String,
    pub provider: ProviderKind,
    /// The Bunny storage zone, or the S3 bucket.
    pub container: String,
    pub region: String,
    pub host: String,
    /// Set exactly when `provider` is [`ProviderKind::S3`].
    pub s3: Option<S3Site>,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_ids_are_unique_uuids_and_display_as_their_string() {
        let a = SiteId::new();
        let b = SiteId::new();
        assert_ne!(a, b);
        assert_eq!(a.to_string(), a.0);
        assert!(uuid::Uuid::parse_str(&a.0).is_ok(), "{a}");
    }

    #[test]
    fn account_ids_are_unique_uuids_and_display_as_their_string() {
        let a = AccountId::new();
        assert_ne!(a, AccountId::new());
        assert_eq!(a.to_string(), a.0);
        assert!(uuid::Uuid::parse_str(&a.0).is_ok(), "{a}");
    }

    /// The Add Site sheet's provider grid (mockup `providers`), in its order, with the short
    /// name the conflict sheet's "On {short}" column uses.
    #[test]
    fn preset_labels_and_monos() {
        use S3Preset::*;
        let presets = [
            (AwsS3, "Amazon S3", "S3", "S3"),
            (CloudflareR2, "Cloudflare R2", "R2", "R2"),
            (BackblazeB2, "Backblaze B2", "B2", "B2"),
            (Wasabi, "Wasabi", "Wasabi", "W"),
            (DigitalOceanSpaces, "DigitalOcean Spaces", "Spaces", "DO"),
            (Minio, "MinIO", "MinIO", "M"),
            (Other, "Other S3", "S3", "…"),
        ];
        for (preset, label, short, mono) in presets {
            assert_eq!(preset.label(), label, "{preset:?}");
            assert_eq!(preset.short(), short, "{preset:?}");
            assert_eq!(preset.mono(), mono, "{preset:?}");
        }
    }
}
