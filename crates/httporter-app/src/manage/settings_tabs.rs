// SPDX-License-Identifier: MIT OR Apache-2.0

//! Settings › General, Sites and Advanced, and read-only sites (I4, T8).
//!
//! - **General:** the appearance (`Theme.appearance` on both windows), the view to start in,
//!   Restore tabs and Ask before previewing big files, all kept in the saved [`UiState`]; the
//!   preview cache's size, and Clear.
//! - **Sites** (master-detail): each site's [`SiteExtras`] (read-only, the public base URL, how
//!   long pre-signed links last), its CDN link, and the conflict choices remembered for it, each
//!   with Forget (review M3); Export Sites… and Import Sites…, whose file holds no secret (the
//!   session writes and reads it).
//! - **Advanced:** Show Activity Log…, Open Log Folder, the checksum cache and Clear; the
//!   renderer (I5), kept in the saved [`UiState`] for the next start (`renderer.rs`).
//!
//! Every edit applies at once, as on the Transfers tab: the window (`settings_tabs.slint`) only
//! draws the form and hands it back; this file checks it, stores it and draws the answer.
//!
//! **Read-only:** [`read_only`] answers [`Manager::read_only`] for the window's every write (the
//! toolbar, the menus, the drops, paste, rename, move, edit); `transfers.rs` refuses the write
//! batches of a read-only site as well. While the site chosen in the sidebar is read-only the
//! cloud pane has a banner, and its subtitle ends "· Read-only".
//!
//! The words come from `copy.rs`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use httporter_core::{ProviderKind, SiteExtras, SiteId, UiState, ViewMode};
use httporter_platform::{cache_bytes, cache_dir, default_log_dir, win};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{Manager, copy};
use crate::renderer;
use crate::session::{Session, blocking};
use crate::{AppWindow, SettingsWindow, SiteSettingsData, SiteSettingsRow, Theme};

/// Whether the stored UI state and site extras may be read without the user asking: at start
/// (the saved appearance), whenever the window redraws (the read-only marks, the sidebar's
/// "· Read-only") and before a batch is sent ([`site_read_only`]). On since T4's store is
/// merged; off, read-only sites would not be enforced.
pub const STORED_EXTRAS_READY: bool = true;

/// The tabs this file draws, by their place in the window's row of tabs.
const GENERAL_TAB: i32 = 0;
const SITES_TAB: i32 = 1;
/// Drawn by `system/` (I5, T4).
const SECURITY_TAB: i32 = 3;
const ADVANCED_TAB: i32 = 5;

/// How long a pre-signed link lasts, by the place of the choice in Settings › Sites' drop-down:
/// 15 minutes, 1 hour (the default), 1 day, 7 days.
const EXPIRY_SECS: [u32; 4] = [900, 3600, 86_400, 604_800];
/// The default's place in [`EXPIRY_SECS`].
const DEFAULT_EXPIRY: i32 = 1;

/// The cache folders the preview cache's Clear empties. The `edit` copies (open in a program) and
/// the `move` staging (in use by a move) stay: the 7-day prune at start takes them.
const CLEARED_CACHES: [&str; 2] = ["preview", "thumbs"];

/// What this view remembers between callbacks.
#[derive(Default)]
pub struct SettingsState {
    /// The sites of Settings › Sites, in the order of its rows.
    rows: Vec<SiteId>,
    /// A file picker (Export Sites…, Import Sites…) is open.
    picking: Arc<AtomicBool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Gives the main window the saved appearance (at start).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    if !STORED_EXTRAS_READY {
        return;
    }
    let appearance = m.session.ui_state().appearance;
    ui.global::<Theme>()
        .set_appearance(i32::from(appearance.min(2)));
}

