//! The CDN side of Publish (T7): finding a site's purger, the CDN key sheet, and the delete
//! alert's purge.
//!
//! A Bunny site's purger is its stored CDN link with the account key kept for it; failing that,
//! the first kept Bunny account that has a pull zone in front of the site's storage zone (which
//! then becomes the link); failing that, the CDN key sheet asks for the key. The key goes from the
//! sheet straight to Windows Credential Manager (`Session::save_cdn_link`); it is never logged nor
//! shown, and no message is made from it.
//!
//! Every purge runs after what it purges is done: a publish's inside `run_publish`; the delete
//! alert's and Retry Purge's here, once the engine's late checks for the site are over too
//! (ruling Q55).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::Utc;
use httporter_bunny::{BunnyAccount, BunnyCdn};
use httporter_core::{
    AccountId, CdnLink, CdnPurger, FileOutcome, PURGE_URL_LIMIT, ProviderError, ProviderKind,
    PurgeMode, PurgeResult, RemotePath, SiteId, SiteRecord, TransferSettings, purge_urls,
    run_purge,
};
use httporter_transfer::{Engine, PublishPhase};
use slint::{ComponentHandle, SharedString};
use tokio::sync::oneshot;

use super::{Publisher, PurgerAnswer, copy, lock};
use crate::AppWindow;
use crate::session::blocking;
use crate::transfers::DeletePurge;

/// How long a purge waiting for the engine's late checks waits, at most, before it looks again.
const CHECKS_POLL: Duration = Duration::from_secs(1);

/// The CDN key sheet while it is up: the site it asks for, and who waits for its answer.
pub(super) struct KeyAsk {
    site: SiteRecord,
    waiters: Vec<oneshot::Sender<PurgerAnswer>>,
    /// Goes up with every Save, so that the answer to an earlier one is dropped.
    saves: u64,
}

/// A Bunny account client for `key` (making one sets up TLS, so on a blocking thread).
async fn account(key: String, settings: TransferSettings) -> Result<BunnyAccount, ProviderError> {
    blocking(move || BunnyAccount::with_settings(key, &settings)).await
}

pub(super) async fn purger_for(p: &Publisher, site: &SiteRecord) -> PurgerAnswer {
    if site.provider != ProviderKind::Bunny {
        return PurgerAnswer::Unavailable(copy::PURGE_NEEDS_BUNNY.to_owned());
    }
    let settings = p.session.settings();
    if let Some(link) = p.session.cdn_link(site.id.clone()).await
        && let Some(key) = p.session.account_key(link.account.clone()).await
    {
        return match account(key, settings).await {
            Ok(account) => PurgerAnswer::Ready(Arc::new(BunnyCdn::new(account, link.zones))),
            Err(e) => PurgerAnswer::Unavailable(copy::account_unreachable(e)),
        };
    }
    for record in p.session.bunny_accounts().await {
        if record.provider != ProviderKind::Bunny {
            continue;
        }
        let Some(key) = p.session.account_key(record.id.clone()).await else {
            continue;
        };
        let Ok(account) = account(key, settings.clone()).await else {
            continue;
        };
        let Ok(zones) = account.cdn_zones_for(&site.container).await else {
            continue;
        };
        let link = CdnLink {
            account: record.id.clone(),
            zones: zones.clone(),
            checked_at: Utc::now(),
        };
        // Kept for next time; if it can't be, the next purge asks Bunny again.
        let _ = p.session.save_cdn_link(site.id.clone(), link, None).await;
        return PurgerAnswer::Ready(Arc::new(BunnyCdn::new(account, zones)));
    }
    PurgerAnswer::NeedKey
}

/// [`Publisher::purger_for`], asking for the key with the CDN key sheet when it is missing.
/// `Err` says why there is no purge.
pub(super) async fn purger_or_key(
    p: &Arc<Publisher>,
    site: &SiteRecord,
) -> Result<Arc<dyn CdnPurger>, String> {
    let answer = match p.purger_for(site).await {
        PurgerAnswer::NeedKey => {
            let (tx, rx) = oneshot::channel();
            ask_key(p, site.clone(), Some(tx));
            rx.await
                .unwrap_or_else(|_| PurgerAnswer::Unavailable(copy::PURGE_NO_KEY.to_owned()))
        }
        answer => answer,
    };
    match answer {
        PurgerAnswer::Ready(purger) => Ok(purger),
        PurgerAnswer::Unavailable(why) => Err(why),
        PurgerAnswer::NeedKey => Err(copy::PURGE_NO_KEY.to_owned()),
    }
}

