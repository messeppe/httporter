// SPDX-License-Identifier: MIT OR Apache-2.0

//! Settings › Security: the passwords and keys HTTPorter saved in Windows Credential Manager,
//! counted by what they belong to, Remove Unused Keys (M7), Open Credential Manager, and Reset
//! HTTPorter… (§11).
//!
//! Only keys [`httporter_platform::CredentialStore::keys`] lists are counted or removed:
//! HTTPorter's own targets, never another program's. A key is unused when no saved site or Bunny
//! account has its id. While the site database can't be read in full (the banner says so), no key
//! is called unused: a site it couldn't read could be the one a key is for.

use std::collections::HashSet;
use std::sync::Arc;

use httporter_platform::{CredentialKey, CredentialKind};
use slint::{ComponentHandle, Model};

use super::{copy, reset};
use crate::SettingsWindow;
use crate::manage::Manager;
use crate::session::blocking;

/// HTTPorter's keys in Credential Manager, by what they belong to.
#[derive(Debug, Default)]
struct Counts {
    sites: usize,
    accounts: usize,
    /// Keys whose site or account is gone.
    unused: Vec<CredentialKey>,
    /// Every saved site and account could be read, so that `unused` means what it says.
    sure: bool,
}

/// Draws the tab from what is stored now: whether Reset may be pressed (at once), then the
/// counts (Credential Manager is read off the UI thread).
pub(super) fn show(window: &SettingsWindow, m: &Arc<Manager>) {
    show_reset(window, m);
    let failed = reset::failure().map(copy::reset_failed);
    window.set_security_message(failed.unwrap_or_default().into());
    count(window, m, None);
}

/// Counts the keys and shows the counts, and `message` (if any) under Remove Unused Keys, or
/// why no key can be called unused now.
fn count(window: &SettingsWindow, m: &Arc<Manager>, message: Option<String>) {
    let (weak, manager) = (window.as_weak(), m.clone());
    m.link.rt().spawn(async move {
        let counted = counts(&manager).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            let (counts, error) = match counted {
                Ok(counts) => (counts, None),
                Err(e) => (Counts::default(), Some(e)),
            };
            let unsure = (error.is_none() && !counts.sure).then(|| copy::KEYS_UNSURE.to_owned());
            let int = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
            window.set_site_keys(int(counts.sites));
            window.set_account_keys(int(counts.accounts));
            window.set_unused_keys(int(counts.unused.len()));
            if let Some(text) = error.or(message).or(unsure) {
                window.set_security_message(text.into());
            }
        });
    });
}

/// Reads HTTPorter's keys and sorts them by whether their site or account is still saved.
async fn counts(m: &Manager) -> Result<Counts, String> {
    let creds = m.session.creds();
    let keys = blocking(move || creds.keys())
        .await
        .map_err(copy::keys_unreadable)?;
    // The sites and accounts saved now, not as the window last showed them.
    let sites: HashSet<String> = m.session.sites().into_iter().map(|s| s.id.0).collect();
    let accounts: HashSet<String> = m
        .session
        .bunny_accounts()
        .await
        .into_iter()
        .map(|a| a.id.0)
        .collect();
    // The database couldn't be opened, or rows of it read: every key might still be in use.
    let sure = m.session.notice().is_none();
    let mut counts = Counts {
        sure,
        ..Counts::default()
    };
    for key in keys {
        let known = match key.kind() {
            CredentialKind::Site => sites.contains(key.id()),
            CredentialKind::Account => accounts.contains(key.id()),
            // `keys()` lists none of these; were there one, it would not be called unused.
            CredentialKind::Other => true,
        };
        if !known && sure {
            counts.unused.push(key);
        } else if key.kind() == CredentialKind::Account {
            counts.accounts += 1;
        } else {
            counts.sites += 1;
        }
    }
    Ok(counts)
}

