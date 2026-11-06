// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduled publish (§8.8.2, I6, T3): the profile page's Schedule card, the Schedule sheet, and
//! each profile's Task Scheduler task (`httporter_platform::win::Scheduler`, in `\HTTPorter\`).
//!
//! - Saving the sheet stores `ProfileExtras::schedule` and upserts the task; Off removes it, and
//!   so does deleting the profile.
//! - At start, a task whose profile no longer exists is removed (`\HTTPorter\Publish *` only).
//! - The card shows the next run and the last `HistorySource::Scheduled` publish; a task deleted
//!   in Task Scheduler while the schedule is on shows "The scheduled task is missing", with
//!   Recreate.
//!
//! Tests use `FakeScheduler`: building HTTPorter never creates a real task.
//!
//! T1 is the skeleton: the Schedule sheet opens and closes, nothing else happens yet.

use std::sync::Arc;

use super::Publisher;
use crate::AppWindow;

/// Wires the Schedule card and sheet of `ui` to `p` (T3). Nothing yet: the card says "Off".
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    let _ = (ui, p);
}
