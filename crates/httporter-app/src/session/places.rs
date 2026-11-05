//! What I4 keeps in the site database (T4): the sidebar's favorites, the window's UI state, each
//! site's extras, the persistent hash cache, and the export / import of the sites.
#![allow(unused_variables, dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use httporter_core::{Favorite, SiteExtras, SiteId, UiState};
use httporter_platform::Store;
use httporter_transfer::{FileHashes, HashStore};

use super::Session;

/// What Import Sites did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub added: usize,
    /// Already saved: the same provider, container and host.
    pub skipped: usize,
    /// The names of the added sites whose password or key must be entered (Edit Site).
    pub need_secret: Vec<String>,
}

impl Session {
    /// The sidebar's favorites.
    pub async fn favorites(&self) -> Vec<Favorite> {
        todo!("T4")
    }

    /// Stores `f`; its new id.
    pub async fn add_favorite(&self, f: Favorite) -> Result<i64, String> {
        todo!("T4")
    }

    pub async fn remove_favorite(&self, id: i64) -> Result<(), String> {
        todo!("T4")
    }

    /// The window's layout and the General settings, as last saved (the defaults when none).
    pub fn ui_state(&self) -> UiState {
        todo!("T4")
    }

    pub fn save_ui_state(&self, s: &UiState) {
        todo!("T4")
    }

    /// The extras of site `id` (cached; the defaults when none).
    pub fn site_extras(&self, id: &SiteId) -> SiteExtras {
        todo!("T4")
    }

    pub async fn save_site_extras(&self, id: SiteId, x: SiteExtras) -> Result<(), String> {
        todo!("T4")
    }

    /// Forgets the conflict choice remembered for the uploads (`upload`) or downloads of site
    /// `id` (M3).
    pub async fn forget_conflicts(&self, id: SiteId, upload: bool) -> Result<(), String> {
        todo!("T4")
    }

    /// The hash cache's store in the site database. T1: a store that keeps nothing yet.
    pub fn hash_store(&self) -> Arc<dyn HashStore> {
        Arc::new(StoreHashes {
            store: self.store.clone(),
        })
    }

    /// Writes the saved sites, without secrets, to `to`; how many.
    pub async fn export_sites(&self, to: PathBuf) -> Result<usize, String> {
        todo!("T4")
    }

    /// Adds the sites of the file `from` that are not saved yet.
    pub async fn import_sites(&self, from: PathBuf) -> Result<ImportReport, String> {
        todo!("T4")
    }
}

/// The hash cache's digests, kept in the site database's `hash_cache` table (schema v4).
struct StoreHashes {
    store: Arc<Mutex<Store>>,
}

impl HashStore for StoreHashes {
    /// T1: none kept yet (T4 reads `Store::hash_get`).
    fn load(&self, path: &Path, size: u64, modified: SystemTime) -> Option<FileHashes> {
        None
    }

    /// T1: kept nowhere yet (T4 writes `Store::hash_put`).
    fn save(&self, path: &Path, h: &FileHashes) {}
}