/// Opens the CDN key sheet for `site`; `waiter` hears how it ends. While it is up for this site
/// already, `waiter` waits for that one; while it is up for another, `waiter` goes without.
pub(super) fn ask_key(
    p: &Arc<Publisher>,
    site: SiteRecord,
    waiter: Option<oneshot::Sender<PurgerAnswer>>,
) {
    {
        let mut state = lock(&p.state);
        match state.key.as_mut() {
            Some(ask) if ask.site.id == site.id => {
                ask.waiters.extend(waiter);
                return;
            }
            Some(_) => {
                if let Some(waiter) = waiter {
                    let _ = waiter.send(PurgerAnswer::Unavailable(copy::KEY_SHEET_BUSY.to_owned()));
                }
                return;
            }
            None => {}
        }
        state.key = Some(KeyAsk {
            site: site.clone(),
            waiters: waiter.into_iter().collect(),
            saves: 0,
        });
    }
    let zone = site.container;
    let _ = p.link.ui().upgrade_in_event_loop(move |ui| {
        ui.set_cdn_key_zone(zone.into());
        ui.set_cdn_key_error(SharedString::new());
        ui.set_cdn_key_busy(false);
        ui.set_cdn_key_open(true);
    });
}

/// Checks `key` with Bunny (it must reach a pull zone in front of `site`'s storage zone), then
/// keeps it and the link. `Err` is what the sheet says.
async fn check_key(
    p: &Publisher,
    site: &SiteRecord,
    key: String,
) -> Result<Arc<dyn CdnPurger>, String> {
    let key = key.trim().to_owned();
    let account = account(key.clone(), p.session.settings())
        .await
        .map_err(copy::key_check_failed)?;
    let zones = account
        .cdn_zones_for(&site.container)
        .await
        .map_err(|e| match e {
            ProviderError::Auth { .. } => copy::KEY_REFUSED.to_owned(),
            ProviderError::NotFound => copy::no_pull_zone(&site.container),
            other => copy::key_check_failed(other),
        })?;
    let link = CdnLink {
        account: AccountId::new(),
        zones: zones.clone(),
        checked_at: Utc::now(),
    };
    p.session
        .save_cdn_link(site.id.clone(), link, Some(key))
        .await
        .map_err(|e| copy::key_not_saved(&e))?;
    Ok(Arc::new(BunnyCdn::new(account, zones)))
}

