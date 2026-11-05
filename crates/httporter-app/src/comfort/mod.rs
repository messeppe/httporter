//! Comfort (I5, T3): the tray icon while transfers run with the window closed (§8.7.8), toasts
//! (§8.7.5), undo (§8.7.7) and the remaining shortcuts (§8.7.3).
//!
//! - this file: [`install`], which wires the window's comfort callbacks;
//! - [`toast`]: the toast stack;
//! - [`undo`]: the undo list;
//! - [`copy`]: the words of all of the above.

mod copy;
pub mod toast;
pub mod undo;

use std::sync::Arc;

use slint::ComponentHandle;

use crate::AppWindow;
use crate::manage::Manager;
use crate::transfers::Transfers;

/// Wires the toasts, undo and the I5 shortcuts. Runs once at start, after the other installs.
pub fn install(ui: &AppWindow, m: &Arc<Manager>, transfers: &Arc<Transfers>) {
    // T3: the tray (with `transfers`), the toast timers and the shortcuts.
    let _ = transfers;
    ui.on_undo({
        let (weak, m) = (ui.as_weak(), m.clone());
        move || {
            if let Some(ui) = weak.upgrade() {
                undo::undo(&ui, &m);
            }
        }
    });
    ui.on_reopen_tab(|| todo!("T3"));
    ui.on_switch_pane(|| todo!("T3"));
    ui.on_go_up(|| todo!("T3"));
    ui.on_toast_action(|_| todo!("T3"));
    ui.on_toast_dismissed(|_| todo!("T3"));
}
