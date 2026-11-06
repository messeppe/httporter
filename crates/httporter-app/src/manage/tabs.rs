// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tabs and synchronized browsing (T7).
//!
//! A tab is a [`TabState`]: the site and the folder each pane shows. The panes always show the
//! current tab, and [`navigated`] keeps it up to date as they move; switching to another tab
//! opens its places (its site first, then its folder). The strip shows while there are two or
//! more. Ctrl+T duplicates the current tab; Ctrl+W closes it, never the last one.
//!
//! Synchronized browsing: opening a subfolder in one pane opens the folder of the same name in
//! the other, and going up (or back to a folder above) takes the other pane up as far. When the
//! other pane has no such folder it turns itself off and says why. Moves the app makes itself (a
//! tab's places, a mirrored folder) are not mirrored back.
//!
//! The tabs and synchronized browsing are kept in the [`UiState`]; the tabs come back at start
//! when `restore_tabs` is on. Nothing is saved before the saved state has been read.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use httporter_core::{RemotePath, SiteId, TabState, UiState};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use super::{Manager, Side, copy, search_view};
use crate::binding::{Change, show_status};
use crate::comfort::undo::UndoAction;
use crate::selection::ActivePane;
use crate::session::blocking;
use crate::{AppWindow, TabItem};

/// What this view remembers between callbacks (T7).
#[derive(Default)]
pub struct TabsState {
    tabs: Vec<TabState>,
    current: usize,
    /// The saved state has been read (or there was none to read): changes are saved from now on.
    restored: bool,
    sync: bool,
    /// Where each pane was when [`navigated`] last looked, to tell how it moved.
    seen_remote: Option<(SiteId, RemotePath)>,
    seen_local: Option<Option<PathBuf>>,
    /// Moves the app started itself (a tab's places, a mirrored folder) still on their way, per
    /// pane: what they bring is not mirrored.
    quiet_remote: u32,
    quiet_local: u32,
    /// A tab's site is opening: the folder to go to once it has.
    opening: Option<(SiteId, RemotePath)>,
}

impl TabsState {
    /// The current tab, made from the panes' places if there is none yet.
    fn current_tab(&mut self) -> &mut TabState {
        if self.tabs.is_empty() {
            self.tabs.push(TabState::default());
            self.current = 0;
        }
        self.current = self.current.min(self.tabs.len() - 1);
        &mut self.tabs[self.current]
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires this view's callbacks of `ui` to `m` (T7), and brings back the saved tabs.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_tab_new({
        let (m, weak) = (m.clone(), ui.as_weak());
        move || {
            if let Some(ui) = weak.upgrade() {
                new_tab(&ui, &m);
            }
        }
    });
    ui.on_tab_selected({
        let (m, weak) = (m.clone(), ui.as_weak());
        move |index| {
            if let (Some(ui), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                select_tab(&ui, &m, index);
            }
        }
    });
    ui.on_tab_close({
        let (m, weak) = (m.clone(), ui.as_weak());
        move |index| {
            if let (Some(ui), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                close_tab(&ui, &m, index);
            }
        }
    });
    ui.on_toggle_sync({
        let (m, weak) = (m.clone(), ui.as_weak());
        move || {
            if let Some(ui) = weak.upgrade() {
                toggle_sync(&ui, &m);
            }
        }
    });
    restore(ui, m);
}

/// A pane of `ui` was put on screen (`binding.rs` calls it after `apply_local` / `apply_remote`):
/// the tab follows it, and with synchronized browsing on the other pane follows too. Ends the
/// window's search when the cloud pane has left it.
pub fn navigated(ui: &AppWindow, side: Side) {
    let Some(m) = super::manager() else { return };
    search_view::navigated(ui, &m, side);
    match side {
        Side::Remote => remote_moved(ui, &m),
        Side::Local => local_moved(ui, &m),
    }
}

