//! Tests of the sheets, alerts and banners: every pinned text (the conflict sheet against the
//! mockup, the delete alert for Bunny and S3, New Folder's checks, Remove Site, quit and the
//! banners), the sheet's numbers, and the sheets in a headless window (default buttons, the
//! keyboard trap, the conflict rows' actions).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use chrono::{FixedOffset, TimeZone};
use httporter_core::{CompareBy, Conflict, ConflictPolicy, RemotePath};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use super::*;
use crate::keyboard_tests::{site, tap};
use crate::viewmodel::{FileKindData, RowData, to_slint_rows};
use crate::{AppWindow, ProviderTile};

const KB: u64 = 1024;
const MB: u64 = 1024 * 1024;

/// UTC+7, where the mockup's "Today 09:58" was written.
fn zone() -> FixedOffset {
    FixedOffset::east_opt(7 * 3600).unwrap()
}

/// 29 October 2026, noon at UTC+7.
fn now() -> DateTime<FixedOffset> {
    zone().with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap()
}

fn at(day: u32, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    Some(
        zone()
            .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
            .with_timezone(&Utc),
    )
}

fn conflict(index: usize, name: &str, same: Option<bool>, by: CompareBy) -> Conflict {
    Conflict {
        index,
        name: name.into(),
        local: PathBuf::from(format!(r"C:\site\{name}")),
        remote: RemotePath::parse(&format!("wp-content/uploads/2026/10/{name}")).unwrap(),
        // 357.2 KB on both sides.
        local_size: 365_773,
        local_modified: at(29, 9, 58),
        remote_size: 365_773,
        remote_modified: at(28, 17, 2),
        same,
        compared_by: by,
    }
}

/// The mockup's twelve: five identical by SHA-256 first, then seven that differ.
fn twelve() -> Vec<Conflict> {
    (0..12)
        .map(|i| {
            let same = i < 5;
            conflict(i, &format!("img-{i}.jpg"), Some(same), CompareBy::Sha256)
        })
        .collect()
}

fn upload_sheet(conflicts: &[Conflict]) -> ConflictSheetView {
    conflict_sheet_at(
        Direction::Upload,
        conflicts,
        640,
        "wp-content / uploads / 2026 / 10",
        "example-shop.com",
        "Bunny",
        now(),
    )
}

// ---- The conflict sheet ----

#[test]
fn conflict_sheet_matches_mockup() {
    let view = upload_sheet(&twelve());
    assert_eq!(view.title, "12 files already exist on example-shop.com");
    assert_eq!(
        view.body,
        "You're uploading 640 files to wp-content / uploads / 2026 / 10. Choose once what \
         happens to existing files; you can change single files below."
    );
    assert_eq!(view.for_all_label, "For all 12:");
    assert_eq!(view.summary, "7 differ · 5 identical (same SHA-256)");
    assert_eq!(
        (view.col_source.as_str(), view.col_target.as_str()),
        ("On this PC", "On Bunny")
    );
    assert_eq!(view.remember_label, "Remember for uploads to this site");
    assert_eq!(view.cancel_label, "Cancel Upload");
    assert_eq!(view.rows.len(), 12);

    // An identical row: left alone under "Replace if different".
    let identical = &view.rows[0];
    assert_eq!(identical.name, "img-0.jpg");
    assert_eq!(identical.source, "357.2 KB · Today 09:58");
    assert_eq!(identical.target, "357.2 KB · Yesterday 17:02");
    assert_eq!(identical.contents, "Identical");
    assert_eq!(identical.action_index, ACTION_SKIP);
    // One that differs is replaced.
    let differs = &view.rows[5];
    assert_eq!(
        (differs.contents.as_str(), differs.action_index),
        ("Different", ACTION_REPLACE)
    );

    // A row that could not be compared is replaced too, and counted apart.
    let mut some = twelve();
    some[11].same = None;
    some[10].same = None;
    let view = upload_sheet(&some);
    assert_eq!(
        (view.rows[11].contents.as_str(), view.rows[11].action_index),
        ("Unknown", ACTION_REPLACE)
    );
    assert_eq!(
        view.summary,
        "5 differ · 5 identical (same SHA-256) · 2 unknown"
    );

    // The summary names how the identical files were told apart, by the weakest comparison.
    let by = |kinds: &[CompareBy]| {
        let conflicts: Vec<_> = kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| conflict(i, &format!("f{i}"), Some(true), *kind))
            .collect();
        upload_sheet(&conflicts).summary
    };
    assert_eq!(by(&[CompareBy::Md5]), "0 differ · 1 identical (same MD5)");
    assert_eq!(by(&[CompareBy::Size]), "0 differ · 1 identical (same size)");
    assert_eq!(
        by(&[CompareBy::Sha256, CompareBy::Md5, CompareBy::Sha256]),
        "0 differ · 3 identical (same MD5)"
    );
    assert_eq!(
        by(&[CompareBy::Md5, CompareBy::Size]),
        "0 differ · 2 identical (same size)"
    );
    // None identical: nothing to say about how.
    let differ: Vec<_> = (0..7)
        .map(|i| conflict(i, &format!("f{i}"), Some(false), CompareBy::Md5))
        .collect();
    assert_eq!(upload_sheet(&differ).summary, "7 differ · 0 identical");

    // One file, and a file without a date.
    let mut one = vec![conflict(0, "index.html", Some(false), CompareBy::Sha256)];
    one[0].remote_modified = None;
    let view = upload_sheet(&one);
    assert_eq!(view.title, "1 file already exists on example-shop.com");
    assert_eq!(view.for_all_label, "For all 1:");
    assert_eq!(view.rows[0].target, "357.2 KB");
    // An older date is written in full.
    one[0].local_modified = Some(Utc.with_ymd_and_hms(2026, 3, 12, 2, 0, 0).unwrap());
    assert_eq!(upload_sheet(&one).rows[0].source, "357.2 KB · 12 Mar 2026");
}

