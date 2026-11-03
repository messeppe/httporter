//! What the sheets, alerts and banners show: the conflict sheet, the delete, Remove Site and quit
//! alerts, the New Folder sheet's checks and the four banners, as plain functions of what the
//! engine reports, and the code that puts them in the window.
//!
//! The words come from `copy.rs` and the numbers from `format.rs`; this module chooses which
//! words go with which facts (singular and plural, upload and download, Bunny and S3) and lays the
//! conflict table out. Every function is tested without a window.
//!
//! [`install`] wires the callbacks for a window without a transfer engine (the window's tests):
//! nothing opens a sheet or an alert and the banners stay empty, so they do nothing, except
//! choosing a default action on the conflict sheet, which sets its rows, and Create on the New
//! Folder sheet, which checks the name against the cloud pane's listing. With an engine,
//! `transfers.rs` replaces them; the Remove Site alert and Edit Site… are `binding.rs`'s (they
//! act on the saved sites).

use std::collections::HashSet;

use chrono::{DateTime, FixedOffset, Local, Utc};
use httporter_core::{
    CompareBy, Conflict, ConflictPolicy, DeleteSummary, Direction, ProviderKind, Resolution,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::format::{format_date, format_size};
use crate::viewmodel::{kind_for, slint_kind};
use crate::{AppWindow, ConflictContents, ConflictRow, copy};

// ---- The conflict sheet ----

/// A row's own choice: replace the file, leave it alone, or keep both (the new one under another
/// name). The numbers are what the window's rows hold in `action`.
pub const ACTION_REPLACE: i32 = 0;
pub const ACTION_SKIP: i32 = 1;
pub const ACTION_KEEP_BOTH: i32 = 2;

/// The sheet's default action, which sets every row at once. The numbers are what the sheet gives
/// back in `mode`.
pub const MODE_REPLACE_IF_DIFFERENT: i32 = 0;
pub const MODE_REPLACE_ALL: i32 = 1;
pub const MODE_SKIP_ALL: i32 = 2;
/// The fourth segment; any number the sheet does not know means it too.
#[allow(dead_code)]
pub const MODE_KEEP_BOTH: i32 = 3;

/// Everything the conflict sheet says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSheetView {
    pub title: String,
    pub body: String,
    /// "For all 12:", before the segmented control.
    pub for_all_label: String,
    /// "7 differ · 5 identical (same SHA-256)".
    pub summary: String,
    /// The headings of the two sides, the side the files come from first.
    pub col_source: String,
    pub col_target: String,
    pub rows: Vec<ConflictRowData>,
    pub remember_label: String,
    pub cancel_label: String,
}

/// One row of the conflict table, in the order of the conflicts it was made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRowData {
    pub name: String,
    /// The side the file comes from, "357.2 KB · Today 09:58".
    pub source: String,
    /// The side it would replace.
    pub target: String,
    /// "Identical", "Different" or "Unknown".
    pub contents: String,
    /// 0 Replace, 1 Skip, 2 Keep both.
    pub action_index: i32,
}

/// What a default action does to one row: under "Replace if different" an identical file is left
/// alone and anything else (different, or not known to be the same) is replaced.
pub fn default_action(mode: i32, same: Option<bool>) -> i32 {
    match mode {
        MODE_REPLACE_IF_DIFFERENT if same == Some(true) => ACTION_SKIP,
        MODE_REPLACE_IF_DIFFERENT | MODE_REPLACE_ALL => ACTION_REPLACE,
        MODE_SKIP_ALL => ACTION_SKIP,
        _ => ACTION_KEEP_BOTH,
    }
}

/// The engine's resolution for the sheet's default action.
pub fn mode_resolution(mode: i32) -> Resolution {
    match mode {
        MODE_REPLACE_IF_DIFFERENT => Resolution::ReplaceIfDifferent,
        MODE_REPLACE_ALL => Resolution::Replace,
        MODE_SKIP_ALL => Resolution::Skip,
        _ => Resolution::KeepBoth,
    }
}

