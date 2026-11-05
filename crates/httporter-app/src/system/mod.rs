//! System (I5, T4): Settings › Security (the saved passwords and keys), Reset HTTPorter (§11), the
//! opt-in update check (§16) and About.
//!
//! - this file: [`install`] for the main window, [`wire_settings`] for the Settings window,
//!   [`show_general`] and [`show_security`] for their tabs, and the palette's About HTTPorter
//!   and Check for Updates ([`palette_entries`], [`run_palette`]);
//! - [`security`]: Settings › Security;
//! - [`reset`]: the reset, run at the next start;
//! - [`update`]: the release feed;
//! - [`copy`]: the words of all of the above.

mod copy;
pub mod reset;
mod security;
pub mod update;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use httporter_core::ActivityKind;
use slint::ComponentHandle;

use crate::comfort::toast::{self, ToastKind};
use crate::manage::Manager;
use crate::manage::activity_view;
use crate::transfers::Transfers;
use crate::{AppWindow, SettingsWindow};

/// The licence texts of everything HTTPorter is built from, beside the exe.
const NOTICES: &str = "THIRD-PARTY-NOTICES.txt";
/// General's place in the Settings window's row of tabs.
const GENERAL_TAB: i32 = 0;

thread_local! {
    /// The Settings window, once made (on the UI thread): the update check's note and the
    /// palette's About reach it here. Weak: `transfers.rs` keeps the window.
    static SETTINGS: RefCell<Option<slint::Weak<SettingsWindow>>> = const { RefCell::new(None) };
}

/// The Settings window, while it lives.
fn settings_window() -> Option<SettingsWindow> {
    SETTINGS.with(|slot| slot.borrow().as_ref().and_then(slint::Weak::upgrade))
}

/// Says why the reset at this start stopped, if it did, and starts the update check when it is
/// due. Runs once at start, after the other installs.
pub fn install(ui: &AppWindow, m: &Arc<Manager>, transfers: &Arc<Transfers>) {
    let _ = transfers;
    if let Some(failed) = reset::failure() {
        let text = copy::reset_failed(failed);
        activity_view::event(ActivityKind::Site, "", "", false, text.clone(), 0);
        // Once the window is up: the toast stack belongs to it.
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                toast::show(&ui, ToastKind::Warning, text, None);
            }
        });
    }
    if update::due(&m.session.ui_state(), chrono::Utc::now().timestamp()) {
        update::check(m);
    }
}

/// Wires Settings › Security and General's update and About rows (the tray and update switches
/// are saved with General's other rows, `manage/settings_tabs.rs`). Runs each time the window is
/// made, on the first press of the gear.
pub fn wire_settings(window: &SettingsWindow) {
    SETTINGS.with(|slot| *slot.borrow_mut() = Some(window.as_weak()));
    // Each callback finds the window and the manager again: the window outlives neither.
    let on = |f: fn(&SettingsWindow, &Arc<Manager>)| {
        let weak = window.as_weak();
        move || {
            if let (Some(window), Some(m)) = (weak.upgrade(), crate::manage::manager()) {
                f(&window, &m);
            }
        }
    };
    window.on_remove_unused_keys(on(security::remove_unused));
    window.on_open_credential_manager(on(|window, _| security::open_credential_manager(window)));
    window.on_reset_requested(on(security::reset_requested));
    window.on_reset_confirmed(on(security::reset_confirmed));
    window.on_check_now(on(|_, m| update::check(m)));
    window.on_open_licenses(on(|_, m| open_licenses(m)));
    security::watch(window);
}

/// General's I5 rows from what is known now: the version, whether the licence file is there,
/// and whether the update rows can be used, with their note. (The tray and the update switch
/// come from the saved UI state, with General's other rows.)
pub fn show_general(window: &SettingsWindow) {
    window.set_app_version(update::VERSION.into());
    window.set_licenses_available(notices_path().is_some_and(|path| path.is_file()));
    window.set_updates_available(update::RELEASE_REPO.is_some());
    window.set_updates_note(update::note().into());
}

/// Settings › Security, drawn again from what is stored now.
pub fn show_security(window: &SettingsWindow, m: &Arc<Manager>) {
    security::show(window, m);
}

/// The update check's note again, in the Settings window if it is made.
fn show_update_note() {
    if let Some(window) = settings_window() {
        window.set_updates_note(update::note().into());
    }
}

/// `THIRD-PARTY-NOTICES.txt` beside the exe.
fn notices_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(NOTICES))
}

/// Third-Party Licenses…: the notices in their default program (off the UI thread). A failure
/// goes to the status line.
fn open_licenses(m: &Arc<Manager>) {
    let Some(path) = notices_path() else {
        return;
    };
    let link = m.link.clone();
    m.link.rt().spawn_blocking(move || {
        if let Err(e) = httporter_platform::win::open_default(&path) {
            link.status(e);
        }
    });
}

// ---- The command palette ----

/// What a palette row of this module does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
    /// Opens Settings › General, where About is.
    About,
    /// Opens Settings › General and presses Check Now.
    CheckForUpdates,
}

/// A palette row: its title and subtitle, what it does, and why it is off (None: on).
pub struct PaletteEntry {
    pub title: &'static str,
    pub subtitle: &'static str,
    pub command: PaletteCommand,
    pub off: Option<&'static str>,
}

/// About HTTPorter and Check for Updates, for the palette's actions (`manage/palette_view.rs`).
pub fn palette_entries(ui: &AppWindow) -> Vec<PaletteEntry> {
    let settings_off = (!ui.get_settings_available()).then_some(copy::PALETTE_SETTINGS_OFF);
    let updates_off = if update::RELEASE_REPO.is_none() {
        Some(copy::NO_RELEASE_ADDRESS)
    } else {
        settings_off
    };
    vec![
        PaletteEntry {
            title: copy::PALETTE_ABOUT,
            subtitle: copy::PALETTE_ABOUT_NOTE,
            command: PaletteCommand::About,
            off: settings_off,
        },
        PaletteEntry {
            title: copy::PALETTE_CHECK_UPDATES,
            subtitle: copy::PALETTE_CHECK_UPDATES_NOTE,
            command: PaletteCommand::CheckForUpdates,
            off: updates_off,
        },
    ]
}

/// Runs a palette row of this module: Settings opens on General (and checks, for Check for
/// Updates, so that the result shows in the note under Check Now).
pub fn run_palette(ui: &AppWindow, command: PaletteCommand) {
    if !ui.get_settings_available() {
        return;
    }
    ui.invoke_open_settings();
    let Some(window) = settings_window() else {
        return;
    };
    // Changing the tab draws it (`tab-shown`); General already on show was drawn on opening.
    window.set_current_tab(GENERAL_TAB);
    if command == PaletteCommand::CheckForUpdates && update::RELEASE_REPO.is_some() {
        window.invoke_check_now();
    }
}
