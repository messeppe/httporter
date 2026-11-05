//! Export and import of the saved sites, without secrets (T4).
#![allow(unused_variables, dead_code)]

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    DeleteGuard, ProfileRecord, ProviderKind, PurgeMode, RemotePath, S3Site, SiteExtras, SiteRecord,
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
    todo!("T4")
}

/// Reads a sites file. The error is [`crate::copy::SITES_WRONG_FORMAT`] or
/// [`crate::copy::SITES_NEWER_VERSION`].
pub fn parse_sites_file(text: &str) -> Result<SitesFile, String> {
    todo!("T4")
}