/// The cloud pane shows a folder (again).
fn remote_moved(ui: &AppWindow, m: &Arc<Manager>) {
    // Loading, a search, or no site: no folder to follow.
    let Some(now) = m.session.remote_shows() else {
        return;
    };
    let mut st = lock(&m.tabs);
    if let Some((site, dir)) = st.opening.clone() {
        if site == now.0 {
            // The tab's site is open: on to the tab's folder.
            st.opening = None;
            st.seen_remote = Some(now.clone());
            if dir != now.1 {
                drop(st);
                go_remote(m, dir);
                return;
            }
        } else if chosen_site(ui).as_ref() == Some(&site) {
            // Still opening.
            return;
        } else {
            // Another site was chosen meanwhile.
            st.opening = None;
        }
    }
    let before = st.seen_remote.replace(now.clone());
    let mirror = st.sync && st.quiet_remote == 0;
    let tab = st.current_tab();
    let changed = tab.site.as_ref() != Some(&now.0) || tab.remote.as_ref() != Some(&now.1);
    tab.site = Some(now.0.clone());
    tab.remote = Some(now.1.clone());
    if changed {
        show_tabs(ui, m, &st);
        save(m, &st);
    }
    drop(st);
    if let (true, Some(before)) = (mirror, before) {
        mirror_from_remote(ui, m, &before, &now);
    }
}

/// The This PC pane shows a folder (or the drives) again.
fn local_moved(ui: &AppWindow, m: &Arc<Manager>) {
    let Some(now) = m.session.local_shows() else {
        return;
    };
    let mut st = lock(&m.tabs);
    let before = st.seen_local.replace(now.clone());
    let mirror = st.sync && st.quiet_local == 0;
    let tab = st.current_tab();
    let changed = tab.local != now;
    tab.local.clone_from(&now);
    if changed {
        show_tabs(ui, m, &st);
        save(m, &st);
    }
    drop(st);
    if let (true, Some(Some(before)), Some(now)) = (mirror, before, now) {
        mirror_from_local(ui, m, &before, &now);
    }
}

// ---- Synchronized browsing ----

/// The cloud pane went from `before` to `now`: This PC follows.
fn mirror_from_remote(
    ui: &AppWindow,
    m: &Arc<Manager>,
    before: &(SiteId, RemotePath),
    now: &(SiteId, RemotePath),
) {
    if before.0 != now.0 || before.1 == now.1 {
        return;
    }
    // At the list of drives (or on its way somewhere) there is nothing to follow with.
    let Some(Some(local)) = m.session.local_shows() else {
        return;
    };
    if now.1.parent().as_ref() == Some(&before.1) {
        let Some(name) = now.1.name() else { return };
        let target = local.join(name);
        if target.is_dir() {
            go_local(m, Some(target));
        } else {
            stop_sync(ui, m, name, &local_label(&local), ActivePane::Cloud);
        }
    } else if before.1.is_within(&now.1) {
        let up = before.1.segments().count() - now.1.segments().count();
        let target = local
            .ancestors()
            .nth(up)
            .or_else(|| local.ancestors().last())
            .map(Path::to_path_buf);
        if let Some(target) = target.filter(|t| *t != local) {
            go_local(m, Some(target));
        }
    }
}

/// This PC went from `before` to `now`: the cloud pane follows.
fn mirror_from_local(ui: &AppWindow, m: &Arc<Manager>, before: &Path, now: &Path) {
    if before == now {
        return;
    }
    let Some((site, dir)) = m.session.remote_shows() else {
        return;
    };
    if now.parent() == Some(before) {
        let Some(name) = now.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        match m.session.remote_child_folder(&name) {
            Some(target) => go_remote(m, target),
            None => {
                let other = match dir.name() {
                    Some(folder) => folder.to_owned(),
                    None => m.session.site(&site).map(|s| s.name).unwrap_or_default(),
                };
                stop_sync(ui, m, &name, &other, ActivePane::ThisPc);
            }
        }
    } else if before.starts_with(now) {
        let up = before.components().count() - now.components().count();
        let mut target = dir.clone();
        for _ in 0..up {
            match target.parent() {
                Some(parent) => target = parent,
                None => break,
            }
        }
        if target != dir {
            go_remote(m, target);
        }
    }
}

/// Synchronized browsing turns itself off: `folder` isn't in `other`. The status line says so,
/// until `pane` (the one that moved) lists again.
fn stop_sync(ui: &AppWindow, m: &Manager, folder: &str, other: &str, pane: ActivePane) {
    let mut st = lock(&m.tabs);
    st.sync = false;
    save(m, &st);
    drop(st);
    ui.set_sync_browsing(false);
    show_status(ui, Some(pane), copy::sync_stopped(folder, other));
}

/// The toolbar's sync button.
fn toggle_sync(ui: &AppWindow, m: &Manager) {
    let mut st = lock(&m.tabs);
    st.sync = !st.sync;
    // Mirroring starts from where the panes are now.
    st.seen_remote = m.session.remote_shows();
    st.seen_local = m.session.local_shows();
    ui.set_sync_browsing(st.sync);
    save(m, &st);
}

