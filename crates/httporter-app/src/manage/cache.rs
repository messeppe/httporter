//! Fetching a site's file into the local cache (T4): `cache_dir(kind)/<site>/<path>`, written
//! as `.part`, then renamed.
#![allow(unused_variables, dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use httporter_core::{Entry, SiteRecord};

use super::Manager;
use crate::AppWindow;

/// Wires this view's callbacks of `ui` to `m` (T4).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}

/// See [`Manager::fetch_to_cache`].
pub(super) async fn fetch_to_cache(
    m: &Manager,
    site: &SiteRecord,
    entry: &Entry,
    kind: &str,
) -> Result<PathBuf, String> {
    todo!("T4")
}
