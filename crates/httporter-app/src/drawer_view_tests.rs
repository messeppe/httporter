//! Tests of the transfer drawer's view: the texts and numbers `drawer_view` makes of an engine
//! snapshot (pinned against the mockup), the redraw throttle, and the drawer in a headless
//! window (expanding, switching tabs, the row buttons).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use httporter_core::{
    BatchSummary, Direction, EngineSnapshot, EngineState, JobId, JobKind, JobView,
};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use super::*;
use crate::keyboard_tests::site;
use crate::{AppWindow, ProviderTile};

/// The one place a `JobView` is built in tests (Ruling Q40).
pub(super) fn job_view(id: u64, kind: JobKind, path: &str, size: u64, done_bytes: u64) -> JobView {
    JobView {
        id: JobId(id),
        kind,
        path: path.to_owned(),
        size,
        done_bytes,
        error: None,
        paused: false,
        folder: matches!(kind, JobKind::CreateFolder),
    }
}

const MB: u64 = 1024 * 1024;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 3, 12, 0, 0).unwrap()
}

/// The upload of the mockup (`Main.dc.html`): 8,412 of 13,793 files, 527 of 865 MB, 96 files/s,
/// 6.1 MB/s, about 56 s left; 32 running, 5,349 waiting.
fn mockup() -> EngineSnapshot {
    EngineSnapshot {
        state: EngineState::Running,
        direction: Direction::Upload,
        site: "example-shop.com".into(),
        host: "sg.storage.bunnycdn.com".into(),
        files_total: 13_793,
        files_done: 8_412,
        bytes_total: 865 * MB,
        bytes_done: 527 * MB,
        files_per_s: 96.0,
        bytes_per_s: 6.1 * MB as f64,
        eta: Some(Duration::from_secs(56)),
        active: vec![job_view(
            9,
            JobKind::Upload,
            "wp-content/uploads/a.jpg",
            2 * MB,
            MB,
        )],
        active_count: 32,
        queued_count: 5_349,
        done_count: 8_412,
        ..EngineSnapshot::default()
    }
}

// ---- The texts ----

#[test]
fn idle_and_last_batch_text() {
    let idle = drawer_view(&EngineSnapshot::default(), now());
    assert!(!idle.running && !idle.paused);
    assert_eq!(idle.collapsed_text, "No transfers running");
    assert_eq!(idle.title, "No transfers running");
    assert_eq!(idle.line, "");
    assert_eq!(idle.eta, "");
    assert_eq!(idle.percent, 0.0);
    assert_eq!(
        idle.tab_titles,
        ["Active 0", "Queued 0", "Failed 0", "Done 0"].map(String::from)
    );

    let last = |ago: chrono::Duration, files: u64, bytes: u64| EngineSnapshot {
        last_batch: Some(BatchSummary {
            finished_at: now() - ago,
            files,
            bytes,
            direction: Direction::Upload,
            site: "example-shop.com".into(),
        }),
        ..EngineSnapshot::default()
    };
    let view = drawer_view(&last(chrono::Duration::hours(2), 34, 4_404_019), now());
    assert_eq!(
        view.collapsed_text,
        "No transfers running · Last transfer 2 hours ago · 34 files · 4.2 MB"
    );
    assert_eq!(view.title, "No transfers running");
    assert_eq!(view.line, "Last transfer 2 hours ago · 34 files · 4.2 MB");
    assert!(!view.running);

    // How long ago, in words.
    let line = |ago, files| drawer_view(&last(ago, files, 900), now()).line;
    assert_eq!(
        line(chrono::Duration::seconds(10), 1),
        "Last transfer just now · 1 file · 900 B"
    );
    assert_eq!(
        line(chrono::Duration::minutes(5), 2),
        "Last transfer 5 min ago · 2 files · 900 B"
    );
    assert_eq!(
        line(chrono::Duration::minutes(61), 2),
        "Last transfer 1 hour ago · 2 files · 900 B"
    );
    assert_eq!(
        line(chrono::Duration::hours(30), 2),
        "Last transfer yesterday · 2 files · 900 B"
    );
    assert_eq!(
        line(chrono::Duration::days(3), 1_204),
        "Last transfer 3 days ago · 1,204 files · 900 B"
    );
}