/// Save on the CDN key sheet.
fn save_key(p: &Arc<Publisher>, ui: &AppWindow, key: String) {
    let (site, save) = {
        let mut state = lock(&p.state);
        let Some(ask) = state.key.as_mut() else {
            return;
        };
        ask.saves += 1;
        (ask.site.clone(), ask.saves)
    };
    ui.set_cdn_key_busy(true);
    let this = p.clone();
    p.link.rt().spawn(async move {
        let checked = check_key(&this, &site, key).await;
        let waiters = {
            let mut state = lock(&this.state);
            match state.key.as_ref() {
                Some(ask) if ask.site.id == site.id && ask.saves == save => {}
                // Cancelled, or saved again, meanwhile.
                _ => return,
            }
            if checked.is_ok() {
                state.key.take().map(|ask| ask.waiters).unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        match checked {
            Ok(purger) => {
                for waiter in waiters {
                    let _ = waiter.send(PurgerAnswer::Ready(purger.clone()));
                }
                let _ = this.link.ui().upgrade_in_event_loop(|ui| {
                    ui.set_cdn_key_busy(false);
                    ui.set_cdn_key_open(false);
                });
                this.link.status(copy::KEY_SAVED.to_owned());
            }
            Err(text) => {
                let _ = this.link.ui().upgrade_in_event_loop(move |ui| {
                    ui.set_cdn_key_busy(false);
                    ui.set_cdn_key_error(text.into());
                });
            }
        }
    });
}

/// Cancel on the CDN key sheet (closed already): whoever waits goes on without the purge.
fn cancel_key(p: &Publisher) {
    let ask = lock(&p.state).key.take();
    for waiter in ask.map(|ask| ask.waiters).unwrap_or_default() {
        let _ = waiter.send(PurgerAnswer::Unavailable(copy::PURGE_NO_KEY.to_owned()));
    }
}

/// Waits until the engine has no late checks left for `site` (ruling Q55): a check may send a
/// file again, and a purge must come after it.
async fn settled(engine: &Engine, site: &SiteId) {
    let mut changes = engine.changes();
    while engine.pending_checks_for(site) > 0 {
        if let Ok(Err(_)) = tokio::time::timeout(CHECKS_POLL, changes.changed()).await {
            // The engine stopped: nothing will check anything any more.
            break;
        }
    }
}

/// `mode`, or the whole zone when changed files would be more URLs than purging them one by one
/// is worth.
fn mode_for(purger: &dyn CdnPurger, mode: PurgeMode, paths: &[RemotePath]) -> PurgeMode {
    if mode == PurgeMode::Changed && purge_urls(purger.zones(), paths).len() > PURGE_URL_LIMIT {
        PurgeMode::Zone
    } else {
        mode
    }
}

/// Purges `paths` of `site` with `purger`, saying how far it is in the status line.
async fn purge(
    p: &Publisher,
    purger: &dyn CdnPurger,
    site: &SiteRecord,
    mode: PurgeMode,
    paths: &[RemotePath],
) -> PurgeResult {
    settled(p.transfers.engine(), &site.id).await;
    let mode = mode_for(purger, mode, paths);
    let (link, name) = (p.link.clone(), site.name.clone());
    let progress = move |done: usize, total: usize| {
        link.status(copy::phase(&name, PublishPhase::Purging { done, total }));
    };
    run_purge(purger, mode, paths, &progress).await
}

/// Purges again what publish `history` uploaded or deleted, and stores how it went.
pub(super) fn retry_purge(p: &Arc<Publisher>, history: i64) {
    let this = p.clone();
    p.link.rt().spawn(async move {
        let Some(profile) = this.profile_of_history(history).await else {
            this.link.status(copy::HISTORY_GONE.to_owned());
            return;
        };
        let Some(site) = this.session.site(&profile.site) else {
            this.link.status(copy::PROFILE_SITE_GONE.to_owned());
            return;
        };
        let paths: Vec<RemotePath> = this
            .session
            .history_files(history)
            .await
            .into_iter()
            .filter(|file| file.outcome == FileOutcome::Done)
            .map(|file| file.path)
            .collect();
        let result = match purger_or_key(&this, &site).await {
            Err(why) => PurgeResult::Unavailable(why),
            Ok(purger) => {
                let mode = match profile.purge {
                    PurgeMode::Off => PurgeMode::Changed,
                    mode => mode,
                };
                purge(&this, &*purger, &site, mode, &paths).await
            }
        };
        let message = copy::purge_done(&result);
        if let Err(e) = this.session.set_history_purge(history, result).await {
            this.link.status(copy::history_not_saved(&e));
            return;
        }
        super::profile_view::refresh(&this);
        this.link.status(message);
    });
}

/// The delete alert's CDN purge, handed to the transfers. It holds the Publisher weakly, so the
/// two don't keep each other alive.
struct DeleteHook {
    publisher: Weak<Publisher>,
    /// Goes up with every alert, so that a link found late for an earlier one is dropped.
    offers: Arc<AtomicU64>,
}

impl DeletePurge for DeleteHook {
    /// A Bunny site can purge: ticked when it has a CDN link, else unticked with "Asks for your
    /// Bunny account key". An S3 site can't.
    fn offer(&self, site: &SiteRecord) {
        let Some(p) = self.publisher.upgrade() else {
            return;
        };
        let offer = self.offers.fetch_add(1, Ordering::SeqCst) + 1;
        let bunny = site.provider == ProviderKind::Bunny;
        let note = if bunny {
            copy::PURGE_ASKS_KEY
        } else {
            copy::PURGE_NEEDS_BUNNY
        };
        let _ = p.link.ui().upgrade_in_event_loop(move |ui| {
            ui.set_delete_purge(false);
            ui.set_delete_purge_available(bunny);
            ui.set_delete_purge_note(note.into());
        });
        if !bunny {
            return;
        }
        // The link is read on a task of its own: a store that fails leaves the purge unticked.
        let (session, ui, offers) = (p.session.clone(), p.link.ui().clone(), self.offers.clone());
        let (rt, id) = (p.link.rt().clone(), site.id.clone());
        p.link.rt().spawn(async move {
            let found = rt.spawn(async move { session.cdn_link(id).await }).await;
            if !matches!(found, Ok(Some(_))) || offers.load(Ordering::SeqCst) != offer {
                return;
            }
            let _ = ui.upgrade_in_event_loop(|ui| {
                if ui.get_delete_open() {
                    ui.set_delete_purge(true);
                    ui.set_delete_purge_note(SharedString::new());
                }
            });
        });
    }

    fn after(&self, site: SiteRecord, batch: String) {
        let Some(p) = self.publisher.upgrade() else {
            return;
        };
        let rt = p.link.rt().clone();
        rt.spawn(async move { purge_after_delete(p, site, batch).await });
    }
}

/// Purges the URLs a delete removed, once all of it is done. The purger is found (the key asked
/// for, if need be) while the delete runs: the delete never waits for it.
async fn purge_after_delete(p: Arc<Publisher>, site: SiteRecord, batch: String) {
    let finding = {
        let (this, site) = (p.clone(), site.clone());
        p.link
            .rt()
            .spawn(async move { purger_or_key(&this, &site).await })
    };
    let outcome = p.transfers.engine().wait_batch(&batch).await;
    let found = finding
        .await
        .unwrap_or_else(|_| Err(copy::PURGE_NO_KEY.to_owned()));
    if outcome.done.is_empty() {
        return;
    }
    let purger = match found {
        Ok(purger) => purger,
        Err(why) => {
            p.link.status(copy::delete_not_purged(&why));
            return;
        }
    };
    let result = purge(&p, &*purger, &site, PurgeMode::Changed, &outcome.done).await;
    p.link.status(copy::purge_done(&result));
}

/// Wires the CDN key sheet, and hands the delete alert's purge to the transfers.
pub(super) fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    ui.on_cdn_key_save({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |key| {
            if let Some(ui) = ui.upgrade() {
                save_key(&p, &ui, key.to_string());
            }
        }
    });
    ui.on_cdn_key_cancel({
        let p = p.clone();
        move || cancel_key(&p)
    });
    p.transfers.set_delete_purge(Arc::new(DeleteHook {
        publisher: Arc::downgrade(p),
        offers: Arc::new(AtomicU64::new(0)),
    }));
}
