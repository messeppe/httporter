//! Comfort (I5, T3): the tray icon while transfers run with the window closed (§8.7.8), toasts
//! (§8.7.5), undo (§8.7.7) and the remaining shortcuts (§8.7.3).
//!
//! - this file: [`install`], which wires the window's comfort callbacks, and the two keys that
//!   move the focus or the pane (F6, Alt+Up);
//! - [`tray`]: the tray icon, and hiding the window into it;
//! - [`toast`]: the toast stack;
//! - [`undo`]: the undo list;
//! - [`copy`]: the words of all of the above.

pub(crate) mod copy;
pub mod toast;
pub mod tray;
pub mod undo;

use std::sync::Arc;

use slint::{ComponentHandle, Model};

pub use tray::{hide_into_tray, in_tray, show_from_tray};

use crate::AppWindow;
use crate::manage::Manager;
use crate::transfers::Transfers;

/// Wires the toasts, undo and the I5 shortcuts. Runs once at start, after the other installs. The
/// tray is made when the window goes into it (the quit alert's Keep Running, `transfers.rs`).
pub fn install(ui: &AppWindow, m: &Arc<Manager>, transfers: &Arc<Transfers>) {
    let _ = transfers;
    ui.on_undo({
        let (weak, m) = (ui.as_weak(), m.clone());
        move || {
            if let Some(ui) = weak.upgrade() {
                undo::undo(&ui, &m);
            }
        }
    });
    ui.on_reopen_tab({
        let (weak, m) = (ui.as_weak(), m.clone());
        move || {
            if let Some(ui) = weak.upgrade() {
                undo::reopen_tab(&ui, &m);
            }
        }
    });
    ui.on_switch_pane({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_focus_other_pane();
            }
        }
    });
    ui.on_go_up({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                go_up(&ui);
            }
        }
    });
    ui.on_toast_action({
        let weak = ui.as_weak();
        move |id| {
            if let Some(ui) = weak.upgrade() {
                toast::act(&ui, id);
            }
        }
    });
    ui.on_toast_dismissed({
        let weak = ui.as_weak();
        move |id| {
            if let Some(ui) = weak.upgrade() {
                toast::dismiss(&ui, id);
            }
        }
    });
}

/// Alt+Up: the parent folder of the pane in use, as its Backspace goes (the pane shown, in the
/// single-pane view). Nothing on a publish profile's page, where no pane shows.
fn go_up(ui: &AppWindow) {
    let on_profile = usize::try_from(ui.get_selected_profile())
        .is_ok_and(|profile| profile < ui.get_profiles().row_count());
    if on_profile {
        return;
    }
    let local = if ui.get_view_mode() == 1 {
        ui.get_single_side() == 0
    } else {
        ui.get_local_pane_active()
    };
    if local {
        ui.invoke_local_up();
    } else {
        ui.invoke_remote_up();
    }
}