#[test]
fn conflict_sheet_download_variant() {
    let view = conflict_sheet_at(
        Direction::Download,
        &twelve(),
        640,
        "Downloads",
        "example-shop.com",
        "Bunny",
        now(),
    );
    assert_eq!(view.title, "12 files already exist in Downloads");
    assert_eq!(
        view.body,
        "You're downloading 640 files to Downloads. Choose once what happens to existing files; \
         you can change single files below."
    );
    // The files come from the site: its column first.
    assert_eq!(
        (view.col_source.as_str(), view.col_target.as_str()),
        ("On Bunny", "On this PC")
    );
    assert_eq!(view.rows[0].source, "357.2 KB · Yesterday 17:02");
    assert_eq!(view.rows[0].target, "357.2 KB · Today 09:58");
    assert_eq!(view.remember_label, "Remember for downloads from this site");
    assert_eq!(view.cancel_label, "Cancel Download");
    assert_eq!(view.summary, "7 differ · 5 identical (same SHA-256)");

    let one = conflict_sheet_at(
        Direction::Download,
        &twelve()[..1],
        1,
        "Downloads",
        "example-shop.com",
        "R2",
        now(),
    );
    assert_eq!(one.title, "1 file already exists in Downloads");
    assert_eq!(one.col_source, "On R2");
    assert!(
        one.body
            .starts_with("You're downloading 1 file to Downloads.")
    );
}