/// Wires the General, Sites and Advanced tabs of a new Settings window (`transfers.rs` calls it
/// from `settings_window()`, on the UI thread). Without a [`Manager`] (no main window) the tabs
/// stay empty.
pub fn wire(window: &SettingsWindow) {
    let Some(m) = super::manager() else {
        return;
    };
    // The main window has the saved appearance already.
    if let Some(ui) = m.link.ui().upgrade() {
        let appearance = ui.global::<Theme>().get_appearance();
        window.global::<Theme>().set_appearance(appearance);
    }
    let manager = Arc::downgrade(&m);

    // Each callback finds the window and the manager again: the window outlives neither.
    let on = |f: fn(&SettingsWindow, &Arc<Manager>)| {
        let (weak, manager) = (window.as_weak(), manager.clone());
        move || {
            if let (Some(window), Some(m)) = (weak.upgrade(), manager.upgrade()) {
                f(&window, &m);
            }
        }
    };
    window.on_general_edited(on(general_edited));
    window.on_clear_preview_cache(on(clear_preview_cache));
    window.on_export_sites(on(export_sites));
    window.on_import_sites(on(import_sites));
    window.on_clear_hash_cache(on(clear_hash_cache));
    window.on_show_activity(on(show_activity));
    window.on_open_log_folder(on(open_log_folder));
    window.on_renderer_edited(on(renderer_edited));

    let (weak, mgr) = (window.as_weak(), manager.clone());
    window.on_tab_shown(move |tab| {
        if let (Some(window), Some(m)) = (weak.upgrade(), mgr.upgrade()) {
            show_tab(&window, &m, tab);
        }
    });
    let (weak, mgr) = (window.as_weak(), manager.clone());
    window.on_site_chosen(move |row| {
        if let (Some(window), Some(m)) = (weak.upgrade(), mgr.upgrade()) {
            show_site(&window, &m, usize::try_from(row).ok());
        }
    });
    let (weak, mgr) = (window.as_weak(), manager.clone());
    window.on_site_form_save(move |form| {
        if let (Some(window), Some(m)) = (weak.upgrade(), mgr.upgrade()) {
            save_site(&window, &m, form);
        }
    });
    let (weak, mgr) = (window.as_weak(), manager);
    window.on_site_forget_choice(move |upload| {
        if let (Some(window), Some(m)) = (weak.upgrade(), mgr.upgrade()) {
            forget_choice(&window, &m, upload);
        }
    });
    // I6 (T5): Settings › Sites › Zone and Bucket.
    super::zones::wire_settings(window);
}

/// The Settings window is being opened (`transfers.rs`): the tab on show, if it is one of these,
/// is drawn again from what is stored now.
pub fn shown(window: &SettingsWindow) {
    if let Some(m) = super::manager() {
        show_tab(window, &m, window.get_current_tab());
    }
}

/// See [`Manager::read_only`].
///
/// `binding.rs` asks about the site chosen in the sidebar whenever the site, its folder or the
/// selection changes, so the window's read-only marks follow from here: the banner over the cloud
/// pane, and Publish turned off (a profile page decides for itself).
pub(super) fn read_only(m: &Manager, site: &SiteId) -> bool {
    let read_only = site_read_only(&m.session, site);
    // `upgrade` answers only on the UI thread; elsewhere there is no window to mark.
    if let Some(ui) = m.link.ui().upgrade()
        && selected_site(&ui).as_ref() == Some(site)
    {
        show_read_only(&ui, &m.session, site, read_only);
    }
    read_only
}

/// Whether `site` is read-only, for every check made without the user asking (see
/// [`STORED_EXTRAS_READY`]): the window's marks, the sidebar, and the batches `transfers.rs`
/// refuses.
pub fn site_read_only(session: &Session, site: &SiteId) -> bool {
    STORED_EXTRAS_READY && session.site_extras(site).read_only
}

/// The text of the status line when a write to the read-only `site` was refused.
pub fn read_only_refused(site: &str) -> String {
    copy::read_only_refused(site)
}

/// The site chosen in the sidebar.
fn selected_site(ui: &AppWindow) -> Option<SiteId> {
    usize::try_from(ui.get_selected_site())
        .ok()
        .and_then(|row| ui.get_sites().row_data(row))
        .map(|site| SiteId(site.id.to_string()))
}

/// The banner, and Publish, for the site chosen in the sidebar.
fn show_read_only(ui: &AppWindow, session: &Session, site: &SiteId, read_only: bool) {
    let banner = if read_only {
        session
            .site(site)
            .map(|site| copy::read_only_banner(&site.name))
            .unwrap_or_default()
    } else {
        String::new()
    };
    if ui.get_read_only_banner_text() != banner.as_str() {
        ui.set_read_only_banner_text(banner.into());
    }
    if read_only && ui.get_selected_profile() < 0 && ui.get_can_publish() {
        ui.set_can_publish(false);
    }
}

