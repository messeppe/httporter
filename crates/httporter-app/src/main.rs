// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use std::error::Error;
use std::sync::Arc;

use chrono::Local;
use drop_hook::{DropBatch, PaneGeometry, PaneLayout, PaneSide};
use httporter_core::Journal;
use httporter_platform::{KeyringStore, SqliteJournal, Store, default_db_path, win};
use httporter_transfer::Engine;
use selection::ActivePane;
use session::{RealFactory, Session};
use slint::Model;
use transfers::Transfers;

mod add_site;
mod binding;
mod copy;
mod drawer_view;
mod drop_hook;
mod format;
mod import;
mod local_fs;
mod manage;
mod publish;
mod selection;
mod session;
mod settings_view;
mod sheets_view;
mod single_instance;
mod site_hub;
mod sort;
mod transfers;
mod viewmodel;

#[cfg(test)]
mod add_site_tests;
#[cfg(test)]
mod keyboard_tests;
#[cfg(test)]
mod lint_tests;
#[cfg(test)]
mod pane_tests;

fn main() -> Result<(), Box<dyn Error>> {
    // A second HTTPorter would take the first one's live queue for unfinished work (review
    // M10): it shows the first one's window instead.
    if !single_instance::first() {
        single_instance::show_first(copy::APP_NAME);
        return Ok(());
    }
    // Before any window exists: Windows reads the identity when it makes the taskbar button.
    // Without it notifications still try again, so a failure here is only noted by them.
    let _ = win::ensure_app_identity();
    let result = run();
    if let Err(error) = &result {
        report_startup_failure(error.as_ref(), show_message_box);
    }
    result
}

/// Tells the user why the app is closing. Release builds have no console (see the
/// `windows_subsystem` attribute), so the error returned from `main` would go unseen: they show
/// it in a message box. Debug builds keep the console, where `main`'s error is printed.
fn report_startup_failure(error: &dyn Error, show: impl FnOnce(&str)) {
    if !cfg!(debug_assertions) {
        show(&copy::startup_error_text(error));
    }
}

/// An OK box with an error mark, titled with the app's name, and no window of ours as its owner.
fn show_message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let (text, title) = (wide(text), wide(copy::APP_NAME));
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call, and a null owner
    // window is allowed.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// `text` as the NUL-terminated UTF-16 that Windows wants.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn run() -> Result<(), Box<dyn Error>> {
    // Software renderer on the winit backend: lowest memory use. A SLINT_BACKEND
    // environment variable overrides this, which tools/measure.ps1 -Backend relies on.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("software".into())
            .select()?;
    }

    // Listings, connection tests, planning and transfers run here, off the UI thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let session = open_session()?;
    let journal = open_journal(&session);
    // Digests of local files not used for 90 days go, off the UI thread.
    session.prune_hash_cache();
    manage::activity_view::open_log(httporter_platform::default_log_dir());
    // Cached previews, thumbnails and edits not touched for a week go, off the UI thread.
    let _ = std::thread::Builder::new()
        .name("prune-cache".into())
        .spawn(|| httporter_platform::prune_cache(std::time::Duration::from_secs(7 * 24 * 3600)));
    let engine = Engine::new(
        rt.handle().clone(),
        journal.clone(),
        session.settings(),
        copy::describe_transfer_error,
    );
    let ui = AppWindow::new()?;
    let transfers = binding::install_app(
        &ui,
        rt.handle().clone(),
        session,
        binding::Engines { engine, journal },
    );
    install_drop(&ui, transfers.clone());
    ui.run()?;
    // Edited files still waiting for their re-upload go first.
    if let Some(manager) = manage::manager() {
        manager.flush_edits();
    }
    // The work in flight stops and the journal catches up (a few seconds at most), so what is
    // left resumes next time.
    transfers.finish();
    // Whatever is still being listed is of no use once the window has closed.
    rt.shutdown_background();
    Ok(())
}

/// The transfer journal, in the same database file as the sites. If it cannot be opened the
/// transfers are kept in memory, won't resume after a restart, and the banner says so (unless it
/// already says the database is unavailable). Jobs finished in an earlier run are let go.
fn open_journal(session: &Session) -> Arc<dyn Journal> {
    let path = default_db_path();
    let journal = match SqliteJournal::open(&path) {
        Ok(journal) => journal,
        Err(e) => {
            if session.notice().is_none() {
                session.set_notice(copy::journal_unavailable(&path, e));
            }
            match SqliteJournal::open_in_memory() {
                Ok(journal) => journal,
                // Not even SQLite in memory: a journal that keeps nothing but still works.
                Err(_) => return Arc::new(httporter_core::MemoryJournal::new()),
            }
        }
    };
    // Only lean housekeeping: a failure leaves the finished jobs for next time.
    let _ = journal.clear_finished();
    Arc::new(journal)
}