#[test]
fn running_title_line_stats_match_mockup() {
    let view = drawer_view(&mockup(), now());
    assert!(view.running && !view.paused);
    assert_eq!(view.title, "Uploading to example-shop.com");
    assert_eq!(
        view.line,
        "8,412 of 13,793 files · 527 of 865 MB · over HTTPS to sg.storage.bunnycdn.com"
    );
    assert_eq!(
        view.collapsed_text,
        "Uploading 8,412 of 13,793 files · 96 files/s"
    );
    assert_eq!(view.files_per_s, "96");
    assert_eq!(view.mb_per_s, "6.1");
    assert_eq!(view.eta, "56 s");
    assert_eq!(
        view.tab_titles,
        ["Active 32", "Queued 5,349", "Failed 0", "Done 8,412"].map(String::from)
    );

    // Below 10 the speeds keep one decimal; the time left grows into minutes and hours.
    let slower = EngineSnapshot {
        files_per_s: 9.44,
        eta: Some(Duration::from_secs(4 * 60)),
        ..mockup()
    };
    let view = drawer_view(&slower, now());
    assert_eq!(
        (view.files_per_s.as_str(), view.eta.as_str()),
        ("9.4", "4 min")
    );
    let long = EngineSnapshot {
        eta: Some(Duration::from_secs(80 * 60)),
        ..mockup()
    };
    assert_eq!(drawer_view(&long, now()).eta, "1 h 20 min");
    let unknown = EngineSnapshot {
        eta: None,
        ..mockup()
    };
    assert_eq!(drawer_view(&unknown, now()).eta, "");

    // The other directions.
    let titled = |direction| {
        let view = drawer_view(
            &EngineSnapshot {
                direction,
                ..mockup()
            },
            now(),
        );
        (view.title, view.line, view.collapsed_text)
    };
    let (title, line, collapsed) = titled(Direction::Download);
    assert_eq!(title, "Downloading from example-shop.com");
    assert!(
        line.ends_with("· over HTTPS to sg.storage.bunnycdn.com"),
        "{line}"
    );
    assert!(
        collapsed.starts_with("Downloading 8,412 of 13,793 files"),
        "{collapsed}"
    );
    let (title, line, collapsed) = titled(Direction::Delete);
    assert_eq!(title, "Deleting from example-shop.com");
    assert_eq!(line, "8,412 of 13,793 items on sg.storage.bunnycdn.com");
    assert!(collapsed.starts_with("Deleting 8,412"), "{collapsed}");
    let (title, _, collapsed) = titled(Direction::Mixed);
    assert_eq!(title, "Transferring with example-shop.com");
    assert!(collapsed.starts_with("Transferring 8,412"), "{collapsed}");

    // The size unit follows the total, in whole numbers from 100 up.
    let small = EngineSnapshot {
        bytes_total: 12_897_485,
        bytes_done: 5_347_738,
        ..mockup()
    };
    assert_eq!(
        drawer_view(&small, now()).line,
        "8,412 of 13,793 files · 5.1 of 12.3 MB · over HTTPS to sg.storage.bunnycdn.com"
    );
}

#[test]
fn paused_offline_auth_titles() {
    let in_state = |state| drawer_view(&EngineSnapshot { state, ..mockup() }, now());

    let paused = in_state(EngineState::Paused);
    assert!(paused.running && paused.paused);
    assert_eq!(paused.title, "Paused · Uploading to example-shop.com");
    // Only a plain run quotes a speed; the collapsed line says why it isn't going.
    assert_eq!(paused.collapsed_text, paused.title);

    let offline = in_state(EngineState::Offline {
        site: "example-shop.com".into(),
    });
    assert!(offline.running && !offline.paused);
    assert_eq!(
        offline.title,
        "Waiting for the connection · Uploading to example-shop.com"
    );
    assert_eq!(offline.collapsed_text, offline.title);

    let auth = in_state(EngineState::AuthStopped {
        site: "media-archive".into(),
    });
    assert_eq!(auth.title, "Sign-in failed · media-archive");
    assert_eq!(auth.collapsed_text, auth.title);

    let disk = drawer_view(
        &EngineSnapshot {
            state: EngineState::DiskFull {
                drive: "D:".into(),
                needed: 3 * MB,
            },
            direction: Direction::Download,
            ..mockup()
        },
        now(),
    );
    assert_eq!(
        disk.title,
        "Not enough space on D: · Downloading from example-shop.com"
    );

    // The line is the same whatever the state.
    assert_eq!(paused.line, in_state(EngineState::Running).line);
}