/// The main window again after a site's settings changed: the sidebar's subtitles, the toolbar,
/// the banner. Nothing is listed again.
fn refresh_main_window(m: &Manager) {
    m.link.show_remote(m.session.remote_view());
}

fn show_tab(window: &SettingsWindow, m: &Arc<Manager>, tab: i32) {
    match tab {
        GENERAL_TAB => show_general(window, m),
        SITES_TAB => show_sites(window, m),
        SECURITY_TAB => crate::system::show_security(window, m),
        ADVANCED_TAB => show_advanced(window, m),
        _ => {}
    }
}

// ---- General ----

fn show_general(window: &SettingsWindow, m: &Arc<Manager>) {
    let state = m.session.ui_state();
    window.set_appearance(i32::from(state.appearance.min(2)));
    window.set_start_view(match state.view {
        ViewMode::TwoPanes => 0,
        ViewMode::Inspector => 1,
    });
    window.set_restore_tabs(state.restore_tabs);
    window.set_ask_big_preview(state.ask_big_preview);
    // I5 (T4): the tray, the update check and About.
    window.set_tray(state.tray);
    window.set_check_updates(state.check_updates);
    crate::system::show_general(window);
    show_preview_cache(window, m, None);
}

/// An edit on General: kept in the saved [`UiState`] at once; the appearance applies to both
/// windows at once, the rest at the next start, preview or close (the tray).
fn general_edited(window: &SettingsWindow, m: &Arc<Manager>) {
    let appearance = u8::try_from(window.get_appearance().clamp(0, 2)).unwrap_or(0);
    let state = UiState {
        appearance,
        view: if window.get_start_view() == 1 {
            ViewMode::Inspector
        } else {
            ViewMode::TwoPanes
        },
        restore_tabs: window.get_restore_tabs(),
        ask_big_preview: window.get_ask_big_preview(),
        tray: window.get_tray(),
        check_updates: window.get_check_updates(),
        ..m.session.ui_state()
    };
    m.session.save_ui_state(&state);
    let appearance = i32::from(appearance);
    window.global::<Theme>().set_appearance(appearance);
    if let Some(ui) = m.link.ui().upgrade() {
        ui.global::<Theme>().set_appearance(appearance);
    }
}

/// Measures the cache (on a blocking thread) and shows its size, or `failed` instead.
fn show_preview_cache(window: &SettingsWindow, m: &Manager, failed: Option<String>) {
    let weak = window.as_weak();
    m.link.rt().spawn_blocking(move || {
        let text = failed.unwrap_or_else(|| copy::preview_cache(cache_bytes()));
        let _ = weak.upgrade_in_event_loop(move |window| {
            window.set_preview_cache_text(text.into());
        });
    });
}

fn clear_preview_cache(window: &SettingsWindow, m: &Arc<Manager>) {
    let weak = window.as_weak();
    let manager = Arc::downgrade(m);
    m.link.rt().spawn_blocking(move || {
        let failed =
            CLEARED_CACHES
                .iter()
                .find_map(|kind| match std::fs::remove_dir_all(cache_dir(kind)) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        Some(copy::cache_not_cleared(e))
                    }
                    _ => None,
                });
        let _ = weak.upgrade_in_event_loop(move |window| {
            if let Some(m) = manager.upgrade() {
                show_preview_cache(&window, &m, failed);
            }
        });
    });
}

// ---- Sites ----

/// Lists the sites again, keeping the one chosen (by id) when it is still there, else the first.
fn show_sites(window: &SettingsWindow, m: &Arc<Manager>) {
    let items = m.session.site_items();
    let rows: Vec<SiteSettingsRow> = items
        .iter()
        .map(|item| SiteSettingsRow {
            id: item.id.as_str().into(),
            name: item.name.as_str().into(),
            subtitle: item.subtitle.as_str().into(),
            read_only: m.session.site_extras(&SiteId(item.id.clone())).read_only,
        })
        .collect();
    let ids: Vec<SiteId> = items.iter().map(|item| SiteId(item.id.clone())).collect();
    let chosen = window.get_site_form().id;
    let row = ids
        .iter()
        .position(|id| id.0 == chosen.as_str())
        .or((!ids.is_empty()).then_some(0));
    lock(&m.settings).rows = ids;
    window.set_site_rows(ModelRc::new(VecModel::from(rows)));
    window.set_site_current(row.and_then(|row| i32::try_from(row).ok()).unwrap_or(-1));
    show_site(window, m, row);
}

