// SPDX-License-Identifier: MIT OR Apache-2.0

use chrono::{DateTime, NaiveDateTime, Utc};
use httporter_core::{Checksum, Entry, ProviderError, RemotePath};
use serde::Deserialize;

/// One element of Bunny's directory-listing JSON. Only the fields we use are
/// declared; the rest (`Guid`, `Path`, `ServerId`, ...) are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BunnyObject {
    pub object_name: String,
    pub length: u64,
    pub is_directory: bool,
    /// ISO 8601 without a zone designator; the service reports UTC.
    pub last_changed: String,
    pub checksum: Option<String>,
    pub content_type: Option<String>,
    /// Comma-separated region codes, for example `"LA,DE,BR"`.
    pub replicated_zones: Option<String>,
    /// When the object was first stored, as raw as `LastChanged`; read only by `stat`.
    #[serde(default)]
    pub date_created: Option<String>,
}

/// Maps a listing element to an [`Entry`] inside the directory `dir`.
pub fn to_entry(obj: BunnyObject, dir: &RemotePath) -> Result<Entry, ProviderError> {
    let path = dir
        .join(&obj.object_name)
        .map_err(|e| ProviderError::Permanent(format!("unusable object name: {e}")))?;
    Ok(Entry {
        path,
        is_folder: obj.is_directory,
        size: obj.length,
        modified: Some(parse_last_changed(&obj.last_changed)?),
        checksum: obj
            .checksum
            .as_deref()
            .filter(|hex| !hex.is_empty())
            .map(Checksum::sha256_hex)
            .transpose()?,
        content_type: obj.content_type.filter(|ct| !ct.is_empty()),
        replicated_regions: obj
            .replicated_zones
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|zone| !zone.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

/// `DateCreated`, when it is there and readable.
pub fn created(obj: &BunnyObject) -> Option<DateTime<Utc>> {
    obj.date_created
        .as_deref()
        .and_then(|value| parse_last_changed(value).ok())
}

fn parse_last_changed(value: &str) -> Result<DateTime<Utc>, ProviderError> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .map(|naive| naive.and_utc())
        .map_err(|e| ProviderError::Permanent(format!("bad LastChanged {value:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httporter_core::{Entry, ProviderError, RemotePath};

    #[test]
    fn maps_listing_fixture() {
        let objs: Vec<BunnyObject> =
            serde_json::from_str(include_str!("../tests/fixtures/list_root.json")).unwrap();
        let e: Vec<Entry> = objs
            .into_iter()
            .map(|o| to_entry(o, &RemotePath::root()))
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(e[0].name(), "author-sitemap.xml");
        assert_eq!(e[0].size, 688);
        assert_eq!(
            e[0].checksum.as_ref().unwrap().hex,
            "b952dedc8acb32567c6a1409077036c7d0f371c85a4d75bcd667e4740b75f991"
        );
        assert_eq!(
            e[0].modified.unwrap().to_rfc3339(),
            "2026-10-28T10:02:51.089+00:00"
        );
        assert_eq!(e[0].replicated_regions, vec!["LA", "DE", "BR"]);
        assert_eq!(e[0].content_type, None);
        assert!(e[1].is_folder && e[1].checksum.is_none() && e[1].name() == "wp-content");
    }

    #[test]
    fn rejects_object_names_with_slash() {
        let obj = BunnyObject {
            object_name: "a/b".into(),
            length: 1,
            is_directory: false,
            last_changed: "2026-10-28T10:02:51.089".into(),
            checksum: None,
            content_type: None,
            replicated_zones: None,
            date_created: None,
        };
        let err = to_entry(obj, &RemotePath::root()).unwrap_err();
        assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    fn object(name: &str) -> BunnyObject {
        BunnyObject {
            object_name: name.into(),
            length: 0,
            is_directory: false,
            last_changed: "2026-10-28T10:02:51.089".into(),
            checksum: None,
            content_type: None,
            replicated_zones: None,
            date_created: None,
        }
    }

    #[test]
    fn joins_the_object_name_onto_the_directory() {
        let dir = RemotePath::parse("wp-content/uploads").unwrap();
        let entry = to_entry(object("a.png"), &dir).unwrap();
        assert_eq!(entry.path.as_str(), "wp-content/uploads/a.png");
    }

    #[test]
    fn accepts_timestamps_without_or_with_long_fractions() {
        for (input, expected) in [
            ("2026-10-28T10:02:51", "2026-10-28T10:02:51+00:00"),
            (
                "2026-10-28T10:02:51.0890000",
                "2026-10-28T10:02:51.089+00:00",
            ),
        ] {
            let mut obj = object("a");
            obj.last_changed = input.into();
            let entry = to_entry(obj, &RemotePath::root()).unwrap();
            assert_eq!(entry.modified.unwrap().to_rfc3339(), expected, "{input}");
        }
    }

    #[test]
    fn a_malformed_timestamp_or_checksum_is_a_permanent_error() {
        let mut bad_time = object("a");
        bad_time.last_changed = "yesterday".into();
        assert!(matches!(
            to_entry(bad_time, &RemotePath::root()),
            Err(ProviderError::Permanent(_))
        ));

        let mut bad_sum = object("a");
        bad_sum.checksum = Some("abc".into());
        assert!(matches!(
            to_entry(bad_sum, &RemotePath::root()),
            Err(ProviderError::Permanent(_))
        ));
    }

    #[test]
    fn empty_optional_strings_become_none_and_zones_are_trimmed() {
        let mut obj = object("a");
        obj.checksum = Some(String::new());
        obj.content_type = Some("text/css".into());
        obj.replicated_zones = Some(" LA , ,DE,".into());
        let entry = to_entry(obj, &RemotePath::root()).unwrap();
        assert!(entry.checksum.is_none());
        assert_eq!(entry.content_type.as_deref(), Some("text/css"));
        assert_eq!(entry.replicated_regions, vec!["LA", "DE"]);
    }
}
