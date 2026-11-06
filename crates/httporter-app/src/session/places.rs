// SPDX-License-Identifier: MIT OR Apache-2.0

//! What I4 keeps in the site database: the sidebar's favorites, the window's UI state, each
//! site's extras, the persistent hash cache, and the export / import of the sites.
//!
//! The UI state and the site extras are read on the UI thread (the toolbar asks whether a site is
//! read-only on every refresh), so they are cached here after the first read; writes go to the
//! database off the UI thread. Like the settings, a value that can't be read answers as the
//! defaults.

// T6 (favorites), T7 (UI state) and T8 (extras, export / import, hash cache) call these; drop
// this once they are merged.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{TimeDelta, Utc};
use httporter_core::sites_file::sites_file_text;
use httporter_core::{
    Favorite, ProfileExtras, ProfileId, ProfileRecord, SiteExtras, SiteId, SiteRecord,
    UI_STATE_KEY, UiState, export_sites, parse_sites_file,
};
use httporter_platform::{Store, site_conflicts_key, site_extras_key};
use httporter_transfer::{FileHashes, HashStore};

use super::{Session, blocking, lock};

/// Digests not used for this many days are forgotten at start.
const HASH_KEEP_DAYS: i64 = 90;

/// What Import Sites did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub added: usize,
    /// Already saved: the same provider, container and host.
    pub skipped: usize,
    /// The names of the added sites whose password or key must be entered (Edit Site).
    pub need_secret: Vec<String>,
}

/// The UI state and the site extras as last read or saved.
#[derive(Default)]
pub(super) struct PlacesCache {
    ui: Option<UiState>,
    extras: HashMap<SiteId, SiteExtras>,
    /// The UI state waiting to be written; the newest wins.
    pending_ui: Option<UiState>,
    /// The UI state as last read from the store or written to it: a write stores only the
    /// fields that differ from it (ledger 138, 144).
    written_ui: Option<UiState>,
    /// M8: how many transfers the engine lets each site run now, after any slow-down.
    engine_limits: HashMap<SiteId, usize>,
}

impl Session {
    /// M8: notes how many transfers the engine lets `site` run now (`None`: not known); whether
    /// that changed.
    pub fn set_engine_limit(&self, site: &SiteId, limit: Option<usize>) -> bool {
        let mut places = lock(&self.places);
        match limit {
            Some(limit) => places.engine_limits.insert(site.clone(), limit) != Some(limit),
            None => places.engine_limits.remove(site).is_some(),
        }
    }

    /// M8: how many transfers the engine lets `site` run now, if known.
    pub(super) fn engine_limit(&self, site: &SiteId) -> Option<usize> {
        lock(&self.places).engine_limits.get(site).copied()
    }

    /// The sidebar's favorites.
    pub async fn favorites(&self) -> Vec<Favorite> {
        let store = self.store.clone();
        blocking(move || lock(&store).favorites().unwrap_or_default()).await
    }

    /// Stores `f`; its new id.
    pub async fn add_favorite(&self, f: Favorite) -> Result<i64, String> {
        let store = self.store.clone();
        blocking(move || lock(&store).add_favorite(&f).map_err(|e| e.to_string())).await
    }

    pub async fn remove_favorite(&self, id: i64) -> Result<(), String> {
        let store = self.store.clone();
        blocking(move || lock(&store).remove_favorite(id).map_err(|e| e.to_string())).await
    }

    /// The window's layout and the General settings, as last saved (the defaults when none).
    pub fn ui_state(&self) -> UiState {
        if let Some(ui) = lock(&self.places).ui.clone() {
            return ui;
        }
        let read: UiState = lock(&self.store).settings(UI_STATE_KEY);
        let mut places = lock(&self.places);
        places.written_ui.get_or_insert_with(|| read.clone());
        places.ui.get_or_insert(read).clone()
    }