/// Puts the site of row `row` in the form: what is known at once, then its CDN link and the
/// choices remembered for it (read from the site database).
fn show_site(window: &SettingsWindow, m: &Arc<Manager>, row: Option<usize>) {
    let site = row
        .and_then(|row| lock(&m.settings).rows.get(row).cloned())
        .and_then(|id| m.session.site(&id));
    // I6 (T5): its Zone or Bucket section.
    super::zones::show_site(window, m, site.as_ref());
    let Some(site) = site else {
        window.set_site_form(SiteSettingsData::default());
        return;
    };
    let extras = m.session.site_extras(&site.id);
    let s3 = site.provider == ProviderKind::S3;
    window.set_site_form(SiteSettingsData {
        id: site.id.0.as_str().into(),
        read_only: extras.read_only,
        public_url: extras.public_base_url.unwrap_or_default().into(),
        presign: s3,
        presign_expiry: expiry_index(extras.presign_expiry_secs),
        ..SiteSettingsData::default()
    });

    let (session, weak) = (m.session.clone(), window.as_weak());
    m.link.rt().spawn(async move {
        let cdn = if s3 {
            String::new()
        } else {
            cdn_text(&session, &site.id).await
        };
        let upload = session.remembered_conflicts(&site.id, true).await;
        let download = session.remembered_conflicts(&site.id, false).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            let mut form = window.get_site_form();
            // Another site was chosen meanwhile.
            if form.id.as_str() != site.id.0 {
                return;
            }
            form.cdn = cdn.into();
            form.upload_choice = upload.map(copy::conflict_choice).unwrap_or_default().into();
            form.download_choice = download
                .map(copy::conflict_choice)
                .unwrap_or_default()
                .into();
            window.set_site_form(form);
        });
    });
}

/// "Pull Zone 7654321 · key saved", or that no pull zone is linked. Whether the key is kept is
/// read from the accounts table, never from the credential store.
async fn cdn_text(session: &Session, site: &SiteId) -> String {
    let Some(link) = session.cdn_link(site.clone()).await else {
        return copy::NO_PULL_ZONE.to_owned();
    };
    if link.zones.is_empty() {
        return copy::NO_PULL_ZONE.to_owned();
    }
    let kept = session
        .bunny_accounts()
        .await
        .iter()
        .any(|account| account.id == link.account);
    let zones: Vec<u64> = link.zones.iter().map(|zone| zone.id).collect();
    copy::cdn_linked(&zones, kept)
}

/// The place in the expiry drop-down of a stored expiry (0 is the default hour).
fn expiry_index(secs: u32) -> i32 {
    EXPIRY_SECS
        .iter()
        .position(|&known| known == secs)
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(DEFAULT_EXPIRY)
}

/// A public base URL as typed, checked: `https://`, a host, no user name, query or fragment; it
/// gets the `/` it ends with if it has none. `None` when it isn't such an address.
fn public_base_url(text: &str) -> Option<String> {
    let text = text.trim();
    let scheme = text.get(..8)?;
    if !scheme.eq_ignore_ascii_case("https://") {
        return None;
    }
    let rest = &text[8..];
    let host = rest.split('/').next().unwrap_or_default();
    let refused = |c: char| c.is_whitespace() || matches!(c, '@' | '?' | '#' | '\\');
    if host.is_empty() || host.starts_with(':') || rest.contains(refused) {
        return None;
    }
    let mut url = format!("https://{rest}");
    if !url.ends_with('/') {
        url.push('/');
    }
    Some(url)
}

