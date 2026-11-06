// SPDX-License-Identifier: MIT OR Apache-2.0

/// Bunny Storage regions as `(code, city)`, in display order.
pub const REGIONS: [(&str, &str); 9] = [
    ("de", "Frankfurt"),
    ("uk", "London"),
    ("ny", "New York"),
    ("la", "Los Angeles"),
    ("sg", "Singapore"),
    ("se", "Stockholm"),
    ("br", "São Paulo"),
    ("jh", "Johannesburg"),
    ("syd", "Sydney"),
];

/// The storage API hostname for a region code, or `None` for an unknown code.
/// Frankfurt (`de`) is the default region and lives on the bare hostname; every
/// other region is a prefix of it.
pub fn storage_host(code: &str) -> Option<String> {
    if !REGIONS.iter().any(|(known, _)| *known == code) {
        return None;
    }
    Some(match code {
        "de" => "storage.bunnycdn.com".to_owned(),
        other => format!("{other}.storage.bunnycdn.com"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_per_region() {
        assert_eq!(storage_host("de").as_deref(), Some("storage.bunnycdn.com"));
        assert_eq!(
            storage_host("sg").as_deref(),
            Some("sg.storage.bunnycdn.com")
        );
        assert_eq!(
            storage_host("syd").as_deref(),
            Some("syd.storage.bunnycdn.com")
        );
        assert_eq!(storage_host("xx"), None);
    }

    #[test]
    fn every_listed_region_has_a_host() {
        for (code, _) in REGIONS {
            assert!(storage_host(code).is_some(), "{code}");
        }
        assert_eq!(storage_host("DE"), None);
    }
}