    /// Keeps `s` as the UI state and writes it to the database on a thread of its own. Saves made
    /// one after another end up with the newest stored.
    pub fn save_ui_state(&self, s: &UiState) {
        {
            let mut places = lock(&self.places);
            places.ui = Some(s.clone());
            places.pending_ui = Some(s.clone());
        }
        let (store, places) = (self.store.clone(), self.places.clone());
        let _ = std::thread::Builder::new()
            .name("save-ui-state".into())
            .spawn(move || {
                // The store first, then the newest waiting: a late thread finds nothing to write.
                // Only the fields changed since the last read or write are written, so that the
                // defaults of a read that failed never replace what is stored.
                let store = lock(&store);
                let Some((ui, written)) = ({
                    let mut places = lock(&places);
                    let written = places.written_ui.clone();
                    places.pending_ui.take().map(|ui| (ui, written))
                }) else {
                    return;
                };
                let written = written.unwrap_or_else(|| store.settings(UI_STATE_KEY));
                if store.update_settings(UI_STATE_KEY, &written, &ui).is_ok() {
                    lock(&places).written_ui = Some(ui);
                }
            });
    }

    /// The extras of site `id` (cached; the defaults when none).
    pub fn site_extras(&self, id: &SiteId) -> SiteExtras {
        if let Some(extras) = lock(&self.places).extras.get(id) {
            return extras.clone();
        }
        let read: SiteExtras = lock(&self.store).settings(&site_extras_key(id));
        lock(&self.places)
            .extras
            .entry(id.clone())
            .or_insert(read)
            .clone()
    }

    pub async fn save_site_extras(&self, id: SiteId, x: SiteExtras) -> Result<(), String> {
        let (store, places) = (self.store.clone(), self.places.clone());
        blocking(move || {
            lock(&store)
                .put_settings(&site_extras_key(&id), &x)
                .map_err(|e| e.to_string())?;
            lock(&places).extras.insert(id, x);
            Ok(())
        })
        .await
    }

    /// Forgets the conflict choice remembered for the uploads (`upload`) or downloads of site
    /// `id` (M3).
    pub async fn forget_conflicts(&self, id: SiteId, upload: bool) -> Result<(), String> {
        let store = self.store.clone();
        blocking(move || {
            lock(&store)
                .remove_setting(&site_conflicts_key(&id, upload))
                .map_err(|e| e.to_string())
        })
        .await
    }

    /// The hash cache's store in the site database.
    pub fn hash_store(&self) -> Arc<dyn HashStore> {
        Arc::new(StoreHashes {
            store: self.store.clone(),
        })
    }

    /// How many local files' digests the hash cache keeps (Settings › Advanced).
    pub async fn hash_cache_count(&self) -> u64 {
        let store = self.store.clone();
        blocking(move || lock(&store).hash_count().unwrap_or(0)).await
    }

    /// Forgets every digest the hash cache keeps (Settings › Advanced).
    pub async fn clear_hash_cache(&self) -> Result<(), String> {
        let store = self.store.clone();
        blocking(move || lock(&store).hash_clear().map_err(|e| e.to_string())).await
    }

    /// Forgets, on a thread of its own, the digests not used for 90 days (once, at start).
    pub fn prune_hash_cache(&self) {
        let store = self.store.clone();
        let _ = std::thread::Builder::new()
            .name("prune-hashes".into())
            .spawn(move || {
                let since = Utc::now() - TimeDelta::days(HASH_KEEP_DAYS);
                let _ = lock(&store).hash_prune(since);
            });
    }

