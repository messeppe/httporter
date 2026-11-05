//! The sidebar's Favorites: folders of this PC and of the sites, kept to go back to.
//!
//! Add to Favorites (a folder row's menu) stores one; a click on a row goes there, the way the
//! user would: the site is chosen in the sidebar, and the pane opens folder after folder (each
//! listed) until it shows the favorite. The menu's Remove forgets one. A favorite of a site that
//! is no longer saved is not shown.

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use httporter_core::{Favorite, PlaceKind, RemotePath, SiteId};
use slint::{Model, ModelRc, SharedString, VecModel};

use super::{Manager, copy};
use crate::viewmodel::{local_crumbs, remote_crumbs};
use crate::{AppWindow, PlaceItem};

/// How long a pane may take to get to the next folder of a walk to a favorite.
const STEP_WAIT: Duration = Duration::from_secs(15);
/// How often a walk looks whether the pane got there.
const POLL: Duration = Duration::from_millis(40);
/// A walk takes at most this many steps (folders deep, and a few to change drive or site).
const MAX_STEPS: usize = 64;

thread_local! {
    /// The favorites the sidebar shows, in its order (UI thread).
    static FAVORITES: RefCell<Vec<Favorite>> = const { RefCell::new(Vec::new()) };
}

/// Counts the walks to a favorite: a newer one ends the one under way.
static WALKS: AtomicU64 = AtomicU64::new(0);

/// Wires this view's callbacks of `ui` to `m`, and shows the stored favorites.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_add_favorite({
        let m = m.clone();
        move |pane, row| add(&m, pane, row)
    });
    ui.on_favorite_selected({
        let m = m.clone();
        move |index| go(&m, index)
    });
    ui.on_favorite_remove({
        let m = m.clone();
        move |index| remove(&m, index)
    });
    reload(m);
}

/// Reads the favorites and puts them in the sidebar.
fn reload(m: &Arc<Manager>) {
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        let sites = m.session.sites();
        let favorites: Vec<Favorite> = m
            .session
            .favorites()
            .await
            .into_iter()
            .filter(|f| match f.kind {
                PlaceKind::Local => f.local.is_some(),
                PlaceKind::Remote => {
                    f.remote.is_some()
                        && f.site
                            .as_ref()
                            .is_some_and(|id| sites.iter().any(|s| s.id == *id))
                }
            })
            .collect();
        let rows: Vec<(i64, String, String, bool)> = favorites
            .iter()
            .map(|f| {
                let (name, subtitle) = match f.kind {
                    PlaceKind::Local => (f.name.clone(), local_subtitle(f.local.as_deref())),
                    PlaceKind::Remote => {
                        let parent = f
                            .remote
                            .as_ref()
                            .and_then(RemotePath::parent)
                            .and_then(|p| p.name().map(str::to_owned));
                        let site = f
                            .site
                            .as_ref()
                            .and_then(|id| sites.iter().find(|s| s.id == *id))
                            .map(|s| s.name.clone())
                            .unwrap_or_default();
                        (copy::favorite_remote_name(parent.as_deref(), &f.name), site)
                    }
                };
                (f.id, name, subtitle, f.kind == PlaceKind::Remote)
            })
            .collect();
        let _ = m.link.ui().upgrade_in_event_loop(move |ui| {
            FAVORITES.with(|slot| *slot.borrow_mut() = favorites);
            let items: Vec<PlaceItem> = rows
                .into_iter()
                .map(|(id, name, subtitle, remote)| PlaceItem {
                    id: i32::try_from(id).unwrap_or(i32::MAX),
                    name: name.into(),
                    subtitle: subtitle.into(),
                    remote,
                })
                .collect();
            ui.set_favorites(ModelRc::new(VecModel::from(items)));
        });
    });
}

/// A local favorite's subtitle: the folder it is in.
fn local_subtitle(path: Option<&Path>) -> String {
    path.and_then(Path::parent)
        .map(|parent| parent.display().to_string())
        .unwrap_or_default()
}

/// The name a local folder is kept under: its own, or the drive's ("C:").
fn local_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string().trim_end_matches('\\').to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Add to Favorites on folder row `row` of pane `pane` (0 This PC, 1 the cloud pane).
fn add(m: &Arc<Manager>, pane: i32, row: i32) {
    let Ok(row) = usize::try_from(row) else {
        return;
    };
    let favorite = if pane == 0 {
        let Some(path) = m.session.local_folder_at(row) else {
            return;
        };
        Favorite {
            id: 0,
            kind: PlaceKind::Local,
            name: local_name(&path),
            local: Some(path),
            site: None,
            remote: None,
        }
    } else {
        let (Some(path), Some(place)) = (m.session.remote_folder_at(row), m.session.remote_place())
        else {
            return;
        };
        Favorite {
            id: 0,
            kind: PlaceKind::Remote,
            name: path.name().unwrap_or_default().to_owned(),
            local: None,
            site: Some(place.site.id),
            remote: Some(path),
        }
    };
    let known = FAVORITES.with(|slot| {
        slot.borrow().iter().any(|f| {
            f.kind == favorite.kind
                && f.local == favorite.local
                && f.site == favorite.site
                && f.remote == favorite.remote
        })
    });
    if known {
        m.link.status(copy::favorite_exists(&favorite.name));
        return;
    }
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        let name = favorite.name.clone();
        match m.session.add_favorite(favorite).await {
            Ok(_) => {
                m.link.status(copy::favorite_added(&name));
                reload(&m);
            }
            Err(e) => m.link.status(copy::favorite_failed(e)),
        }
    });
}