/// The form was edited (every edit applies at once): read-only and the expiry are stored; the
/// public base URL too when it is an `https://…/` address, else it stays as stored and the form
/// says what is wrong under it.
fn save_site(window: &SettingsWindow, m: &Arc<Manager>, form: SiteSettingsData) {
    let id = SiteId(form.id.to_string());
    if m.session.site(&id).is_none() {
        return;
    }
    let before = m.session.site_extras(&id);
    let typed = form.public_url.trim().to_owned();
    let (public_base_url, error) = if typed.is_empty() {
        (None, None)
    } else {
        match public_base_url(&typed) {
            Some(url) => (Some(url), None),
            None => (
                before.public_base_url.clone(),
                Some(copy::PUBLIC_URL_INVALID),
            ),
        }
    };
    let expiry = usize::try_from(form.presign_expiry)
        .ok()
        .and_then(|index| EXPIRY_SECS.get(index))
        .copied()
        .unwrap_or(0);
    let extras = SiteExtras {
        read_only: form.read_only,
        public_base_url,
        presign_expiry_secs: expiry,
    };
    // What the form shows: the address as stored, or as typed when it was refused.
    let shown_url = match error {
        Some(_) => typed,
        None => extras.public_base_url.clone().unwrap_or_default(),
    };
    let (session, weak, manager) = (m.session.clone(), window.as_weak(), Arc::downgrade(m));
    m.link.rt().spawn(async move {
        let saved = if extras == before {
            Ok(())
        } else {
            session.save_site_extras(id.clone(), extras.clone()).await
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            let mut shown = window.get_site_form();
            if shown.id.as_str() != id.0 {
                return;
            }
            match saved {
                Ok(()) => {
                    shown.read_only = extras.read_only;
                    shown.presign_expiry = expiry_index(extras.presign_expiry_secs);
                    shown.public_url = shown_url.into();
                    shown.error = error.unwrap_or_default().into();
                }
                Err(e) => shown.error = copy::site_settings_not_saved(e).into(),
            }
            window.set_site_form(shown);
            if let Some(m) = manager.upgrade() {
                mark_row(&window, &m, &id);
                refresh_main_window(&m);
            }
        });
    });
}

/// Redraws the row of site `id` (its subtitle and lock) after its settings changed.
fn mark_row(window: &SettingsWindow, m: &Manager, id: &SiteId) {
    let rows = window.get_site_rows();
    let Some(row) = rows.iter().position(|row| row.id.as_str() == id.0) else {
        return;
    };
    let Some(item) = m
        .session
        .site_items()
        .into_iter()
        .find(|item| item.id == id.0)
    else {
        return;
    };
    if let Some(mut data) = rows.row_data(row) {
        data.subtitle = item.subtitle.into();
        data.read_only = m.session.site_extras(id).read_only;
        rows.set_row_data(row, data);
    }
}

/// Forget beside a remembered choice: the Transfers setting applies to the site again.
fn forget_choice(window: &SettingsWindow, m: &Arc<Manager>, upload: bool) {
    let id = SiteId(window.get_site_form().id.to_string());
    if id.0.is_empty() {
        return;
    }
    let (session, weak) = (m.session.clone(), window.as_weak());
    m.link.rt().spawn(async move {
        let forgotten = session.forget_conflicts(id.clone(), upload).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            let mut form = window.get_site_form();
            if form.id.as_str() != id.0 {
                return;
            }
            match forgotten {
                Ok(()) if upload => form.upload_choice = SharedString::new(),
                Ok(()) => form.download_choice = SharedString::new(),
                Err(e) => form.error = copy::site_settings_not_saved(e).into(),
            }
            window.set_site_form(form);
        });
    });
}

/// Opens a file picker on a blocking thread, unless one is open already; `None` when it was
/// cancelled (or not opened).
async fn pick<T: Send + 'static>(
    picking: Arc<AtomicBool>,
    ask: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    if picking.swap(true, Ordering::SeqCst) {
        return None;
    }
    let answer = blocking(ask).await;
    picking.store(false, Ordering::SeqCst);
    answer
}

/// Export Sites…: the saved sites, their extras and profiles, without a secret, to a `.json`.
fn export_sites(window: &SettingsWindow, m: &Arc<Manager>) {
    let picking = lock(&m.settings).picking.clone();
    let (session, weak) = (m.session.clone(), window.as_weak());
    m.link.rt().spawn(async move {
        let filters: [(&str, &[&str]); 1] = [(copy::SITES_FILE_TYPE, &["json"])];
        let Some(path) = pick(picking, move || {
            win::pick_save_file(copy::EXPORT_SITES_TITLE, copy::SITES_FILE_NAME, &filters)
        })
        .await
        else {
            return;
        };
        let file = file_name(&path);
        let text = match session.export_sites(path).await {
            Ok(n) => copy::sites_exported(n, &file),
            Err(e) => copy::sites_not_exported(e),
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            window.set_sites_message(text.into());
        });
    });
}