/// Remove Unused Keys: counts again (the window's numbers may be old), deletes the keys found
/// unused, says how many went, and counts once more.
pub(super) fn remove_unused(window: &SettingsWindow, m: &Arc<Manager>) {
    window.set_security_message("".into());
    let (weak, manager) = (window.as_weak(), m.clone());
    m.link.rt().spawn(async move {
        let message = match counts(&manager).await {
            Err(e) => e,
            Ok(counts) if !counts.sure => copy::KEYS_UNSURE.to_owned(),
            Ok(counts) => {
                let creds = manager.session.creds();
                blocking(move || {
                    let mut removed = 0;
                    for key in &counts.unused {
                        if let Err(e) = creds.delete(key) {
                            return copy::keys_not_removed(removed, e);
                        }
                        removed += 1;
                    }
                    copy::keys_removed(removed)
                })
                .await
            }
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            if let Some(m) = crate::manage::manager() {
                count(&window, &m, Some(message));
            }
        });
    });
}

/// Open Credential Manager: Control Panel's page for it.
pub(super) fn open_credential_manager(window: &SettingsWindow) {
    // Control Panel from the Windows folder, not whatever `control.exe` the PATH finds first.
    let control = std::env::var_os("SystemRoot")
        .map(|root| {
            std::path::PathBuf::from(root)
                .join("System32")
                .join("control.exe")
        })
        .unwrap_or_else(|| "control.exe".into());
    let opened = std::process::Command::new(control)
        .args(["/name", "Microsoft.CredentialManager"])
        .spawn();
    if let Err(e) = opened {
        window.set_security_message(copy::credential_manager_failed(e).into());
    }
}

/// Whether nothing would be lost by a reset now: no transfer queued, running, paused or waiting
/// to be written, none from last time waiting in the resume banner, and no file being edited.
fn reset_allowed(m: &Manager) -> bool {
    if m.transfers.engine().is_busy() {
        return false;
    }
    m.link
        .ui()
        .upgrade()
        .is_none_or(|ui| ui.get_edits().row_count() == 0 && ui.get_resume_banner_text().is_empty())
}

/// How often the tab looks again whether a reset may start, while it is shown.
const RESET_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
/// Security's place in the Settings window's row of tabs.
const SECURITY_TAB: i32 = 3;

thread_local! {
    /// Keeps Reset HTTPorter… on or off as transfers and edits come and go (UI thread).
    static RESET_WATCH: slint::Timer = slint::Timer::default();
}

/// Looks again every second, while the Security tab is shown, whether a reset may start: the
/// button turns on once the transfers are done, without the tab being opened again.
pub(super) fn watch(window: &SettingsWindow) {
    let weak = window.as_weak();
    RESET_WATCH.with(|timer| {
        timer.start(slint::TimerMode::Repeated, RESET_EVERY, move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if window.get_current_tab() != SECURITY_TAB {
                return;
            }
            if let Some(m) = crate::manage::manager() {
                show_reset(&window, &m);
            }
        });
    });
}

/// Reset HTTPorter… on or off, and why.
fn show_reset(window: &SettingsWindow, m: &Manager) -> bool {
    let allowed = reset_allowed(m);
    window.set_reset_allowed(allowed);
    window.set_reset_note(if allowed { "" } else { copy::RESET_BUSY }.into());
    allowed
}

/// Reset HTTPorter… was pressed: the alert shows while a reset is allowed (it hides when work
/// starts meanwhile, since the tab shows it only then).
pub(super) fn reset_requested(window: &SettingsWindow, m: &Arc<Manager>) {
    show_reset(window, m);
}

/// The alert's Reset HTTPorter: once more only if nothing would be lost; then the marker, a new
/// HTTPorter that waits for this one, and quitting. If the new one can't be started, this one
/// stays and says the reset happens at its next start.
pub(super) fn reset_confirmed(window: &SettingsWindow, m: &Arc<Manager>) {
    if !show_reset(window, m) {
        return;
    }
    if let Err(e) = reset::schedule() {
        window.set_security_message(copy::reset_not_prepared(e).into());
        return;
    }
    if let Err(e) = reset::restart() {
        window.set_security_message(copy::restart_failed(e).into());
        return;
    }
    let _ = window.hide();
    // Nothing is running (checked above), so there is nothing to ask about or wait for: the
    // event loop ends, `main` lets go of the window and the process exits, which frees the
    // mutex the new HTTPorter waits for.
    if let Some(ui) = m.link.ui().upgrade() {
        let _ = ui.hide();
    }
    let _ = slint::quit_event_loop();
}