/// The conflict policy that "Remember for … this site" stores for the default action `mode`.
pub fn mode_policy(mode: i32) -> ConflictPolicy {
    match mode {
        MODE_REPLACE_IF_DIFFERENT => ConflictPolicy::ReplaceIfDifferent,
        MODE_REPLACE_ALL => ConflictPolicy::Replace,
        MODE_SKIP_ALL => ConflictPolicy::Skip,
        _ => ConflictPolicy::KeepBoth,
    }
}

/// The engine's resolution for one row's action.
pub fn action_resolution(action: i32) -> Resolution {
    match action {
        ACTION_REPLACE => Resolution::Replace,
        ACTION_SKIP => Resolution::Skip,
        _ => Resolution::KeepBoth,
    }
}

/// How sure a comparison was: a digest is better than a size.
fn strength(by: CompareBy) -> u8 {
    match by {
        CompareBy::Size => 0,
        CompareBy::Md5 => 1,
        CompareBy::Sha256 => 2,
    }
}

/// The conflict sheet for `conflicts`, with dates written as of now in the PC's time zone.
///
/// `dir` says which way the files go (an upload names `site`, a download names the folder),
/// `total_files` is the size of the whole batch, `target` the folder the files go to as the sheet
/// says it ("wp-content / uploads / 2026 / 10"), and `short` the site's short name for the column
/// heading ("Bunny").
pub fn conflict_sheet(
    dir: Direction,
    conflicts: &[Conflict],
    total_files: u64,
    target: &str,
    site: &str,
    short: &str,
) -> ConflictSheetView {
    conflict_sheet_at(
        dir,
        conflicts,
        total_files,
        target,
        site,
        short,
        Local::now().fixed_offset(),
    )
}

/// [`conflict_sheet`] as of `now`, whose offset is the time zone the dates are written in.
pub fn conflict_sheet_at(
    dir: Direction,
    conflicts: &[Conflict],
    total_files: u64,
    target: &str,
    site: &str,
    short: &str,
    now: DateTime<FixedOffset>,
) -> ConflictSheetView {
    let download = dir == Direction::Download;
    let (zone, today) = (*now.offset(), now.naive_local());
    let side = |size: u64, modified: Option<DateTime<Utc>>| {
        let date = modified.map(|t| format_date(t.with_timezone(&zone).naive_local(), today));
        copy::size_and_date(&format_size(size), date.as_deref())
    };

    let count = conflicts.len() as u64;
    let identical = conflicts.iter().filter(|c| c.same == Some(true)).count() as u64;
    let differ = conflicts.iter().filter(|c| c.same == Some(false)).count() as u64;
    // An identical file is only as certain as the weakest comparison that said so.
    let identical_by = conflicts
        .iter()
        .filter(|c| c.same == Some(true))
        .map(|c| c.compared_by)
        .min_by_key(|by| strength(*by));

    let rows = conflicts
        .iter()
        .map(|c| {
            let (local, remote) = (
                side(c.local_size, c.local_modified),
                side(c.remote_size, c.remote_modified),
            );
            let (from, to) = if download {
                (remote, local)
            } else {
                (local, remote)
            };
            ConflictRowData {
                name: c.name.clone(),
                source: from,
                target: to,
                contents: contents_text(c.same).to_owned(),
                action_index: default_action(MODE_REPLACE_IF_DIFFERENT, c.same),
            }
        })
        .collect();

    let (col_source, col_target) = if download {
        (copy::on_site(short), copy::ON_THIS_PC.to_owned())
    } else {
        (copy::ON_THIS_PC.to_owned(), copy::on_site(short))
    };
    ConflictSheetView {
        title: copy::conflict_title(dir, count, site, target),
        body: copy::conflict_body(dir, total_files, target),
        for_all_label: copy::conflict_for_all(count),
        summary: copy::conflict_summary(
            differ,
            identical,
            count - identical - differ,
            identical_by,
        ),
        col_source,
        col_target,
        rows,
        remember_label: copy::conflict_remember(dir),
        cancel_label: copy::conflict_cancel(dir),
    }
}