#[test]
fn percent_is_files_done_over_total() {
    let view = drawer_view(&mockup(), now());
    let expected = (8_412.0 / 13_793.0 * 100.0) as f32;
    assert!((view.percent - expected).abs() < 0.001, "{}", view.percent);
    // Bytes do not count: half the bytes of none of the files is 0 %.
    let none = EngineSnapshot {
        files_done: 0,
        bytes_done: 432 * MB,
        ..mockup()
    };
    assert_eq!(drawer_view(&none, now()).percent, 0.0);
    let all = EngineSnapshot {
        files_done: 13_793,
        ..mockup()
    };
    assert_eq!(drawer_view(&all, now()).percent, 100.0);
    let empty = EngineSnapshot {
        files_total: 0,
        files_done: 0,
        ..mockup()
    };
    assert_eq!(drawer_view(&empty, now()).percent, 0.0);

    // The rows: an active one by its bytes, queued ones at 0, done ones at 100, failed ones as
    // far as they got, with their reason; a paused queued row says so.
    let mut paused = job_view(2, JobKind::Upload, "b.txt", 1024, 0);
    paused.paused = true;
    let mut failed = job_view(3, JobKind::Upload, "c.txt", 4 * MB, MB);
    failed.error = Some(httporter_transfer::messages::DAMAGED_TWICE.into());
    let s = EngineSnapshot {
        active: vec![job_view(1, JobKind::Upload, "a.txt", 4 * MB, MB)],
        queued: vec![job_view(4, JobKind::Download, "d.png", 10, 0), paused],
        failed: vec![failed],
        done: vec![job_view(5, JobKind::Delete, "old/e.html", 0, 0)],
        files_failed: 1,
        ..mockup()
    };
    let view = drawer_view(&s, now());
    let a = &view.active[0];
    assert_eq!(
        (a.id, a.name.as_str(), a.size.as_str()),
        (1, "a.txt", "4.0 MB")
    );
    assert_eq!((a.percent, a.detail.as_str(), a.paused), (25.0, "", false));
    assert_eq!(view.queued[0].percent, 0.0);
    assert_eq!(view.queued[0].detail, "");
    assert_eq!(view.queued[0].kind, crate::viewmodel::FileKindData::Image);
    assert_eq!(
        (view.queued[1].detail.as_str(), view.queued[1].paused),
        ("Paused", true)
    );
    assert_eq!(view.failed[0].percent, 25.0);
    assert_eq!(
        view.failed[0].detail,
        "The file arrived damaged twice (checksum mismatch)."
    );
    assert_eq!(view.done[0].percent, 100.0);
    assert_eq!(view.done[0].size, "0 B");
    assert_eq!(view.tab_titles[2], "Failed 1");
    // A folder made with New Folder shows a folder.
    let folder = EngineSnapshot {
        active: vec![job_view(6, JobKind::CreateFolder, "new", 0, 0)],
        ..mockup()
    };
    assert_eq!(
        drawer_view(&folder, now()).active[0].kind,
        crate::viewmodel::FileKindData::Folder
    );
}

#[test]
fn throttle_allows_10_per_second() {
    let mut pace = Throttle::new(REDRAW_EVERY);
    assert_eq!(REDRAW_EVERY, Duration::from_millis(100));
    let start = Instant::now();
    assert!(pace.ready(start), "the first one goes through");
    assert!(!pace.ready(start + Duration::from_millis(50)));
    assert!(!pace.ready(start + Duration::from_millis(99)));
    assert!(pace.ready(start + Duration::from_millis(100)));

    // A report every millisecond for a second: ten redraws.
    let mut pace = Throttle::new(REDRAW_EVERY);
    let passed = (0..1000)
        .filter(|ms| pace.ready(start + Duration::from_millis(*ms)))
        .count();
    assert_eq!(passed, 10);
}

