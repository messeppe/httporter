// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use std::sync::Arc;

use chrono::Local;
use drop_hook::{DropBatch, PaneLayout, PaneSide};
use httporter_platform::{KeyringStore, Store, default_db_path};
use session::{RealFactory, Session, items};
use slint::Model;

mod add_site;
mod binding;
mod drop_hook;
mod format;
mod local_fs;
mod session;
mod sort;
mod viewmodel;

#[cfg(test)]
mod add_site_tests;
#[cfg(test)]
mod keyboard_tests;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Software renderer on the winit backend: lowest memory use. A SLINT_BACKEND
    // environment variable overrides this, which tools/measure.ps1 -Backend relies on.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("software".into())
            .select()?;
    }

    // Listings and connection tests run here, off the UI thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ui = AppWindow::new()?;
    binding::install(&ui, rt.handle().clone(), open_session()?);
    install_drop(&ui);
    ui.run()?;
    // Whatever is still being listed is of no use once the window has closed.
    rt.shutdown_background();
    Ok(())
}

/// The session over the saved sites in `%APPDATA%\HTTPorter`. If that database cannot be opened
/// the app still runs, on an empty one that lasts until it closes, and says so in the banner.
fn open_session() -> Result<Session, httporter_platform::StoreError> {
    let path = default_db_path();
    let (store, problem) = match Store::open(&path) {
        Ok(store) => (store, None),
        Err(e) => (
            Store::open_in_memory()?,
            Some(format!(
                "Couldn't open the site database at {}: {e}. Sites added now won't be kept.",
                path.display()
            )),
        ),
    };
    let mut session = Session::new(store, Arc::new(KeyringStore), Arc::new(RealFactory), || {
        Local::now().naive_local()
    });
    if let Some(problem) = problem {
        session.set_notice(problem);
    }
    Ok(session)
}

/// Files dragged in from Explorer: the cloud pane lights up while they are over it. Dropping is
/// only acknowledged in the status line for now; uploads arrive in I2.
fn install_drop(ui: &AppWindow) {
    let hover_ui = ui.as_weak();
    let drop_ui = ui.as_weak();
    let layout_ui = ui.as_weak();
    drop_hook::install_drop_hook(
        ui.window(),
        move |over_remote| {
            if let Some(ui) = hover_ui.upgrade() {
                ui.set_drop_hover_remote(over_remote);
            }
        },
        move |batch| {
            if let Some(ui) = drop_ui.upgrade() {
                let site = selected_site_name(&ui);
                ui.set_status_left(drop_status(&batch, site.as_deref()).into());
            }
        },
        move || {
            layout_ui
                .upgrade()
                .map_or_else(PaneLayout::default, |ui| PaneLayout {
                    split_x: ui.get_pane_split(),
                    stacked: ui.get_panes_stacked(),
                    remote_y: ui.get_remote_pane_y(),
                })
        },
    );
}

/// The name of the site selected in the sidebar, if any.
fn selected_site_name(ui: &AppWindow) -> Option<String> {
    usize::try_from(ui.get_selected_site())
        .ok()
        .and_then(|index| ui.get_sites().row_data(index))
        .map(|site| site.name.to_string())
}

/// The status line for a drop on a pane, naming the selected site (placeholder text until uploads
/// exist). With no site selected there is no cloud pane to point to, so the text says to add one.
fn drop_status(batch: &DropBatch, site: Option<&str>) -> String {
    let dropped = items(batch.paths.len());
    match (batch.side, site) {
        (PaneSide::Remote, Some(site)) => {
            format!("Uploading arrives in the next build (I2): {dropped} dropped on {site}.")
        }
        (PaneSide::Remote, None) => {
            format!("Uploading arrives in the next build (I2): {dropped} dropped.")
        }
        (PaneSide::Local, Some(site)) => format!("Drop files on the {site} pane to upload them."),
        (PaneSide::Local, None) => "Add a site, then drop files on its pane to upload them.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;

    #[test]
    fn window_builds_with_defaults() {
        i_slint_backend_testing::init_no_event_loop();
        let w = AppWindow::new().unwrap();
        assert_eq!(w.get_connection_text(), "");
        assert_eq!(w.get_sites().row_count(), 0);
    }

    fn batch(side: PaneSide, n: usize) -> DropBatch {
        DropBatch {
            side,
            paths: (0..n).map(|i| format!(r"C:\f{i}").into()).collect(),
        }
    }

    #[test]
    fn a_drop_on_the_cloud_pane_is_counted_and_one_on_this_pc_points_to_the_site() {
        let site = Some("example-shop.com");
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 1), site),
            "Uploading arrives in the next build (I2): 1 item dropped on example-shop.com."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 3), site),
            "Uploading arrives in the next build (I2): 3 items dropped on example-shop.com."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Local, 3), site),
            "Drop files on the example-shop.com pane to upload them."
        );
    }

    #[test]
    fn with_no_site_the_drop_status_names_none() {
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 2), None),
            "Uploading arrives in the next build (I2): 2 items dropped."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Local, 2), None),
            "Add a site, then drop files on its pane to upload them."
        );
    }

    #[test]
    fn exported_types_match_the_interface() {
        let site = SiteItem {
            id: "a".into(),
            name: "example-shop.com".into(),
            subtitle: "Bunny, Singapore".into(),
            mono: "B".into(),
            healthy: true,
        };
        let row = FileRow {
            name: "wp-content".into(),
            kind: FileKind::Folder,
            size: "".into(),
            modified: "".into(),
            folder: true,
        };
        assert!(site.healthy && row.folder);
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