// ---- Moving the panes ----

/// Takes the cloud pane to `dir` of the open site; what it brings is not mirrored.
fn go_remote(m: &Arc<Manager>, dir: RemotePath) {
    lock(&m.tabs).quiet_remote += 1;
    let ticket = m.session.remote_ticket();
    let (session, link, manager) = (m.session.clone(), m.link.clone(), m.clone());
    m.link.rt().spawn(async move {
        let started = {
            let link = link.clone();
            move |view| link.show_remote_as(view, Change::Loading)
        };
        let view = session.goto_remote(ticket, dir, started).await;
        link.show_remote_as(view, Change::Listed);
        // After the view (the event loop keeps the order): the pane's moves are the user's again.
        let _ = link.ui().upgrade_in_event_loop(move |_| {
            let mut st = lock(&manager.tabs);
            st.quiet_remote = st.quiet_remote.saturating_sub(1);
        });
    });
}

/// As [`go_remote`], for This PC (`None` is the list of drives).
fn go_local(m: &Arc<Manager>, dir: Option<PathBuf>) {
    lock(&m.tabs).quiet_local += 1;
    let ticket = m.session.local_ticket();
    let (session, link, manager) = (m.session.clone(), m.link.clone(), m.clone());
    m.link.rt().spawn(async move {
        let started = {
            let link = link.clone();
            move |view| link.show_local_as(view, Change::Loading)
        };
        let view = session.goto_local(ticket, dir, started).await;
        link.show_local_as(view, Change::Listed);
        let _ = link.ui().upgrade_in_event_loop(move |_| {
            let mut st = lock(&manager.tabs);
            st.quiet_local = st.quiet_local.saturating_sub(1);
        });
    });
}

/// Opens the places of `tab`: its site (if another is chosen) and folder, and This PC's folder.
fn open_tab(ui: &AppWindow, m: &Arc<Manager>, tab: &TabState) {
    if let Some(site) = &tab.site {
        let dir = tab.remote.clone().unwrap_or_else(RemotePath::root);
        if chosen_site(ui).as_ref() == Some(site) {
            let open = m.session.remote_shows().is_some_and(|(s, _)| s == *site);
            if open || m.session.remote_searching() {
                go_remote(m, dir);
            } else {
                // Still opening: `navigated` goes on once it has.
                lock(&m.tabs).opening = Some((site.clone(), dir));
            }
        } else if let Some(index) = m
            .session
            .sites()
            .iter()
            .position(|s| s.id == *site)
            .and_then(|i| i32::try_from(i).ok())
        {
            lock(&m.tabs).opening = Some((site.clone(), dir));
            ui.set_selected_profile(-1);
            ui.set_selected_site(index);
            ui.invoke_site_selected(index);
        }
    }
    if m.session.local_shows() != Some(tab.local.clone()) {
        go_local(m, tab.local.clone());
    }
}

// ---- The tabs ----

/// Ctrl+T and the strip's +: a copy of the current tab, next to it.
fn new_tab(ui: &AppWindow, m: &Manager) {
    let mut st = lock(&m.tabs);
    let tab = st.current_tab().clone();
    let at = st.current + 1;
    st.tabs.insert(at, tab);
    st.current = at;
    show_tabs(ui, m, &st);
    save(m, &st);
}

/// A tab of the strip, or Ctrl+(Shift+)Tab.
fn select_tab(ui: &AppWindow, m: &Arc<Manager>, index: usize) {
    let mut st = lock(&m.tabs);
    if index >= st.tabs.len() || index == st.current {
        ui.set_current_tab(i32::try_from(st.current).unwrap_or(0));
        return;
    }
    st.current = index;
    st.opening = None;
    let tab = st.tabs[index].clone();
    show_tabs(ui, m, &st);
    save(m, &st);
    drop(st);
    open_tab(ui, m, &tab);
}

/// A tab's close button, a middle click, or Ctrl+W. The last tab stays. A toast offers to undo it
/// (I5), as do Ctrl+Z and Ctrl+Shift+T.
fn close_tab(ui: &AppWindow, m: &Arc<Manager>, index: usize) {
    let mut st = lock(&m.tabs);
    if st.tabs.len() < 2 || index >= st.tabs.len() {
        return;
    }
    let closed = st.tabs.remove(index);
    let reopen = index == st.current;
    if index < st.current || st.current >= st.tabs.len() {
        st.current = st.current.saturating_sub(1).min(st.tabs.len() - 1);
    }
    let tab = st.tabs[st.current].clone();
    if reopen {
        st.opening = None;
    }
    show_tabs(ui, m, &st);
    save(m, &st);
    drop(st);
    if reopen {
        open_tab(ui, m, &tab);
    }
    crate::comfort::undo::push(ui, UndoAction::ReopenTab { tab: closed, index });
}