#[test]
fn drawer_view_handles_50k_rows() {
    // The engine sends windows of at most 200 jobs per list, and counts for the rest.
    let window: Vec<JobView> = (0..200)
        .map(|i| {
            job_view(
                i + 1,
                JobKind::Upload,
                &format!("site/page-{i}.html"),
                4096,
                0,
            )
        })
        .collect();
    let s = EngineSnapshot {
        files_total: 50_000,
        bytes_total: 50_000 * 4096,
        active: window.clone(),
        active_count: 32,
        queued: window.clone(),
        queued_count: 49_768,
        done: window,
        done_count: 200,
        ..mockup()
    };
    // The best of a few runs, so that a busy machine does not fail the test.
    let fastest = (0..5)
        .map(|_| {
            let started = Instant::now();
            let view = drawer_view(&s, now());
            let took = started.elapsed();
            assert_eq!(view.queued.len(), 200);
            assert_eq!(view.tab_titles[1], "Queued 49,768");
            took
        })
        .min()
        .unwrap();
    assert!(fastest < Duration::from_millis(5), "{fastest:?}");
}

// ---- The drawer in the window ----

/// A window with one site (the drawer sits under the panes, which only show with a site), the
/// drawer's callbacks logged, and `view` applied.
fn window_with(view: &DrawerView) -> (AppWindow, Rc<RefCell<Vec<String>>>) {
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
    apply_drawer(&w, view);
    let log = Rc::new(RefCell::new(Vec::new()));
    let note = |text: &'static str| {
        let log = log.clone();
        move || log.borrow_mut().push(text.to_owned())
    };
    let note_id = |text: &'static str| {
        let log = log.clone();
        move |id: i32| log.borrow_mut().push(format!("{text} {id}"))
    };
    w.on_transfers_toggle(note("toggle"));
    w.on_transfers_pause(note("pause"));
    w.on_transfers_resume(note("resume"));
    w.on_transfers_cancel_all(note("cancel-all"));
    w.on_transfers_retry_failed(note("retry-failed"));
    w.on_transfer_cancel(note_id("cancel"));
    w.on_transfer_retry(note_id("retry"));
    w.on_transfer_move_to_top(note_id("move-to-top"));
    w.on_transfer_pause(note_id("pause"));
    w.on_transfer_resume(note_id("resume"));
    w.show().unwrap();
    (w, log)
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

/// The running mockup with a row on each tab.
fn busy_view() -> DrawerView {
    let mut failed = job_view(30, JobKind::Upload, "c.txt", 1024, 0);
    failed.error = Some("No connection".into());
    let s = EngineSnapshot {
        active: vec![job_view(10, JobKind::Upload, "a.txt", 2048, 1024)],
        queued: vec![job_view(20, JobKind::Upload, "b.txt", 1024, 0)],
        failed: vec![failed],
        done: vec![job_view(40, JobKind::Upload, "d.txt", 1024, 1024)],
        files_failed: 1,
        ..mockup()
    };
    drawer_view(&s, now())
}