/// The session over the saved sites in `%APPDATA%\HTTPorter`. If that database cannot be opened
/// the app still runs, on an empty one that lasts until it closes, and says so in the banner.
fn open_session() -> Result<Session, httporter_platform::StoreError> {
    let path = default_db_path();
    let (store, problem) = match Store::open(&path) {
        Ok(store) => (store, None),
        Err(e) => (
            Store::open_in_memory()?,
            Some(copy::database_unavailable(&path, e)),
        ),
    };
    let session = Session::new(store, Arc::new(KeyringStore), Arc::new(RealFactory), || {
        Local::now().naive_local()
    });
    if let Some(problem) = problem {
        session.set_notice(problem);
    }
    Ok(session)
}

/// Files dragged in from Explorer: the cloud pane lights up while they are over it (if a site is
/// selected; the window sees to that), and so does the folder row they are over. Dropped there,
/// they are uploaded into that folder, or the folder the pane shows ([`dropped`]). Files released
/// where the window cannot tell, or over no pane, are ignored.
fn install_drop(ui: &AppWindow, transfers: Arc<Transfers>) {
    let hover_ui = ui.as_weak();
    let drop_ui = ui.as_weak();
    let lost_ui = ui.as_weak();
    let layout_ui = ui.as_weak();
    drop_hook::install_drop_hook(
        ui.window(),
        move |target| {
            if let Some(ui) = hover_ui.upgrade() {
                let (over_remote, row) = match target {
                    Some((PaneSide::Remote, row)) => (true, row),
                    _ => (false, None),
                };
                ui.set_drop_hover_remote(over_remote);
                let folder = remote_folder(&ui, row);
                ui.set_drop_hover_row(folder.map_or(-1, |(row, _)| row));
            }
        },
        move |batch| {
            if let Some(ui) = drop_ui.upgrade() {
                dropped(&ui, &transfers, batch);
            }
        },
        move || {
            if let Some(ui) = lost_ui.upgrade() {
                binding::show_status(&ui, None, copy::DROP_NOT_LOCATED.to_owned());
            }
        },
        move || {
            layout_ui
                .upgrade()
                .map_or_else(PaneLayout::default, |ui| PaneLayout {
                    local: geometry(&ui.get_local_bounds()),
                    remote: geometry(&ui.get_remote_bounds()),
                })
        },
    );
}

/// Files dropped from Explorer. On the cloud pane, once its site is open, they are uploaded into
/// the folder row they landed on, or the folder the pane shows. On This PC there is nothing to do
/// with them: the status line says where to drop them.
fn dropped(ui: &AppWindow, transfers: &Arc<Transfers>, batch: DropBatch) {
    let site = selected_site_name(ui);
    let status = match (batch.side, site) {
        (PaneSide::Remote, _) if ui.get_can_upload() => {
            transfers.upload_dropped(batch.paths, batch.row);
            return;
        }
        (PaneSide::Remote, None) => copy::DROP_NO_SITE.to_owned(),
        (PaneSide::Remote, Some(site)) => copy::drop_not_ready(&site),
        (PaneSide::Local, site) => copy::drop_status(&batch, site.as_deref(), None),
    };
    // The message belongs to the pane the files were dropped on: only that pane clears it.
    let pane = match batch.side {
        PaneSide::Local => ActivePane::ThisPc,
        PaneSide::Remote => ActivePane::Cloud,
    };
    binding::show_status(ui, Some(pane), status);
}

/// A pane's place in the window as the hook wants it.
fn geometry(bounds: &PaneBounds) -> PaneGeometry {
    PaneGeometry {
        x: bounds.x,
        y: bounds.y,
        w: bounds.width,
        h: bounds.height,
        list_top: bounds.list_top,
        row_height: bounds.row_height,
        scroll_y: bounds.scroll_y,
        rows: usize::try_from(bounds.rows).unwrap_or(0),
    }
}

/// The row of the cloud pane and its name, when `row` is a folder there. A file row is not a
/// target: files dropped on one go to the folder the pane shows.
fn remote_folder(ui: &AppWindow, row: Option<usize>) -> Option<(i32, String)> {
    let index = row?;
    let row = ui.get_remote_rows().row_data(index)?;
    row.folder
        .then(|| i32::try_from(index).ok().map(|i| (i, row.name.to_string())))
        .flatten()
}