    /// Writes the saved sites, without secrets, to `to`; how many.
    pub async fn export_sites(&self, to: PathBuf) -> Result<usize, String> {
        let sites = self.sites();
        let store = self.store.clone();
        blocking(move || {
            let (profiles, extras) = {
                let store = lock(&store);
                let profiles = store.list_profiles().map_err(|e| e.to_string())?;
                let extras: Vec<SiteExtras> = sites
                    .iter()
                    .map(|site| store.settings(&site_extras_key(&site.id)))
                    .collect();
                (profiles, extras)
            };
            let rows: Vec<(SiteRecord, SiteExtras, Vec<ProfileRecord>)> = sites
                .into_iter()
                .zip(extras)
                .map(|(site, extras)| {
                    let mine = profiles
                        .iter()
                        .filter(|p| p.site == site.id)
                        .cloned()
                        .collect();
                    (site, extras, mine)
                })
                .collect();
            let file = export_sites(&rows);
            let text = sites_file_text(&file)?;
            std::fs::write(&to, text).map_err(|e| e.to_string())?;
            Ok(file.sites.len())
        })
        .await
    }

    /// Adds the sites of the file `from` that are not saved yet, with new ids, their extras and
    /// their profiles. They have no password or key yet: Edit Site asks for it.
    pub async fn import_sites(&self, from: PathBuf) -> Result<ImportReport, String> {
        let (store, book, places) = (self.store.clone(), self.book.clone(), self.places.clone());
        blocking(move || {
            let text = std::fs::read_to_string(&from).map_err(|e| e.to_string())?;
            let file = parse_sites_file(&text)?;
            let mut report = ImportReport::default();
            let store = lock(&store);
            let mut saved = store.list_sites().map_err(|e| e.to_string())?.sites;
            for site in file.sites {
                let present = saved.iter().any(|s| {
                    s.provider == site.provider
                        && s.container.eq_ignore_ascii_case(&site.container)
                        && s.host.eq_ignore_ascii_case(&site.host)
                });
                if present {
                    report.skipped += 1;
                    continue;
                }
                let now = Utc::now();
                let record = SiteRecord {
                    id: SiteId::new(),
                    name: site.name.clone(),
                    provider: site.provider,
                    container: site.container,
                    region: site.region,
                    host: site.host,
                    s3: site.s3,
                    created_at: now,
                };
                store.add_site(&record).map_err(|e| e.to_string())?;
                if site.extras != SiteExtras::default() {
                    let _ = store.put_settings(&site_extras_key(&record.id), &site.extras);
                }
                lock(&places)
                    .extras
                    .insert(record.id.clone(), site.extras.clone());
                for p in site.profiles {
                    let profile = ProfileRecord {
                        id: ProfileId::new(),
                        site: record.id.clone(),
                        name: p.name,
                        local_dir: p.local_dir,
                        remote_dir: p.remote_dir,
                        skip_rules: p.skip_rules,
                        remove_extras: p.remove_extras,
                        purge: p.purge,
                        guard: p.guard,
                        created_at: now,
                        last_published_at: None,
                        cache_control: p.cache_control,
                        // Watching and schedules are this PC's: an import starts with them off.
                        extras: ProfileExtras::default(),
                    };
                    let _ = store.add_profile(&profile);
                }
                report.added += 1;
                report.need_secret.push(site.name);
                saved.push(record);
            }
            if let Ok(list) = store.list_sites() {
                let mut book = lock(&book);
                book.sites = list.sites;
                book.unreadable = list.unreadable;
            }
            Ok(report)
        })
        .await
    }
}

/// The hash cache's digests, kept in the site database's `hash_cache` table (schema v4).
struct StoreHashes {
    store: Arc<Mutex<Store>>,
}

/// `t` in nanoseconds since 1970, if it is after then and fits.
fn nanos(t: SystemTime) -> Option<u64> {
    u64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()
}

impl HashStore for StoreHashes {
    fn load(&self, path: &Path, size: u64, modified: SystemTime) -> Option<FileHashes> {
        let (sha256, md5) = lock(&self.store)
            .hash_get(path, size, nanos(modified)?)
            .ok()??;
        Some(FileHashes {
            sha256,
            md5,
            size,
            modified,
        })
    }

    fn save(&self, path: &Path, h: &FileHashes) {
        if let Some(mtime) = nanos(h.modified) {
            let _ = lock(&self.store).hash_put(path, h.size, mtime, &h.sha256, &h.md5);
        }
    }
}
