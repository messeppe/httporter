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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    Bunny,
}

/// A saved connection. It holds no secret (password / API key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteRecord {
    pub id: SiteId,
    pub name: String,
    pub provider: ProviderKind,
    pub zone: String,
    pub region: String,
    pub host: String,
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
}