/// The name of the site selected in the sidebar, if any.
fn selected_site_name(ui: &AppWindow) -> Option<String> {
    usize::try_from(ui.get_selected_site())
        .ok()
        .and_then(|index| ui.get_sites().row_data(index))
        .map(|site| site.name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    #[test]
    fn release_startup_failure_shows_a_message() {
        let error: Box<dyn Error> = "no display".into();
        let shown = std::cell::RefCell::new(None);
        report_startup_failure(error.as_ref(), |text| {
            *shown.borrow_mut() = Some(text.to_owned());
        });
        // Only release builds have no console to print the error to, so only they show it.
        let expected =
            (!cfg!(debug_assertions)).then(|| "HTTPorter couldn't start.\n\nno display".to_owned());
        assert_eq!(shown.into_inner(), expected);
        assert_eq!(wide("OK"), [79, 75, 0]);
    }

    fn window_with_rows() -> AppWindow {
        i_slint_backend_testing::init_no_event_loop();
        let w = AppWindow::new().unwrap();
        let row = |name: &str, folder: bool| FileRow {
            name: name.into(),
            kind: if folder {
                FileKind::Folder
            } else {
                FileKind::Doc
            },
            folder,
            ..FileRow::default()
        };
        w.set_remote_rows(slint::ModelRc::new(slint::VecModel::from(vec![
            row("wp-content", true),
            row("index.html", false),
        ])));
        w
    }

    /// Explorer's drops land on a folder row only when the row is a folder.
    #[test]
    fn a_drop_targets_a_folder_row_and_never_a_file_row() {
        let w = window_with_rows();
        assert_eq!(
            remote_folder(&w, Some(0)),
            Some((0, "wp-content".to_owned()))
        );
        assert_eq!(remote_folder(&w, Some(1)), None);
        assert_eq!(remote_folder(&w, Some(7)), None);
        assert_eq!(remote_folder(&w, None), None);
    }

    #[test]
    fn the_selected_site_is_named_by_the_sidebar() {
        let w = window_with_rows();
        assert_eq!(selected_site_name(&w), None);
        w.set_sites(slint::ModelRc::new(slint::VecModel::from(vec![
            SiteItem {
                name: "example-shop.com".into(),
                ..SiteItem::default()
            },
            SiteItem {
                name: "media".into(),
                ..SiteItem::default()
            },
        ])));
        w.set_selected_site(1);
        assert_eq!(selected_site_name(&w).as_deref(), Some("media"));
        w.set_selected_site(5);
        assert_eq!(selected_site_name(&w), None);
    }

    #[test]
    fn a_panes_bounds_become_the_drop_hooks_geometry() {
        let bounds = PaneBounds {
            x: 10.0,
            y: 20.0,
            width: 300.0,
            height: 400.0,
            list_top: 60.0,
            row_height: 22.0,
            scroll_y: 44.0,
            rows: 12,
        };
        let g = geometry(&bounds);
        assert_eq!(
            (
                g.x,
                g.y,
                g.w,
                g.h,
                g.list_top,
                g.row_height,
                g.scroll_y,
                g.rows
            ),
            (10.0, 20.0, 300.0, 400.0, 60.0, 22.0, 44.0, 12)
        );
        assert_eq!(geometry(&PaneBounds { rows: -1, ..bounds }).rows, 0);
    }

    #[test]
    fn window_builds_with_defaults() {
        i_slint_backend_testing::init_no_event_loop();
        let w = AppWindow::new().unwrap();
        assert_eq!(w.get_connection_text(), "");
        assert_eq!(w.get_sites().row_count(), 0);
    }

    #[test]
    fn exported_types_match_the_interface() {
        let site = SiteItem {
            id: "a".into(),
            name: "example-shop.com".into(),
            subtitle: "Bunny, Singapore".into(),
            provider: ProviderTile::Bunny,
            mono: "B".into(),
            healthy: true,
        };
        let row = FileRow {
            name: "wp-content".into(),
            kind: FileKind::Folder,
            size: "".into(),
            modified: "".into(),
            folder: true,
            selected: false,
            status: CompareStatus::None,
            thumb: Default::default(),
        };
        assert!(site.healthy && row.folder);
        for provider in [
            ProviderTile::Bunny,
            ProviderTile::S3,
            ProviderTile::R2,
            ProviderTile::B2,
            ProviderTile::Wasabi,
            ProviderTile::DigitalOcean,
            ProviderTile::Minio,
            ProviderTile::Other,
        ] {
            assert_eq!(
                SiteItem {
                    provider,
                    ..SiteItem::default()
                }
                .provider,
                provider
            );
        }
        for kind in [
            FileKind::Folder,
            FileKind::Code,
            FileKind::Image,
            FileKind::Doc,
        ] {
            assert_eq!(
                FileRow {
                    kind,
                    ..FileRow::default()
                }
                .kind,
                kind
            );
        }
    }
}