/// Remove on Favorites row `index`.
fn remove(m: &Arc<Manager>, index: i32) {
    let Some(favorite) = favorite_at(index) else {
        return;
    };
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        match m.session.remove_favorite(favorite.id).await {
            Ok(()) => reload(&m),
            Err(e) => m.link.status(copy::favorite_failed(e)),
        }
    });
}

fn favorite_at(index: i32) -> Option<Favorite> {
    let index = usize::try_from(index).ok()?;
    FAVORITES.with(|slot| slot.borrow().get(index).cloned())
}

/// A click on Favorites row `index`: the pane goes there.
fn go(m: &Arc<Manager>, index: i32) {
    let Some(favorite) = favorite_at(index) else {
        return;
    };
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let walk = WALKS.fetch_add(1, Ordering::SeqCst) + 1;
    // The panes, not a profile's page.
    ui.set_selected_profile(-1);
    match favorite.kind {
        PlaceKind::Local => {
            let Some(target) = favorite.local else {
                return;
            };
            if ui.get_view_mode() == 1 {
                ui.set_single_side(0);
            }
            ui.set_local_pane_active(true);
            let m = m.clone();
            let rt = m.link.rt().clone();
            rt.spawn(async move { walk_local(&m, walk, target, favorite.name).await });
        }
        PlaceKind::Remote => {
            let (Some(site), Some(target)) = (favorite.site, favorite.remote) else {
                return;
            };
            let sites = m.session.sites();
            let Some(record) = sites.iter().find(|s| s.id == site).cloned() else {
                m.link.status(copy::FAVORITE_SITE_GONE.to_owned());
                reload(m);
                return;
            };
            let Some(row) = sites
                .iter()
                .position(|s| s.id == site)
                .and_then(|i| i32::try_from(i).ok())
            else {
                return;
            };
            if ui.get_view_mode() == 1 {
                ui.set_single_side(1);
            }
            ui.set_local_pane_active(false);
            if ui.get_selected_site() != row
                || m.session.remote_place().is_none_or(|p| p.site.id != site)
            {
                ui.set_selected_site(row);
                ui.invoke_site_selected(row);
            }
            let m = m.clone();
            let rt = m.link.rt().clone();
            rt.spawn(async move {
                walk_remote(&m, walk, &site, &record.container, target, favorite.name).await;
            });
        }
    }
}

/// What one step of a walk does in the window.
#[derive(Debug, Clone)]
enum Step {
    /// Opens the folder row of this name.
    Open(String),
    /// The first crumb: the drive's root, or the site's.
    Root,
    /// This PC's Up (from a drive's root, to the list of drives).
    Up,
}

/// How a step went in the window.
enum Ran {
    Done,
    /// The window doesn't show yet what the session does: look again shortly.
    Behind,
    /// The folder to open is not there.
    Missing,
}

/// Runs `f` on the UI thread and hands back what it returned; `None` once the window is gone.
async fn on_ui<T: std::marker::Send + 'static>(
    m: &Manager,
    f: impl FnOnce(&AppWindow) -> T + std::marker::Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    m.link
        .ui()
        .upgrade_in_event_loop(move |ui| {
            let _ = tx.send(f(&ui));
        })
        .ok()?;
    rx.await.ok()
}

/// The drive of `path` ("C:").
fn drive_of(path: &Path) -> Option<String> {
    match path.components().next()? {
        Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().into_owned()),
        _ => None,
    }
}

/// The next step from `shown` (`None`: the list of drives) towards `target`, and where the
/// pane is then (`None`: the list of drives). `None` when there is no way on.
fn local_step(shown: Option<&Path>, target: &Path) -> Option<(Step, Option<PathBuf>)> {
    let Some(dir) = shown else {
        let drive = drive_of(target)?;
        let root = PathBuf::from(format!("{drive}\\"));
        return Some((Step::Open(drive), Some(root)));
    };
    if let Ok(rest) = target.strip_prefix(dir)
        && let Some(Component::Normal(next)) = rest.components().next()
    {
        return Some((
            Step::Open(next.to_string_lossy().into_owned()),
            Some(dir.join(next)),
        ));
    }
    let dir_drive = drive_of(dir)?;
    if dir.parent().is_none() {
        // At a drive's root, and the target is on another drive.
        return Some((Step::Up, None));
    }
    Some((Step::Root, Some(PathBuf::from(format!("{dir_drive}\\")))))
}

/// Whether the walk numbered `walk` is still the latest.
fn current(walk: u64) -> bool {
    WALKS.load(Ordering::SeqCst) == walk
}