/// Undoes a closed tab (I5: Ctrl+Z, its toast's Undo, Ctrl+Shift+T): `tab` is back where it was
/// in the strip (or last, if the strip is shorter now), and current.
pub(crate) fn reopen(ui: &AppWindow, m: &Arc<Manager>, tab: TabState, index: usize) {
    let mut st = lock(&m.tabs);
    let at = index.min(st.tabs.len());
    st.tabs.insert(at, tab.clone());
    st.current = at;
    st.opening = None;
    show_tabs(ui, m, &st);
    save(m, &st);
    drop(st);
    open_tab(ui, m, &tab);
}

/// A tab's title, as the strip shows it.
pub(crate) fn title(m: &Manager, tab: &TabState) -> String {
    tab_item(m, tab).title.to_string()
}

/// Puts the tabs in the window (the strip shows with two or more).
fn show_tabs(ui: &AppWindow, m: &Manager, st: &TabsState) {
    let items: Vec<TabItem> = st.tabs.iter().map(|tab| tab_item(m, tab)).collect();
    let model = ui.get_tabs();
    if model.row_count() == items.len() {
        for (i, item) in items.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&item) {
                model.set_row_data(i, item);
            }
        }
    } else {
        ui.set_tabs(ModelRc::new(VecModel::from(items)));
    }
    ui.set_current_tab(i32::try_from(st.current).unwrap_or(0));
}

fn tab_item(m: &Manager, tab: &TabState) -> TabItem {
    let site = tab.site.as_ref().and_then(|id| m.session.site(id));
    let folder = tab.remote.as_ref().and_then(RemotePath::name);
    let local = tab.local.as_deref().map(local_label);
    let path = tab.local.as_deref().map(|p| p.display().to_string());
    TabItem {
        title: copy::tab_title(
            site.as_ref().map(|s| s.name.as_str()),
            folder,
            local.as_deref(),
        )
        .into(),
        subtitle: copy::tab_subtitle(path.as_deref()).into(),
    }
}

// ---- Keeping them ----

/// Reads the saved state off the UI thread, then brings back synchronized browsing and the tabs.
fn restore(ui: &AppWindow, m: &Arc<Manager>) {
    let (session, weak) = (m.session.clone(), ui.as_weak());
    m.link.rt().spawn(async move {
        let state = blocking(move || session.ui_state()).await;
        let _ = weak.upgrade_in_event_loop(move |ui| restored(&ui, state));
    });
}

fn restored(ui: &AppWindow, state: UiState) {
    let Some(m) = super::manager() else { return };
    let mut st = lock(&m.tabs);
    st.restored = true;
    st.sync = state.sync;
    st.seen_remote = m.session.remote_shows();
    st.seen_local = m.session.local_shows();
    ui.set_sync_browsing(state.sync);
    if !state.restore_tabs || state.tabs.is_empty() {
        st.current_tab();
        show_tabs(ui, &m, &st);
        return;
    }
    st.current = state.current.min(state.tabs.len() - 1);
    st.tabs = state.tabs;
    let tab = st.tabs[st.current].clone();
    show_tabs(ui, &m, &st);
    drop(st);
    open_tab(ui, &m, &tab);
}

/// Saves the tabs and synchronized browsing (off the UI thread), once the saved state is in.
fn save(m: &Manager, st: &TabsState) {
    if !st.restored {
        return;
    }
    let (tabs, current, sync) = (st.tabs.clone(), st.current, st.sync);
    let session = m.session.clone();
    m.link.rt().spawn_blocking(move || {
        let mut state = session.ui_state();
        if state.tabs == tabs && state.current == current && state.sync == sync {
            return;
        }
        state.tabs = tabs;
        state.current = current;
        state.sync = sync;
        session.save_ui_state(&state);
    });
}

// ---- Helpers ----

/// The site chosen in the sidebar, if any.
fn chosen_site(ui: &AppWindow) -> Option<SiteId> {
    usize::try_from(ui.get_selected_site())
        .ok()
        .and_then(|index| ui.get_sites().row_data(index))
        .map(|site| SiteId(site.id.to_string()))
}

/// A local folder as a tab or a message names it: its name, or the drive.
fn local_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}
