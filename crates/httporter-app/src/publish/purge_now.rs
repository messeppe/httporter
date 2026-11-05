//! Purge from CDN, now (T5): the toolbar's, the menu's and the inspector's purge.
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use httporter_core::{RemotePath, SiteRecord};

use super::Publisher;

impl Publisher {
    /// Waits until `pending_checks_for(site) == 0` (Q55), then purges `purge_urls(files)` and
    /// `https://{host}/{folder}/*` for each folder; a missing key opens the CDN key sheet; logs a
    /// Purge event.
    pub fn purge_now(
        self: &Arc<Self>,
        site: SiteRecord,
        files: Vec<RemotePath>,
        folders: Vec<RemotePath>,
    ) {
        todo!("T5")
    }
}