#[test]
fn the_sheets_numbers_map_to_the_engines_choices() {
    // Every row under each default action.
    for (same, wanted) in [
        (
            Some(true),
            [ACTION_SKIP, ACTION_REPLACE, ACTION_SKIP, ACTION_KEEP_BOTH],
        ),
        (
            Some(false),
            [
                ACTION_REPLACE,
                ACTION_REPLACE,
                ACTION_SKIP,
                ACTION_KEEP_BOTH,
            ],
        ),
        (
            None,
            [
                ACTION_REPLACE,
                ACTION_REPLACE,
                ACTION_SKIP,
                ACTION_KEEP_BOTH,
            ],
        ),
    ] {
        let got = [
            MODE_REPLACE_IF_DIFFERENT,
            MODE_REPLACE_ALL,
            MODE_SKIP_ALL,
            MODE_KEEP_BOTH,
        ]
        .map(|mode| default_action(mode, same));
        assert_eq!(got, wanted, "{same:?}");
    }
    assert_eq!(
        mode_resolution(MODE_REPLACE_IF_DIFFERENT),
        Resolution::ReplaceIfDifferent
    );
    assert_eq!(mode_resolution(MODE_REPLACE_ALL), Resolution::Replace);
    assert_eq!(mode_resolution(MODE_SKIP_ALL), Resolution::Skip);
    assert_eq!(mode_resolution(MODE_KEEP_BOTH), Resolution::KeepBoth);
    assert_eq!(mode_resolution(99), Resolution::KeepBoth);
    assert_eq!(
        mode_policy(MODE_REPLACE_IF_DIFFERENT),
        ConflictPolicy::ReplaceIfDifferent
    );
    assert_eq!(mode_policy(MODE_REPLACE_ALL), ConflictPolicy::Replace);
    assert_eq!(mode_policy(MODE_SKIP_ALL), ConflictPolicy::Skip);
    assert_eq!(mode_policy(MODE_KEEP_BOTH), ConflictPolicy::KeepBoth);
    assert_eq!(action_resolution(ACTION_REPLACE), Resolution::Replace);
    assert_eq!(action_resolution(ACTION_SKIP), Resolution::Skip);
    assert_eq!(action_resolution(ACTION_KEEP_BOTH), Resolution::KeepBoth);
}

// ---- The alerts ----

fn summary(
    folders: u64,
    files: u64,
    total_files: u64,
    total_bytes: u64,
    single: Option<(&str, bool)>,
) -> DeleteSummary {
    DeleteSummary {
        folders,
        files,
        total_files,
        total_bytes,
        single: single.map(|(name, folder)| (name.to_owned(), folder)),
    }
}

#[test]
fn delete_alert_variants_bunny_and_s3() {
    let alert = |s: &DeleteSummary, provider| {
        let (title, body, button) = delete_alert(s, "example-shop.com", provider);
        (title, body, button)
    };
    let one_folder = summary(1, 0, 48, 12_897_485, Some(("old-promo-2025", true)));
    let one_file = summary(0, 1, 1, 1_234, Some(("index.html", false)));
    let several = summary(3, 12, 1_284, 54 * MB, None);

    assert_eq!(
        alert(&one_folder, ProviderKind::Bunny),
        (
            "Delete “old-promo-2025” from example-shop.com?".to_owned(),
            "This folder holds 48 files (12.3 MB). Bunny Storage has no trash, so this can't \
             be undone."
                .to_owned(),
            "Delete 48 Files".to_owned()
        )
    );
    assert_eq!(
        alert(&one_file, ProviderKind::Bunny),
        (
            "Delete “index.html” from example-shop.com?".to_owned(),
            "Bunny Storage has no trash, so this can't be undone.".to_owned(),
            "Delete".to_owned()
        )
    );
    assert_eq!(
        alert(&several, ProviderKind::Bunny),
        (
            "Delete 3 folders and 12 files from example-shop.com?".to_owned(),
            "That's 1,284 files (54 MB) in all. Bunny Storage has no trash, so this can't be \
             undone."
                .to_owned(),
            "Delete 1,284 Files".to_owned()
        )
    );

    // S3 keeps nothing back either, but has no trash to speak of.
    assert_eq!(
        alert(&one_folder, ProviderKind::S3).1,
        "This folder holds 48 files (12.3 MB). This can't be undone."
    );
    assert_eq!(
        alert(&one_file, ProviderKind::S3).1,
        "This can't be undone."
    );
    assert_eq!(
        alert(&several, ProviderKind::S3).1,
        "That's 1,284 files (54 MB) in all. This can't be undone."
    );
    assert_eq!(alert(&several, ProviderKind::S3).2, "Delete 1,284 Files");

    // Folders only, files only, one file in a folder, and nothing to count.
    let folders_only = summary(2, 0, 10, 2 * MB, None);
    assert_eq!(
        alert(&folders_only, ProviderKind::Bunny).0,
        "Delete 2 folders from example-shop.com?"
    );
    let files_only = summary(0, 12, 12, 512 * KB, None);
    let (title, body, button) = alert(&files_only, ProviderKind::Bunny);
    assert_eq!(title, "Delete 12 files from example-shop.com?");
    assert_eq!(
        body,
        "That's 12 files (512 KB) in all. Bunny Storage has no trash, so this can't be undone."
    );
    assert_eq!(button, "Delete 12 Files");
    let one_inside = summary(1, 0, 1, 900, Some(("empty-ish", true)));
    let (_, body, button) = alert(&one_inside, ProviderKind::Bunny);
    assert_eq!(
        body,
        "This folder holds 1 file (900 B). Bunny Storage has no trash, so this can't be undone."
    );
    assert_eq!(button, "Delete 1 File");
    let empty = summary(1, 0, 0, 0, Some(("empty", true)));
    let (title, body, button) = alert(&empty, ProviderKind::Bunny);
    assert_eq!(title, "Delete “empty” from example-shop.com?");
    assert_eq!(body, "Bunny Storage has no trash, so this can't be undone.");
    assert_eq!(button, "Delete");
    let one_folder_one_file = summary(1, 1, 3, 3 * KB, None);
    assert_eq!(
        alert(&one_folder_one_file, ProviderKind::Bunny).0,
        "Delete 1 folder and 1 file from example-shop.com?"
    );
}

