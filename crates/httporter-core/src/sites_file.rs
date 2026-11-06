// SPDX-License-Identifier: MIT OR Apache-2.0

//! Export and import of the saved sites, without secrets.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    DeleteGuard, ProfileRecord, ProviderKind, PurgeMode, RemotePath, S3Site, SiteExtras,
    SiteRecord, copy,
};

/// The `format` of a sites file.
pub const SITES_FORMAT: &str = "httporter-sites";
/// The newest `version` of a sites file this build writes and reads.
pub const SITES_VERSION: u32 = 1;

/// A sites file: the sites, their extras and their publish profiles. No password, key or secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitesFile {
    pub format: String,
    pub version: u32,
    pub exported_at: DateTime<Utc>,
    pub sites: Vec<ExportedSite>,
}

/// One site of a sites file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSite {
    pub name: String,
    pub provider: ProviderKind,
    pub container: String,
    pub region: String,
    pub host: String,
    pub s3: Option<S3Site>,
    #[serde(default)]
    pub extras: SiteExtras,
    #[serde(default)]
    pub profiles: Vec<ExportedProfile>,
}

/// One publish profile of an exported site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedProfile {
    pub name: String,
    pub local_dir: PathBuf,
    pub remote_dir: RemotePath,
    pub skip_rules: Vec<String>,
    pub remove_extras: bool,
    pub purge: PurgeMode,
    pub guard: DeleteGuard,
    #[serde(default)]
    pub cache_control: Option<String>,
}

/// The sites file for `sites`, each with its extras and profiles.
pub fn export_sites(sites: &[(SiteRecord, SiteExtras, Vec<ProfileRecord>)]) -> SitesFile {
    SitesFile {
        format: SITES_FORMAT.to_owned(),
        version: SITES_VERSION,
        exported_at: Utc::now(),
        sites: sites
            .iter()
            .map(|(site, extras, profiles)| ExportedSite {
                name: site.name.clone(),
                provider: site.provider,
                container: site.container.clone(),
                region: site.region.clone(),
                host: site.host.clone(),
                s3: site.s3.clone(),
                extras: extras.clone(),
                profiles: profiles
                    .iter()
                    .filter(|p| p.site == site.id)
                    .map(|p| ExportedProfile {
                        name: p.name.clone(),
                        local_dir: p.local_dir.clone(),
                        remote_dir: p.remote_dir.clone(),
                        skip_rules: p.skip_rules.clone(),
                        remove_extras: p.remove_extras,
                        purge: p.purge,
                        guard: p.guard,
                        cache_control: p.cache_control.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// `file` as pretty JSON, refused (`Err`) if any field name says password, key or secret: an
/// export must never carry one.
pub fn sites_file_text(file: &SitesFile) -> Result<String, String> {
    let value = serde_json::to_value(file).map_err(|e| e.to_string())?;
    if secret_field(&value).is_some() {
        return Err(copy::SITES_SECRET_FIELD.to_owned());
    }
    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

/// The first field name in `value` that could name a secret (password, secret, key, token).
fn secret_field(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => map.iter().find_map(|(name, inner)| {
            let lower = name.to_ascii_lowercase();
            let secret = ["password", "secret", "key", "token"]
                .iter()
                .any(|word| lower.contains(word));
            if secret {
                Some(name.clone())
            } else {
                secret_field(inner)
            }
        }),
        serde_json::Value::Array(items) => items.iter().find_map(secret_field),
        _ => None,
    }
}

/// Reads a sites file. The error is [`crate::copy::SITES_WRONG_FORMAT`] or
/// [`crate::copy::SITES_NEWER_VERSION`].
pub fn parse_sites_file(text: &str) -> Result<SitesFile, String> {
    let wrong = || copy::SITES_WRONG_FORMAT.to_owned();
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| wrong())?;
    if value.get("format").and_then(serde_json::Value::as_str) != Some(SITES_FORMAT) {
        return Err(wrong());
    }
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(wrong)?;
    if version > u64::from(SITES_VERSION) {
        return Err(copy::SITES_NEWER_VERSION.to_owned());
    }
    serde_json::from_value(value).map_err(|_| wrong())
}