#[test]
fn drawer_expands_and_tabs_switch() {
    let (w, log) = window_with(&busy_view());
    // Collapsed: the one line and Show Transfers.
    assert!(!w.get_transfers_open());
    assert_eq!(
        w.get_transfers_collapsed_text(),
        "Uploading 8,412 of 13,793 files · 96 files/s"
    );
    assert!(find(&w, AccessibleRole::Button, "Hide Transfers").is_none());
    press(&w, AccessibleRole::Button, "Show Transfers");
    assert!(w.get_transfers_open());
    assert_eq!(log.borrow().as_slice(), ["toggle"]);
    assert!(find(&w, AccessibleRole::Button, "Show Transfers").is_none());

    // Expanded: the tabs, Active first, with its row.
    for title in ["Active 32", "Queued 5,349", "Failed 1", "Done 8,412"] {
        assert!(find(&w, AccessibleRole::Tab, title).is_some(), "{title}");
    }
    assert_eq!(
        find(&w, AccessibleRole::Tab, "Active 32")
            .unwrap()
            .accessible_item_selected(),
        Some(true)
    );
    let row = |label: &str| find(&w, AccessibleRole::ListItem, label).is_some();
    assert!(row("a.txt, 2.0 KB, 50 percent"));
    assert!(!row("b.txt, 1.0 KB, waiting"));

    press(&w, AccessibleRole::Tab, "Queued 5,349");
    assert_eq!(w.get_transfers_tab(), 1);
    assert!(row("b.txt, 1.0 KB, waiting"));
    assert!(!row("a.txt, 2.0 KB, 50 percent"));
    press(&w, AccessibleRole::Tab, "Failed 1");
    assert_eq!(w.get_transfers_tab(), 2);
    assert!(row("c.txt, 1.0 KB, failed: No connection"));
    press(&w, AccessibleRole::Tab, "Done 8,412");
    assert!(row("d.txt, 1.0 KB, done"));

    // Pause and Cancel act on everything; a paused engine offers Resume instead.
    press(&w, AccessibleRole::Button, "Pause");
    press(&w, AccessibleRole::Button, "Cancel");
    assert!(find(&w, AccessibleRole::Button, "Resume").is_none());
    let mut paused = busy_view();
    paused.paused = true;
    apply_drawer(&w, &paused);
    assert!(find(&w, AccessibleRole::Button, "Pause").is_none());
    press(&w, AccessibleRole::Button, "Resume");

    press(&w, AccessibleRole::Button, "Hide Transfers");
    assert!(!w.get_transfers_open());
    assert_eq!(
        log.borrow().as_slice(),
        ["toggle", "pause", "cancel-all", "resume", "toggle"]
    );
}

#[test]
fn row_cancel_and_retry_callbacks_carry_ids() {
    let (w, log) = window_with(&busy_view());
    w.set_transfers_open(true);
    slint::platform::update_timers_and_animations();
    press(&w, AccessibleRole::Button, "Cancel a.txt");
    w.set_transfers_tab(1);
    slint::platform::update_timers_and_animations();
    press(&w, AccessibleRole::Button, "Cancel b.txt");
    w.set_transfers_tab(2);
    slint::platform::update_timers_and_animations();
    press(&w, AccessibleRole::Button, "Retry");
    press(&w, AccessibleRole::Button, "Retry All");
    assert_eq!(
        log.borrow().as_slice(),
        ["cancel 10", "cancel 20", "retry 30", "retry-failed"]
    );
    // Retry All is only on the Failed tab, and only with something failed.
    w.set_transfers_tab(3);
    slint::platform::update_timers_and_animations();
    assert!(find(&w, AccessibleRole::Button, "Retry All").is_none());
    assert!(find(&w, AccessibleRole::Button, "Cancel d.txt").is_none());
}

#[test]
fn an_idle_drawer_has_its_empty_lines_and_nothing_to_pause() {
    let (w, _) = window_with(&drawer_view(&EngineSnapshot::default(), now()));
    assert_eq!(w.get_transfers_collapsed_text(), "No transfers running");
    w.set_transfers_open(true);
    slint::platform::update_timers_and_animations();
    let pause = find(&w, AccessibleRole::Button, "Pause").unwrap();
    assert_eq!(pause.accessible_enabled(), Some(false));
    for (tab, empty) in [
        (0, "Nothing is transferring."),
        (1, "Nothing is waiting."),
        (2, "Nothing has failed."),
        (3, "Nothing has finished yet."),
    ] {
        w.set_transfers_tab(tab);
        slint::platform::update_timers_and_animations();
        assert_eq!(
            ElementHandle::find_by_accessible_label(&w, empty).count(),
            1,
            "{empty}"
        );
    }
}

#[test]
fn applying_a_view_keeps_the_models_and_updates_rows_in_place() {
    let (w, _) = window_with(&busy_view());
    let before = w.get_transfers_active();
    let mut next = busy_view();
    next.active[0].percent = 75.0;
    next.active.push(TransferRowData {
        id: 11,
        ..next.active[0].clone()
    });
    apply_drawer(&w, &next);
    let after = w.get_transfers_active();
    // The same model, so the list keeps its scroll position and focus.
    assert!(before == after, "the same model");
    assert_eq!(after.row_count(), 2);
    assert_eq!(after.row_data(0).unwrap().percent, 75.0);
    assert_eq!(after.row_data(1).unwrap().id, 11);
    next.active.clear();
    apply_drawer(&w, &next);
    assert_eq!(w.get_transfers_active().row_count(), 0);
}