/// The Contents column's word for a comparison.
fn contents_text(same: Option<bool>) -> &'static str {
    match same {
        Some(true) => copy::CONTENTS_IDENTICAL,
        Some(false) => copy::CONTENTS_DIFFERENT,
        None => copy::CONTENTS_UNKNOWN,
    }
}

/// What the sheet's chip colours by: the other way round from [`contents_text`].
fn contents_kind(text: &str) -> ConflictContents {
    if text == copy::CONTENTS_IDENTICAL {
        ConflictContents::Identical
    } else if text == copy::CONTENTS_DIFFERENT {
        ConflictContents::Different
    } else {
        ConflictContents::Unknown
    }
}

// ---- The alerts ----

/// The delete alert: title, text and the label of the delete button.
pub fn delete_alert(
    summary: &DeleteSummary,
    site: &str,
    provider: ProviderKind,
) -> (String, String, String) {
    (
        copy::delete_title(summary, site),
        copy::delete_body(summary, provider),
        copy::delete_button(summary),
    )
}

/// The Remove Site alert: title and text. `provider_label` is the service's name ("Bunny
/// Storage").
pub fn remove_site_alert(site: &str, provider_label: &str) -> (String, String) {
    (
        copy::remove_site_title(site),
        copy::remove_site_body(provider_label),
    )
}

/// The alert for closing the window with `unfinished` files not done: title and text.
pub fn quit_alert(unfinished: u64) -> (String, String) {
    (copy::QUIT_TITLE.to_owned(), copy::quit_body(unfinished))
}

// ---- The banners ----

/// "5 transfers didn't finish last time." (Resume, Discard).
pub fn resume_banner(n: usize, deletes: usize) -> String {
    copy::resume_banner(n, deletes)
}

/// "Your PC's clock is off. … Transfers to {site} are paused until you click Resume." (Resume).
pub fn clock_banner(site: &str) -> String {
    copy::clock_banner(site)
}

/// "You're offline. Transfers continue when the connection is back."
pub fn offline_banner() -> String {
    copy::offline_banner()
}

/// "Couldn't sign in to {site}. Its transfers are paused." (Edit Site…).
pub fn auth_banner(site: &str) -> String {
    copy::auth_banner(site)
}

/// "Drive {drive} is full. Free up {size} and click Resume." (Resume).
pub fn disk_full_banner(drive: &str, needed: u64) -> String {
    copy::disk_full_banner(drive, needed)
}

// ---- The New Folder sheet ----

/// What is wrong with `name` as the name of a new folder next to the items named in `existing`,
/// or `None` if it will do. Spaces around the name do not count.
pub fn new_folder_error(name: &str, existing: &HashSet<String>) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        Some(copy::NAME_MISSING.to_owned())
    } else if name.contains(['/', '\\']) {
        Some(copy::NAME_HAS_SEPARATOR.to_owned())
    } else if name == "." || name == ".." {
        Some(copy::name_not_allowed(name))
    } else if existing.contains(name) {
        Some(copy::name_exists(name))
    } else {
        None
    }
}

// ---- The window ----

fn slint_rows(rows: &[ConflictRowData]) -> Vec<ConflictRow> {
    rows.iter()
        .map(|row| ConflictRow {
            name: row.name.as_str().into(),
            kind: slint_kind(kind_for(&row.name, false)),
            source: row.source.as_str().into(),
            target: row.target.as_str().into(),
            contents: row.contents.as_str().into(),
            same: contents_kind(&row.contents),
            action: row.action_index,
        })
        .collect()
}