#[test]
fn new_folder_errors() {
    let existing: HashSet<String> = ["assets", "index.html"].map(String::from).into();
    let error = |name: &str| new_folder_error(name, &existing);
    assert_eq!(error("").as_deref(), Some("Enter a name."));
    assert_eq!(error("   ").as_deref(), Some("Enter a name."));
    assert_eq!(
        error("a/b").as_deref(),
        Some("Names can't contain / or \\.")
    );
    assert_eq!(
        error("a\\b").as_deref(),
        Some("Names can't contain / or \\.")
    );
    assert_eq!(error(".").as_deref(), Some("“.” can't be used as a name."));
    assert_eq!(
        error(" .. ").as_deref(),
        Some("“..” can't be used as a name.")
    );
    assert_eq!(
        error("assets").as_deref(),
        Some("“assets” already exists here.")
    );
    // Spaces around the name do not count.
    assert_eq!(
        error("  assets ").as_deref(),
        Some("“assets” already exists here.")
    );
    assert_eq!(
        error("index.html").as_deref(),
        Some("“index.html” already exists here.")
    );
    assert_eq!(error("untitled folder"), None);
    assert_eq!(error(" new ").as_deref(), None);
    assert_eq!(copy::NEW_FOLDER_NAME, "untitled folder");
}

#[test]
fn remove_quit_and_banner_texts() {
    assert_eq!(
        remove_site_alert("example-shop.com", "Bunny Storage"),
        (
            "Remove “example-shop.com”?".to_owned(),
            "HTTPorter forgets this site and its saved password. Files on Bunny Storage aren't \
             touched."
                .to_owned()
        )
    );
    assert_eq!(
        remove_site_alert("media", "Cloudflare R2").1,
        "HTTPorter forgets this site and its saved password. Files on Cloudflare R2 aren't touched."
    );
    assert_eq!(
        quit_alert(5),
        (
            "Transfers are still running".to_owned(),
            "5 files haven't finished.".to_owned()
        )
    );
    assert_eq!(quit_alert(1).1, "1 file hasn't finished.");
    assert_eq!(quit_alert(13_793).1, "13,793 files haven't finished.");

    assert_eq!(resume_banner(1), "1 transfer didn't finish last time.");
    assert_eq!(resume_banner(5), "5 transfers didn't finish last time.");
    assert_eq!(
        resume_banner(1_204),
        "1,204 transfers didn't finish last time."
    );
    assert_eq!(
        offline_banner(),
        "You're offline. Transfers continue when the connection is back."
    );
    assert_eq!(
        auth_banner("example-shop.com"),
        "Couldn't sign in to example-shop.com. Its transfers are paused."
    );
    assert_eq!(
        disk_full_banner("D:", 3 * MB),
        "Drive D: is full. Free up 3.0 MB and click Resume."
    );
    assert_eq!(
        disk_full_banner("E:", 1_536),
        "Drive E: is full. Free up 1.5 KB and click Resume."
    );
}

// ---- In the window ----

type Log = Rc<RefCell<Vec<String>>>;