/// Import Sites…: adds the sites of a file that are not saved yet; their passwords and keys are
/// entered in Edit Site.
fn import_sites(window: &SettingsWindow, m: &Arc<Manager>) {
    let picking = lock(&m.settings).picking.clone();
    let (session, weak, manager) = (m.session.clone(), window.as_weak(), Arc::downgrade(m));
    m.link.rt().spawn(async move {
        let filters: [(&str, &[&str]); 1] = [(copy::SITES_FILE_TYPE, &["json"])];
        let Some(path) = pick(picking, move || {
            win::pick_file(copy::IMPORT_SITES_TITLE, &filters)
        })
        .await
        else {
            return;
        };
        let (text, added) = match session.import_sites(path).await {
            Ok(report) => (
                copy::sites_imported(report.added, report.skipped, &report.need_secret),
                report.added > 0,
            ),
            Err(e) => (copy::sites_not_imported(e), false),
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            window.set_sites_message(text.into());
            if let Some(m) = manager.upgrade()
                && added
            {
                show_sites(&window, &m);
                refresh_main_window(&m);
            }
        });
    });
}

/// The last part of `path`, for the message.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---- Advanced ----

fn show_advanced(window: &SettingsWindow, m: &Arc<Manager>) {
    // I5: the renderer, and whether the GPU one couldn't start.
    let state = m.session.ui_state();
    window.set_renderer(i32::from(state.renderer == renderer::GPU_RENDERER));
    window.set_renderer_fell_back(renderer::fell_back(&state));
    let (session, weak) = (m.session.clone(), window.as_weak());
    m.link.rt().spawn(async move {
        let text = match session.hash_count().await {
            Ok(files) => copy::hash_cache(files),
            Err(e) => copy::hash_cache_failed(e),
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            window.set_hash_cache_text(text.into());
        });
    });
}

/// The renderer was chosen: kept for the next start. Choosing one (GPU again, say) also forgets
/// that the GPU renderer couldn't start last time, so the next start tries what was chosen.
fn renderer_edited(window: &SettingsWindow, m: &Arc<Manager>) {
    let renderer = if window.get_renderer() == i32::from(renderer::GPU_RENDERER) {
        renderer::GPU_RENDERER
    } else {
        0
    };
    let state = UiState {
        renderer,
        gpu_failed: false,
        ..m.session.ui_state()
    };
    m.session.save_ui_state(&state);
    window.set_renderer_fell_back(renderer::fell_back(&state));
}

fn clear_hash_cache(window: &SettingsWindow, m: &Arc<Manager>) {
    let (session, weak, manager) = (m.session.clone(), window.as_weak(), Arc::downgrade(m));
    m.link.rt().spawn(async move {
        let cleared = session.clear_hashes().await;
        let _ = weak.upgrade_in_event_loop(move |window| match cleared {
            Ok(()) => {
                if let Some(m) = manager.upgrade() {
                    show_advanced(&window, &m);
                }
            }
            Err(e) => window.set_hash_cache_text(copy::hash_cache_failed(e).into()),
        });
    });
}

/// Show Activity Log…: the main window comes forward with its activity sheet open.
fn show_activity(_window: &SettingsWindow, m: &Arc<Manager>) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    ui.set_activity_open(true);
    ui.invoke_activity_filter_changed(ui.get_activity_filter());
    bring_forward(&ui);
}

fn bring_forward(ui: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    ui.window()
        .with_winit_window(|window| window.focus_window());
}

/// Open Log Folder: the activity log's folder in Explorer (made first if nothing was logged
/// yet). What went wrong goes to the main window's status line.
fn open_log_folder(_window: &SettingsWindow, m: &Arc<Manager>) {
    let link = m.link.clone();
    m.link.rt().spawn_blocking(move || {
        let dir = default_log_dir();
        let opened = std::fs::create_dir_all(&dir)
            .map_err(|e| e.to_string())
            .and_then(|()| win::open_default(&dir));
        if let Err(e) = opened {
            link.status(copy::log_folder_failed(e));
        }
    });
}
