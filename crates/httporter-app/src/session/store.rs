//! The settings kept in the site database: the Transfers settings and each site's remembered
//! conflict choice.
//!
//! [`Store::settings`] answers the default when a setting cannot be read, so nothing here ever
//! writes back what it read: a setting is saved only when the user changed it, and then only the
//! fields that changed ([`Store::update_settings`]), so that the defaults of a read that failed
//! never replace what is stored (ledger 138, 144).

use std::sync::{Arc, Mutex};

use httporter_core::{ConflictPolicy, SiteId, TransferSettings};
use httporter_platform::{SETTINGS_TRANSFERS, Store, site_conflicts_key};
use tokio::task::JoinHandle;

use super::{Session, blocking, lock};
use crate::rt::Rt;

impl Session {
    /// The Transfers settings in use: as stored when the session started (the defaults when
    /// none could be read), or as last changed.
    pub fn settings(&self) -> TransferSettings {
        self.hub.settings()
    }

    /// Saves the Transfers settings, on one of `rt`'s blocking threads: the fields that differ
    /// from those last read or written. Saves started one after another end up with the newest
    /// stored, whatever order their threads run in.
    pub fn save_settings(
        &self,
        rt: &Rt,
        settings: TransferSettings,
    ) -> JoinHandle<Result<(), String>> {
        let (store, pending) = (self.store.clone(), self.pending_settings.clone());
        lock(&pending).newest = Some(settings);
        rt.spawn_blocking(move || write_newest(&store, &pending))
    }

    /// How conflicts are settled for an upload (`upload`) or a download on `site`: the choice
    /// remembered for the site, or the Transfers setting.
    pub async fn conflict_policy(&self, site: &SiteId, upload: bool) -> ConflictPolicy {
        let (store, key) = (self.store.clone(), site_conflicts_key(site, upload));
        let remembered: Option<ConflictPolicy> =
            blocking(move || lock(&store).settings(&key)).await;
        remembered.unwrap_or_else(|| {
            let settings = self.hub.settings();
            if upload {
                settings.upload_conflicts
            } else {
                settings.download_conflicts
            }
        })
    }

    /// Remembers `policy` for the uploads (`upload`) or downloads of `site` ("Remember for this
    /// site" on the conflict sheet).
    pub async fn remember_conflicts(
        &self,
        site: &SiteId,
        upload: bool,
        policy: ConflictPolicy,
    ) -> Result<(), String> {
        let (store, key) = (self.store.clone(), site_conflicts_key(site, upload));
        blocking(move || {
            lock(&store)
                .put_settings(&key, &policy)
                .map_err(|e| e.to_string())
        })
        .await
    }
}

/// The Transfers settings waiting to be written, and those last read or written: a write stores
/// the fields that differ between the two.
pub(super) struct PendingSettings {
    written: TransferSettings,
    newest: Option<TransferSettings>,
}

impl PendingSettings {
    /// Nothing waiting, after reading `settings` (the defaults, when the read failed).
    pub(super) fn read_as(settings: TransferSettings) -> Self {
        Self {
            written: settings,
            newest: None,
        }
    }
}

/// Writes the newest settings waiting, if any: the fields they changed since the last write. The
/// store is locked before they are taken, so a write that runs late finds nothing newer to take
/// and leaves the newest in place. A write that fails leaves its changes to the next one.
fn write_newest(store: &Mutex<Store>, pending: &Arc<Mutex<PendingSettings>>) -> Result<(), String> {
    let store = lock(store);
    let (written, settings) = {
        let mut pending = lock(pending);
        let Some(settings) = pending.newest.take() else {
            return Ok(());
        };
        (pending.written.clone(), settings)
    };
    store
        .update_settings(SETTINGS_TRANSFERS, &written, &settings)
        .map_err(|e| e.to_string())?;
    lock(pending).written = settings;
    Ok(())
}