/// Puts `view` in the window; the sheet opens when `conflicts-open` is set.
pub fn apply_conflicts(ui: &AppWindow, view: &ConflictSheetView) {
    ui.set_conflicts_title(view.title.as_str().into());
    ui.set_conflicts_body(view.body.as_str().into());
    ui.set_conflicts_for_all(view.for_all_label.as_str().into());
    ui.set_conflicts_summary(view.summary.as_str().into());
    ui.set_conflicts_col_source(view.col_source.as_str().into());
    ui.set_conflicts_col_target(view.col_target.as_str().into());
    ui.set_conflicts_remember(view.remember_label.as_str().into());
    ui.set_conflicts_cancel_label(view.cancel_label.as_str().into());
    ui.set_conflicts_rows(ModelRc::new(VecModel::from(slint_rows(&view.rows))));
}

/// Sets every row of the conflict sheet to what the default action `mode` does to it.
fn apply_mode(ui: &AppWindow, mode: i32) {
    let rows = ui.get_conflicts_rows();
    let Some(model) = rows.as_any().downcast_ref::<VecModel<ConflictRow>>() else {
        return;
    };
    for i in 0..model.row_count() {
        let Some(mut row) = model.row_data(i) else {
            continue;
        };
        let same = match row.same {
            ConflictContents::Identical => Some(true),
            ConflictContents::Different => Some(false),
            ConflictContents::Unknown => None,
        };
        row.action = default_action(mode, same);
        model.set_row_data(i, row);
    }
}

/// The names in the cloud pane's folder.
fn remote_names(ui: &AppWindow) -> HashSet<String> {
    ui.get_remote_rows()
        .iter()
        .map(|row| row.name.to_string())
        .collect()
}

/// Opens the delete alert for `alert` (see [`delete_alert`]).
pub fn show_delete_alert(ui: &AppWindow, (title, body, button): &(String, String, String)) {
    ui.set_delete_title(title.as_str().into());
    ui.set_delete_body(body.as_str().into());
    ui.set_delete_label(button.as_str().into());
    ui.set_delete_open(true);
}

/// Opens the Remove Site alert for `alert` (see [`remove_site_alert`]).
pub fn show_remove_site_alert(ui: &AppWindow, (title, body): &(String, String)) {
    ui.set_remove_site_title(title.as_str().into());
    ui.set_remove_site_body(body.as_str().into());
    ui.set_remove_site_open(true);
}

/// Opens the quit alert for `alert` (see [`quit_alert`]).
pub fn show_quit_alert(ui: &AppWindow, (title, body): &(String, String)) {
    ui.set_quit_title(title.as_str().into());
    ui.set_quit_body(body.as_str().into());
    ui.set_quit_open(true);
}

/// Wires the callbacks of the sheets, alerts and banners. `transfers.rs` and `binding.rs` replace
/// each stub below with a call into the engine or the session; without them nothing opens them.
pub fn install(ui: &AppWindow) {
    ui.set_new_folder_name(copy::NEW_FOLDER_NAME.into());

    ui.on_conflicts_mode_changed({
        let weak = ui.as_weak();
        move |mode| {
            if let Some(ui) = weak.upgrade() {
                apply_mode(&ui, mode);
            }
        }
    });
    ui.on_conflicts_continue(|_mode, _remember| {});
    ui.on_conflicts_cancel(|| {});

    ui.on_delete_confirm(|| {});
    ui.on_delete_cancel(|| {});

    ui.on_new_folder_create({
        let weak = ui.as_weak();
        move |name| {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(error) = new_folder_error(&name, &remote_names(&ui)) {
                ui.set_new_folder_error(error.into());
            }
            // A name that will do is made into a folder, and the sheet closed, by the engine
            // (Task 23).
        }
    });
    ui.on_new_folder_cancel(|| {});

    ui.on_quit_pause(|| {});
    ui.on_quit_cancel_all(|| {});
    ui.on_quit_keep_running(|| {});

    ui.on_resume_banner_resume(|| {});
    ui.on_resume_banner_discard(|| {});
    ui.on_disk_banner_resume(|| {});
}

#[cfg(test)]
#[path = "sheets_view_tests.rs"]
mod tests;