/// Takes This PC to `target`, one folder at a time.
async fn walk_local(m: &Arc<Manager>, walk: u64, target: PathBuf, name: String) {
    let mut steps = 0;
    let mut since = Instant::now();
    while current(walk) && steps < MAX_STEPS {
        let Some(shown) = m.session.local_shows() else {
            // A listing is on its way.
            if since.elapsed() > STEP_WAIT {
                return;
            }
            tokio::time::sleep(POLL).await;
            continue;
        };
        if shown.as_deref() == Some(target.as_path()) {
            return;
        }
        let Some((step, then)) = local_step(shown.as_deref(), &target) else {
            m.link.status(copy::favorite_missing(&name));
            return;
        };
        let crumbs = shown.as_deref().map(local_crumbs).unwrap_or_default();
        let ran = on_ui(m, {
            let step = step.clone();
            move |ui| {
                let on_screen: Vec<String> = ui
                    .get_local_crumbs()
                    .iter()
                    .map(|c| c.to_string())
                    .collect();
                if on_screen != crumbs || ui.get_local_loading() {
                    return Ran::Behind;
                }
                match step {
                    Step::Open(folder) => match folder_row(&ui.get_local_rows(), &folder) {
                        Some(row) => {
                            ui.invoke_open_local(row);
                            Ran::Done
                        }
                        None => Ran::Missing,
                    },
                    Step::Root => {
                        ui.invoke_local_crumb_clicked(0);
                        Ran::Done
                    }
                    Step::Up => {
                        ui.invoke_local_up();
                        Ran::Done
                    }
                }
            }
        })
        .await;
        match ran {
            None => return,
            Some(Ran::Missing) => {
                m.link.status(copy::favorite_missing(&name));
                return;
            }
            Some(Ran::Behind) => {
                if since.elapsed() > STEP_WAIT {
                    return;
                }
                tokio::time::sleep(POLL).await;
                continue;
            }
            Some(Ran::Done) => {}
        }
        steps += 1;
        // Until the pane is where the step leads (or it gives up: the listing failed).
        since = Instant::now();
        loop {
            if !current(walk) {
                return;
            }
            if m.session.local_shows() == Some(then.clone()) {
                break;
            }
            if since.elapsed() > STEP_WAIT {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
        since = Instant::now();
    }
}

/// The index of the folder row called `name` (any case).
fn folder_row(rows: &ModelRc<crate::FileRow>, name: &str) -> Option<i32> {
    let name = SharedString::from(name);
    rows.iter()
        .position(|row| row.folder && row.name == name)
        .or_else(|| {
            rows.iter().position(|row| {
                row.folder && row.name.to_lowercase() == name.as_str().to_lowercase()
            })
        })
        .and_then(|i| i32::try_from(i).ok())
}

/// Takes the cloud pane to `target` of `site` (whose container is `container`), one folder at
/// a time, once the site has opened.
async fn walk_remote(
    m: &Arc<Manager>,
    walk: u64,
    site: &SiteId,
    container: &str,
    target: RemotePath,
    name: String,
) {
    let mut steps = 0;
    let mut since = Instant::now();
    while current(walk) && steps < MAX_STEPS {
        let dir = match m.session.remote_shows() {
            Some((open, dir)) if open == *site => dir,
            _ => {
                // Opening, listing, or another site still shown.
                if since.elapsed() > STEP_WAIT {
                    return;
                }
                tokio::time::sleep(POLL).await;
                continue;
            }
        };
        if dir == target {
            return;
        }
        let (step, then) = if target.is_within(&dir) {
            let Some(next) = target.segments().nth(dir.segments().count()) else {
                return;
            };
            let Ok(then) = dir.join(next) else {
                return;
            };
            (Step::Open(next.to_owned()), then)
        } else {
            (Step::Root, RemotePath::root())
        };
        let crumbs = remote_crumbs(container, &dir);
        let ran = on_ui(m, {
            let step = step.clone();
            move |ui| {
                let on_screen: Vec<String> = ui
                    .get_remote_crumbs()
                    .iter()
                    .map(|c| c.to_string())
                    .collect();
                if on_screen != crumbs || ui.get_remote_loading() {
                    return Ran::Behind;
                }
                match step {
                    Step::Open(folder) => match folder_row(&ui.get_remote_rows(), &folder) {
                        Some(row) => {
                            ui.invoke_open_remote(row);
                            Ran::Done
                        }
                        None => Ran::Missing,
                    },
                    Step::Root | Step::Up => {
                        ui.invoke_remote_crumb_clicked(0);
                        Ran::Done
                    }
                }
            }
        })
        .await;
        match ran {
            None => return,
            Some(Ran::Missing) => {
                m.link.status(copy::favorite_missing(&name));
                return;
            }
            Some(Ran::Behind) => {
                if since.elapsed() > STEP_WAIT {
                    return;
                }
                tokio::time::sleep(POLL).await;
                continue;
            }
            Some(Ran::Done) => {}
        }
        steps += 1;
        since = Instant::now();
        loop {
            if !current(walk) {
                return;
            }
            if m.session.remote_shows() == Some((site.clone(), then.clone())) {
                break;
            }
            if since.elapsed() > STEP_WAIT {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
        since = Instant::now();
    }
}
