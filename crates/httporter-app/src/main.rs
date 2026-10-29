// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use drop_hook::{DropBatch, PaneLayout, PaneSide};

mod drop_hook;
mod format;
mod local_fs;
mod sort;
// Nothing in the running app builds pane rows until the session controller (Task 15) wires the
// panes to the local disk and the cloud site. The allow also keeps the modules above, which only
// the view model uses, from being reported as unused.
#[allow(dead_code)]
mod viewmodel;

#[cfg(test)]
mod keyboard_tests;

fn main() -> Result<(), slint::PlatformError> {
    // Software renderer on the winit backend: lowest memory use. A SLINT_BACKEND
    // environment variable overrides this, which tools/measure.ps1 -Backend relies on.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("software".into())
            .select()?;
    }

    let ui = AppWindow::new()?;
    install_drop(&ui);
    ui.run()
}

/// Files dragged in from Explorer: the cloud pane lights up while they are over it. Dropping is
/// only acknowledged in the status line for now; the session controller (Task 15) uploads them.
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
                ui.set_status_left(drop_status(&batch).into());
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

/// The status line for a drop (placeholder text until uploads exist).
fn drop_status(batch: &DropBatch) -> String {
    match batch.side {
        PaneSide::Remote => {
            let n = batch.paths.len();
            let items = if n == 1 { "item" } else { "items" };
            format!("Uploading arrives in the next build (I2): {n} {items} dropped.")
        }
        PaneSide::Local => "Drop files on the cloud pane to upload them.".into(),
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

    #[test]
    fn a_drop_on_the_cloud_pane_is_counted_and_one_on_this_pc_points_to_the_cloud_pane() {
        let batch = |side, n: usize| DropBatch {
            side,
            paths: (0..n).map(|i| format!(r"C:\f{i}").into()).collect(),
        };
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 1)),
            "Uploading arrives in the next build (I2): 1 item dropped."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Remote, 3)),
            "Uploading arrives in the next build (I2): 3 items dropped."
        );
        assert_eq!(
            drop_status(&batch(PaneSide::Local, 3)),
            "Drop files on the cloud pane to upload them."
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
