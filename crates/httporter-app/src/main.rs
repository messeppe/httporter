// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use std::error::Error;
use std::sync::Arc;

use chrono::Local;
use drop_hook::PaneLayout;
use httporter_platform::{KeyringStore, Store, default_db_path};
use session::{RealFactory, Session};
use slint::Model;

mod add_site;
mod binding;
mod copy;
mod drawer_view;
mod drop_hook;
mod format;
mod local_fs;
mod session;
mod sheets_view;
mod sort;
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
/// selected; the window sees to that). Dropping is only acknowledged in the status line for now;
/// uploads arrive in I2.
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
                ui.set_status_message(copy::drop_status(&batch, site.as_deref()).into());
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