/// A window with one site (the banners sit above the panes, which only show with a site), the
/// sheets' callbacks as `install` wires them, and every answer logged.
fn window() -> (AppWindow, Log) {
    // The platform can be set once per thread; a test may need several windows.
    thread_local! {
        static PLATFORM_SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !PLATFORM_SET.replace(true) {
        testing::init_no_event_loop();
    }
    let w = AppWindow::new().unwrap();
    w.set_sites(ModelRc::new(VecModel::from(vec![site(
        "example-shop.com",
        ProviderTile::Bunny,
        "B",
    )])));
    install(&w);
    let log: Log = Rc::default();
    let note = |text: &'static str| {
        let log = log.clone();
        move || log.borrow_mut().push(text.to_owned())
    };
    w.on_conflicts_continue({
        let log = log.clone();
        move |mode, remember| log.borrow_mut().push(format!("continue {mode} {remember}"))
    });
    w.on_conflicts_cancel(note("conflicts-cancel"));
    w.on_delete_confirm(note("delete-confirm"));
    w.on_delete_cancel(note("delete-cancel"));
    w.on_new_folder_cancel(note("new-folder-cancel"));
    w.on_remove_site_confirm(note("remove-confirm"));
    w.on_remove_site_cancel(note("remove-cancel"));
    w.on_quit_pause(note("quit-pause"));
    w.on_quit_cancel_all(note("quit-cancel-all"));
    w.on_quit_keep_running(note("keep-running"));
    w.on_resume_banner_resume(note("resume"));
    w.on_resume_banner_discard(note("discard"));
    w.on_auth_banner_edit_site(note("edit-site"));
    w.on_disk_banner_resume(note("disk-resume"));
    w.show().unwrap();
    (w, log)
}

fn told(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.borrow_mut())
}

fn key(w: &AppWindow, key: impl Into<slint::SharedString>) {
    tap(w.window(), key);
    slint::platform::update_timers_and_animations();
}

fn backtab(w: &AppWindow) {
    key(w, Key::Backtab);
}

fn find(w: &AppWindow, role: AccessibleRole, label: &str) -> Option<ElementHandle> {
    ElementHandle::find_by_accessible_label(w, label).find(|e| e.accessible_role() == Some(role))
}

fn press(w: &AppWindow, role: AccessibleRole, label: &str) {
    find(w, role, label)
        .unwrap_or_else(|| panic!("no {role:?} {label:?}"))
        .invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
}

fn open_delete(w: &AppWindow) {
    let alert = delete_alert(
        &summary(1, 0, 48, 12_897_485, Some(("old-promo-2025", true))),
        "example-shop.com",
        ProviderKind::Bunny,
    );
    show_delete_alert(w, &alert);
    slint::platform::update_timers_and_animations();
}

#[test]
fn delete_alert_default_is_cancel() {
    let (w, log) = window();
    open_delete(&w);
    let card = find(
        &w,
        AccessibleRole::Form,
        "Delete “old-promo-2025” from example-shop.com?",
    )
    .expect("the alert is open");
    assert_eq!(
        card.accessible_description().as_deref(),
        Some(
            "This folder holds 48 files (12.3 MB). Bunny Storage has no trash, so this can't be \
             undone."
        )
    );
    assert!(find(&w, AccessibleRole::Button, "Delete 48 Files").is_some());
    // The CDN purge waits for Publish: off, and it says so.
    let purge = find(
        &w,
        AccessibleRole::Checkbox,
        "Also purge these URLs from the CDN",
    )
    .unwrap();
    assert_eq!(purge.accessible_enabled(), Some(false));
    assert_eq!(purge.accessible_checked(), Some(false));
    assert_eq!(
        ElementHandle::find_by_accessible_label(&w, "CDN purge arrives with Publish.").count(),
        1
    );

    // Return presses Cancel, the default.
    key(&w, Key::Return);
    assert_eq!(told(&log), ["delete-cancel"]);
    assert!(!w.get_delete_open());
    // So does Escape.
    open_delete(&w);
    key(&w, Key::Escape);
    assert_eq!(told(&log), ["delete-cancel"]);
    assert!(!w.get_delete_open());
    // Cancel has the focus: Space presses it.
    open_delete(&w);
    key(&w, Key::Space);
    assert_eq!(told(&log), ["delete-cancel"]);

    // Tab wraps from Cancel to Delete, and Delete deletes.
    open_delete(&w);
    key(&w, Key::Tab);
    key(&w, Key::Space);
    assert_eq!(told(&log), ["delete-confirm"]);
    assert!(!w.get_delete_open());
    // Tab from Delete goes back to Cancel; Shift+Tab from Delete wraps to Cancel too.
    open_delete(&w);
    key(&w, Key::Tab);
    key(&w, Key::Tab);
    key(&w, Key::Space);
    assert_eq!(told(&log), ["delete-cancel"]);
    open_delete(&w);
    backtab(&w);
    backtab(&w);
    key(&w, Key::Space);
    assert_eq!(told(&log), ["delete-cancel"]);
}

