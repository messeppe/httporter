// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

// Nothing consumes these helpers until the file panes are wired up in a later task.
#[allow(dead_code)]
mod format;
#[allow(dead_code)]
mod local_fs;
#[allow(dead_code)]
mod sort;

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
    ui.run()
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
