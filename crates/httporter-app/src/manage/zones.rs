// SPDX-License-Identifier: MIT OR Apache-2.0

//! Zone and bucket tools (§8.8.4–8.8.5, I6, T5):
//!
//! - Create New Zone… (the Add Site sheet's account-key sign-in): a Bunny storage zone and,
//!   if asked, a pull zone in front of it (`BunnyAccount::create_zone`); the zone becomes a site
//!   the way the account-key sign-in adds one, and the pull zone its CDN.
//! - Create Bucket… (the bucket picker): `S3Storage::create_bucket`.
//! - Settings › Sites › Zone: the 404 page, "Return status 200 for the 404 page" and the
//!   replication regions (which can only be added), through the kept account key.
//! - Settings › Sites › Bucket: the CORS rule and the static website, where the preset offers them
//!   (`httporter_s3::PresetFeatures`); otherwise the section says the service doesn't.
//!
//! Tests use wiremock: building HTTPorter never creates a real zone, pull zone or bucket.
//!
//! T1 is the skeleton: the sheets open and close, nothing else happens yet.

use std::sync::Arc;

use super::Manager;
use crate::{AppWindow, SettingsWindow};

/// Wires Create New Zone… and Create Bucket… of `ui` to `m` (T5). Nothing yet.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    let _ = (ui, m);
}

/// Wires the Zone and Bucket sections of a new Settings window (`settings_tabs::wire` calls it,
/// on the UI thread) (T5). Nothing yet: both sections stay hidden.
pub fn wire_settings(window: &SettingsWindow) {
    let _ = window;
}