/// Six rows: identical, different, unknown, twice over.
fn conflict_window() -> (AppWindow, Log) {
    let (w, log) = window();
    let conflicts: Vec<_> = (0..6)
        .map(|i| {
            let same = [Some(true), Some(false), None][i % 3];
            conflict(i, &format!("f{i}.txt"), same, CompareBy::Sha256)
        })
        .collect();
    apply_conflicts(&w, &upload_sheet(&conflicts));
    w.set_conflicts_open(true);
    slint::platform::update_timers_and_animations();
    (w, log)
}

fn actions(w: &AppWindow) -> Vec<i32> {
    w.get_conflicts_rows()
        .iter()
        .map(|row| row.action)
        .collect()
}

fn picker_value(w: &AppWindow, name: &str) -> String {
    find(w, AccessibleRole::Combobox, &format!("Action for {name}"))
        .unwrap_or_else(|| panic!("no picker for {name}"))
        .accessible_value()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn conflict_rows_change_action() {
    let (w, log) = conflict_window();
    assert!(
        find(
            &w,
            AccessibleRole::Form,
            "6 files already exist on example-shop.com"
        )
        .is_some()
    );
    assert_eq!(actions(&w), [1, 0, 0, 1, 0, 0]);
    // An identical row under "Replace if different" is skipped, and says why.
    assert_eq!(picker_value(&w, "f0.txt"), "Skip (identical)");
    assert_eq!(picker_value(&w, "f1.txt"), "Replace");

    // A row's own menu: Tab from the default action to the first row's, Down to Keep both.
    key(&w, Key::Tab);
    key(&w, Key::DownArrow);
    assert_eq!(actions(&w), [2, 0, 0, 1, 0, 0]);
    assert_eq!(picker_value(&w, "f0.txt"), "Keep both");
    assert_eq!(told(&log), Vec::<String>::new(), "nothing else is told");

    // "Skip all" rewrites every row; the identical ones then read plain "Skip".
    press(&w, AccessibleRole::RadioButton, "Skip all");
    assert_eq!(actions(&w), [1; 6]);
    assert_eq!(picker_value(&w, "f0.txt"), "Skip");
    press(&w, AccessibleRole::RadioButton, "Keep both");
    assert_eq!(actions(&w), [2; 6]);
    press(&w, AccessibleRole::RadioButton, "Replace all");
    assert_eq!(actions(&w), [0; 6]);
    press(&w, AccessibleRole::RadioButton, "Replace if different");
    assert_eq!(actions(&w), [1, 0, 0, 1, 0, 0]);

    // Continue gives the default action and the remember box (ticked at first).
    press(&w, AccessibleRole::RadioButton, "Skip all");
    press(
        &w,
        AccessibleRole::Checkbox,
        "Remember for uploads to this site",
    );
    press(&w, AccessibleRole::Button, "Continue");
    assert_eq!(told(&log), ["continue 2 false"]);
    assert!(!w.get_conflicts_open());
    // The rows' own choices stay for the app to read.
    assert_eq!(actions(&w), [1; 6]);
}

#[test]
fn sheets_trap_focus_and_escape_cancels() {
    // The conflict sheet: Escape cancels, Return continues, the focus wraps both ways.
    let (w, log) = conflict_window();
    key(&w, Key::Escape);
    assert_eq!(told(&log), ["conflicts-cancel"]);
    assert!(!w.get_conflicts_open());

    let (w, log) = conflict_window();
    key(&w, Key::Return);
    assert_eq!(told(&log), ["continue 0 true"]);

    let (w, log) = conflict_window();
    // Back from the default action wraps to Continue.
    backtab(&w);
    key(&w, Key::Space);
    assert_eq!(told(&log), ["continue 0 true"]);
    let (w, log) = conflict_window();
    // Forward from Continue wraps to the default action, where Right picks the next one.
    backtab(&w);
    key(&w, Key::Tab);
    key(&w, Key::RightArrow);
    assert_eq!(actions(&w), [0; 6], "Replace all");
    key(&w, Key::Escape);
    assert_eq!(told(&log), ["conflicts-cancel"]);

    // Remove Site: Cancel is the default; the focus wraps between its two buttons.
    let (w, log) = window();
    let open_remove = |w: &AppWindow| {
        show_remove_site_alert(w, &remove_site_alert("example-shop.com", "Bunny Storage"));
        slint::platform::update_timers_and_animations();
    };
    open_remove(&w);
    assert!(find(&w, AccessibleRole::Form, "Remove “example-shop.com”?").is_some());
    key(&w, Key::Escape);
    open_remove(&w);
    key(&w, Key::Return);
    open_remove(&w);
    key(&w, Key::Tab);
    key(&w, Key::Space);
    open_remove(&w);
    backtab(&w);
    backtab(&w);
    key(&w, Key::Space);
    assert_eq!(
        told(&log),
        [
            "remove-cancel",
            "remove-cancel",
            "remove-confirm",
            "remove-cancel"
        ]
    );

    // Quit: Keep Running is the default; Escape and Return keep running.
    let (w, log) = window();
    let open_quit = |w: &AppWindow| {
        show_quit_alert(w, &quit_alert(5));
        slint::platform::update_timers_and_animations();
    };
    open_quit(&w);
    let card = find(&w, AccessibleRole::Form, "Transfers are still running").unwrap();
    assert_eq!(
        card.accessible_description().as_deref(),
        Some("5 files haven't finished.")
    );
    key(&w, Key::Escape);
    open_quit(&w);
    key(&w, Key::Return);
    // Tab from Keep Running wraps to Pause and Quit; the next is Cancel and Quit.
    open_quit(&w);
    key(&w, Key::Tab);
    key(&w, Key::Space);
    open_quit(&w);
    key(&w, Key::Tab);
    key(&w, Key::Tab);
    key(&w, Key::Space);
    // Back from Pause and Quit wraps to Keep Running.
    open_quit(&w);
    key(&w, Key::Tab);
    backtab(&w);
    key(&w, Key::Space);
    assert_eq!(
        told(&log),
        [
            "keep-running",
            "keep-running",
            "quit-pause",
            "quit-cancel-all",
            "keep-running"
        ]
    );
    assert!(!w.get_quit_open());
}

#[test]
fn the_new_folder_sheet_checks_the_name_and_keeps_the_focus() {
    let (w, log) = window();
    let rows: Vec<RowData> = ["assets", "index.html"]
        .iter()
        .map(|name| RowData {
            name: (*name).into(),
            kind: FileKindData::Folder,
            size: String::new(),
            modified: String::new(),
            folder: true,
            bytes: 0,
        })
        .collect();
    w.set_remote_rows(to_slint_rows(&rows));
    let created: Log = Rc::default();
    let open = |w: &AppWindow| {
        w.set_new_folder_open(true);
        slint::platform::update_timers_and_animations();
    };
    assert_eq!(w.get_new_folder_name(), "untitled folder");

    // The name starts selected: typing replaces it. A name that is taken is refused by `install`,
    // and the sheet stays open with the message.
    open(&w);
    assert!(find(&w, AccessibleRole::Form, "New Folder").is_some());
    let field = || find(&w, AccessibleRole::TextInput, "Name").unwrap();
    assert_eq!(
        field().accessible_value().as_deref(),
        Some("untitled folder")
    );
    for ch in "assets".chars() {
        key(&w, ch.to_string());
    }
    key(&w, Key::Return);
    assert_eq!(w.get_new_folder_error(), "“assets” already exists here.");
    assert!(w.get_new_folder_open());
    assert_eq!(
        field().accessible_description().as_deref(),
        Some("“assets” already exists here.")
    );
    // Editing the name clears the message.
    key(&w, Key::Backspace);
    assert_eq!(w.get_new_folder_error(), "");

    // Create with a name that will do reaches the app (which makes the folder).
    w.on_new_folder_create({
        let created = created.clone();
        move |name| created.borrow_mut().push(name.to_string())
    });
    key(&w, Key::Return);
    assert_eq!(created.borrow().as_slice(), ["asset"]);

    // Tab wraps between the field and Create; Escape cancels.
    w.set_new_folder_open(false);
    slint::platform::update_timers_and_animations();
    // Looking for the sheet makes the window drop it, so the next one starts afresh.
    assert!(find(&w, AccessibleRole::Form, "New Folder").is_none());
    open(&w);
    backtab(&w); // the field is the first stop: back to Create
    key(&w, Key::Space);
    assert_eq!(created.borrow().as_slice(), ["asset", "untitled folder"]);
    key(&w, Key::Tab); // from Create, the last stop, to the field
    key(&w, "x");
    key(&w, Key::Escape);
    assert_eq!(told(&log), ["new-folder-cancel"]);
    assert!(!w.get_new_folder_open());
}

#[test]
fn banners_show_with_text_and_their_buttons_answer() {
    let (w, log) = window();
    let shown = |text: &str| ElementHandle::find_by_accessible_label(&w, text).count();
    let resume = resume_banner(5);
    assert_eq!(shown(&resume), 0);
    assert!(find(&w, AccessibleRole::Button, "Discard").is_none());

    w.set_resume_banner_text(resume.as_str().into());
    slint::platform::update_timers_and_animations();
    assert_eq!(shown(&resume), 1);
    press(&w, AccessibleRole::Button, "Resume");
    press(&w, AccessibleRole::Button, "Discard");
    w.set_resume_banner_text("".into());

    w.set_offline_banner_text(offline_banner().into());
    slint::platform::update_timers_and_animations();
    assert_eq!(shown(&offline_banner()), 1);
    w.set_offline_banner_text("".into());

    // Edit Site… is there, and off until the second round of Add Site is wired (Ruling Q10).
    w.set_auth_banner_text(auth_banner("example-shop.com").into());
    slint::platform::update_timers_and_animations();
    let edit = find(&w, AccessibleRole::Button, "Edit Site…").unwrap();
    assert_eq!(edit.accessible_enabled(), Some(false));
    assert_eq!(
        edit.accessible_description().as_deref(),
        Some("Arrives later in this build.")
    );
    w.set_sites_v2_ready(true);
    press(&w, AccessibleRole::Button, "Edit Site…");
    w.set_auth_banner_text("".into());

    w.set_disk_banner_text(disk_full_banner("D:", 3 * MB).into());
    slint::platform::update_timers_and_animations();
    assert_eq!(
        shown("Drive D: is full. Free up 3.0 MB and click Resume."),
        1
    );
    press(&w, AccessibleRole::Button, "Resume");
    w.set_disk_banner_text("".into());
    slint::platform::update_timers_and_animations();
    assert!(find(&w, AccessibleRole::Button, "Resume").is_none());

    assert_eq!(
        told(&log),
        ["resume", "discard", "edit-site", "disk-resume"]
    );
}

#[test]
fn install_leaves_every_sheet_closed_and_every_banner_empty() {
    let (w, _) = window();
    assert!(!w.get_conflicts_open() && !w.get_delete_open() && !w.get_new_folder_open());
    assert!(!w.get_remove_site_open() && !w.get_quit_open());
    for text in [
        w.get_resume_banner_text(),
        w.get_offline_banner_text(),
        w.get_auth_banner_text(),
        w.get_disk_banner_text(),
    ] {
        assert_eq!(text, "");
    }
    // A held Return that opened the delete alert does not press anything.
    open_delete(&w);
    w.window().dispatch_event(WindowEvent::KeyPressRepeated {
        text: Key::Return.into(),
    });
    assert!(w.get_delete_open());
}
