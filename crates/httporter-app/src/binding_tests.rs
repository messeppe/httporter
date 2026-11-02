//! Tests of the binding: the window's callbacks reach the session, and views reach the window.

use super::*;
use crate::session::fakes::{FakeFactory, Gated, session_with};
use crate::viewmodel::{FileKindData, RowData};
use httporter_core::{MockProvider, StorageProvider};
use i_slint_backend_testing as testing;
use slint::Model;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

struct Bound {
    ui: AppWindow,
    /// Runs the tasks the window starts; they stop when it is dropped.
    rt: Runtime,
    session: Arc<Session>,
    factory: Arc<FakeFactory>,
}

fn bound(provider: Arc<dyn StorageProvider>) -> Bound {
    testing::init_no_event_loop();
    let rt = Runtime::new().unwrap();
    let (session, _creds, factory) = session_with(provider);
    let ui = AppWindow::new().unwrap();
    let session = install(&ui, rt.handle().clone(), session);
    Bound {
        ui,
        rt,
        session,
        factory,
    }
}

fn empty() -> Arc<dyn StorageProvider> {
    Arc::new(MockProvider::new(Default::default()))
}

/// Polls `f` until it has an answer: the spawned task runs on another thread.
fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn row(name: &str, folder: bool) -> RowData {
    RowData {
        name: name.into(),
        kind: if folder {
            FileKindData::Folder
        } else {
            FileKindData::Doc
        },
        size: "1 B".into(),
        modified: "Today 10:02".into(),
        folder,
    }
}

fn row_names(ui: &AppWindow) -> Vec<String> {
    ui.get_remote_rows()
        .iter()
        .map(|r| r.name.to_string())
        .collect()
}

fn strings(model: slint::ModelRc<slint::SharedString>) -> Vec<String> {
    model.iter().map(|s| s.to_string()).collect()
}

/// Note for Task 15: the sheet's four arguments reach the session as (name, zone, region,
/// password), for Test Connection and for Add Site alike.
#[test]
fn the_sheet_reports_its_fields_to_the_session_in_order() {
    let b = bound(empty());
    b.ui.invoke_add_site_test("My site".into(), "testzone".into(), 4, "pw".into());
    // Set before the task starts, so a second press cannot start another.
    assert!(b.ui.get_add_site_busy());
    let tested = wait_for("the connection test", || {
        b.factory.configs().first().cloned()
    });
    assert_eq!(
        tested,
        (
            "testzone".to_owned(),
            "sg.storage.bunnycdn.com".to_owned(),
            "pw".to_owned()
        )
    );

    b.ui.invoke_add_site_save("My site".into(), "testzone".into(), 4, "pw".into());
    let saved = wait_for("the save", || b.session.sites().first().cloned());
    assert_eq!(
        (
            saved.name.as_str(),
            saved.zone.as_str(),
            saved.region.as_str()
        ),
        ("My site", "testzone", "sg")
    );
}

#[test]
fn an_invalid_form_shows_its_field_error_and_starts_nothing() {
    let b = bound(empty());
    b.ui.invoke_add_site_test("".into(), "".into(), 0, "".into());
    assert_eq!(
        b.ui.get_add_site_zone_error(),
        "Enter the storage zone name."
    );
    assert!(!b.ui.get_add_site_busy());

    b.ui.invoke_add_site_save("name".into(), "zone".into(), -1, "pw".into());
    assert_eq!(b.ui.get_add_site_region_error(), "Choose a region.");
    assert_eq!(b.ui.get_add_site_zone_error(), "");
    assert!(!b.ui.get_add_site_busy());

    assert!(b.factory.configs().is_empty());
    assert!(b.session.sites().is_empty());
}

#[test]
fn cancel_gives_up_on_a_running_test_and_clears_busy() {
    // Its root listing is held back for good, like a server that stopped answering.
    let hanging = Gated::new(MockProvider::new(Default::default()), "");
    let b = bound(hanging.clone());
    // References to the storage: this test's and the factory's. A running test holds a third.
    let idle = Arc::strong_count(&hanging);
    b.ui.invoke_add_site_test("".into(), "testzone".into(), 4, "pw".into());
    assert!(b.ui.get_add_site_busy());
    wait_for("the test to start", || {
        (Arc::strong_count(&hanging) > idle).then_some(())
    });

    b.ui.invoke_add_site_cancel();
    assert!(!b.ui.get_add_site_busy());
    // The listing that would never have finished is dropped.
    wait_for("the test to be given up", || {
        (Arc::strong_count(&hanging) == idle).then_some(())
    });
}

#[test]
fn the_sheet_endpoint_follows_zone_and_region() {
    let b = bound(empty());
    b.ui.invoke_add_site_changed("testzone".into(), 4);
    assert_eq!(
        b.ui.get_add_site_endpoint(),
        "https://sg.storage.bunnycdn.com/testzone/"
    );
    b.ui.invoke_add_site_changed("testzone".into(), -1);
    assert_eq!(b.ui.get_add_site_endpoint(), "");
}

#[test]
fn install_fills_the_region_list_and_the_drives() {
    let b = bound(empty());
    let regions = strings(b.ui.get_region_names());
    assert_eq!(regions.len(), 9);
    assert_eq!(regions[0], "Frankfurt");
    assert_eq!(regions[4], "Singapore");
    assert!(b.ui.get_local_rows().iter().any(|r| r.name == "C:"));
    assert!(b.ui.get_local_crumbs().row_count() == 0);
    assert_eq!(b.ui.get_selected_site(), -1);
    assert_eq!(b.ui.get_sites().row_count(), 0);
}

#[test]
fn the_first_site_is_opened_at_start() {
    testing::init_no_event_loop();
    let rt = Runtime::new().unwrap();
    let (session, _creds, factory) = session_with(empty());
    for (name, zone, region) in [("beta", "zone-b", "sg"), ("alpha", "zone-a", "de")] {
        let site = ValidSite {
            name: name.into(),
            zone: zone.into(),
            region_code: region.into(),
            password: "pw".into(),
        };
        rt.block_on(session.save_site(&site)).unwrap();
    }
    let ui = AppWindow::new().unwrap();
    install(&ui, rt.handle().clone(), session);
    assert_eq!(ui.get_sites().row_count(), 2);
    assert_eq!(ui.get_sites().row_data(0).unwrap().name, "alpha");
    assert_eq!(ui.get_selected_site(), 0);
    let opened = wait_for("the first site to open", || {
        factory.configs().first().cloned()
    });
    assert_eq!(opened.0, "zone-a");
}

#[test]
fn picking_a_site_empties_the_cloud_pane_and_drops_its_error_only() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_remote_rows(to_slint_rows(&[row("old.txt", false)]));
    ui.set_remote_crumbs(super::strings(&["old-zone".into()]));
    ui.set_remote_selected(0);
    ui.set_status_left("3 items".into());
    let shown = Mutex::new(Shown {
        remote_error: Some("Couldn't sign in to old-zone.".into()),
        local_error: Some("Couldn't open D:\\x: Access is denied.".into()),
        ..Shown::default()
    });
    start_opening_site(&ui, &shown, None);
    assert_eq!(ui.get_remote_rows().row_count(), 0);
    assert_eq!(ui.get_remote_crumbs().row_count(), 0);
    assert_eq!(ui.get_remote_selected(), -1);
    assert_eq!(ui.get_status_left(), "3 items");
    assert_eq!(
        ui.get_error_text(),
        "Couldn't open D:\\x: Access is denied."
    );
}

#[test]
fn the_banner_has_the_notice_then_each_pane_s_error() {
    let shown = Shown {
        remote_error: Some("cloud".into()),
        local_error: Some("this pc".into()),
        ..Shown::default()
    };
    assert_eq!(
        shown.banner(Some("notice".into())),
        "notice\ncloud\nthis pc"
    );
    assert_eq!(shown.banner(None), "cloud\nthis pc");
    assert_eq!(Shown::default().banner(None), "");
}

/// Ruling P21: views from tasks that ran side by side can arrive out of order; the window never
/// puts an older one over a newer one.
#[test]
fn an_older_view_never_replaces_a_newer_one() {
    let b = bound(empty());
    let shown = Mutex::new(Shown::default());
    let newer = RemoteView {
        version: 5,
        rows: vec![row("new.txt", false)],
        ..RemoteView::default()
    };
    let older = RemoteView {
        version: 4,
        rows: vec![row("old.txt", false)],
        error: Some("an old failure".into()),
        ..RemoteView::default()
    };
    show_remote(&b.ui, &shown, &b.session, &newer, Change::Listed);
    show_remote(&b.ui, &shown, &b.session, &older, Change::Listed);
    assert_eq!(row_names(&b.ui), ["new.txt"]);
    assert_eq!(b.ui.get_error_text(), "");

    let newer = LocalView {
        version: 7,
        rows: vec![row("now", true)],
        ..LocalView::default()
    };
    let older = LocalView {
        version: 6,
        rows: vec![row("before", true)],
        error: Some("an old failure".into()),
        ..LocalView::default()
    };
    show_local(&b.ui, &shown, &b.session, &newer, Change::Listed);
    show_local(&b.ui, &shown, &b.session, &older, Change::Listed);
    assert_eq!(b.ui.get_local_rows().row_data(0).unwrap().name, "now");
    assert_eq!(b.ui.get_error_text(), "");
}

/// Ruling P21 (2), through the window: a site is added while the site opened at start hangs.
#[test]
fn a_site_can_be_added_while_a_cloud_listing_hangs() {
    testing::init_no_event_loop();
    let rt = Runtime::new().unwrap();
    let hanging = Gated::new(MockProvider::new(Default::default()), "");
    let (session, _creds, factory) = session_with(empty());
    factory.serve("hanging-zone", hanging.clone());
    let site = ValidSite {
        name: "hanging".into(),
        zone: "hanging-zone".into(),
        region_code: "sg".into(),
        password: "pw".into(),
    };
    rt.block_on(session.save_site(&site)).unwrap();
    let ui = AppWindow::new().unwrap();
    let session = install(&ui, rt.handle().clone(), session);
    wait_for("the first site's listing to hang", || {
        (hanging.waiting() == 1).then_some(())
    });

    ui.invoke_add_site_save("other".into(), "other-zone".into(), 0, "pw".into());
    wait_for("the save", || (session.sites().len() == 2).then_some(()));
    ui.invoke_add_site_test("".into(), "other-zone".into(), 0, "pw".into());
    wait_for("the test", || {
        (factory
            .configs()
            .iter()
            .filter(|c| c.0 == "other-zone")
            .count()
            == 1)
            .then_some(())
    });
    assert_eq!(
        hanging.waiting(),
        1,
        "the first site's listing is still out"
    );
}

#[test]
fn a_remote_view_is_applied_to_the_window() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let view = RemoteView {
        version: 1,
        rows: vec![row("wp-content", true), row("index.html", false)],
        crumbs: vec!["testzone".into(), "wp-content".into()],
        connection: "HTTPS · 21 ms".into(),
        connection_ok: true,
        endpoint: "Bunny Storage · testzone".into(),
        chip: "https://sg.storage.bunnycdn.com".into(),
        status_right: "HTTP/2 · sg.storage.bunnycdn.com".into(),
        can_back: true,
        can_forward: false,
        error: None,
    };
    ui.set_remote_selected(1);
    apply_remote(&ui, &view, Change::Listed);
    assert_eq!(row_names(&ui), ["wp-content", "index.html"]);
    assert_eq!(strings(ui.get_remote_crumbs()), ["testzone", "wp-content"]);
    assert_eq!(ui.get_connection_text(), "HTTPS · 21 ms");
    assert!(ui.get_connection_ok());
    assert_eq!(ui.get_endpoint_text(), "Bunny Storage · testzone");
    assert_eq!(ui.get_remote_chip(), "https://sg.storage.bunnycdn.com");
    assert_eq!(ui.get_status_right(), "HTTP/2 · sg.storage.bunnycdn.com");
    assert_eq!(ui.get_status_left(), "2 items");
    assert_eq!(ui.get_remote_selected(), -1);
    assert!(ui.get_remote_can_go_back() && !ui.get_remote_can_go_forward());

    // A failed connection turns the pill orange; the item count is left alone.
    let failed = RemoteView {
        connection: "Not connected".into(),
        connection_ok: false,
        status_right: String::new(),
        error: Some("Couldn't reach x: y".into()),
        rows: vec![row("a", false)],
        ..view
    };
    apply_remote(&ui, &failed, Change::Listed);
    assert!(!ui.get_connection_ok());
    assert_eq!(ui.get_connection_text(), "Not connected");
    assert_eq!(ui.get_status_left(), "2 items");
}

#[test]
fn a_local_view_is_applied_to_the_window() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let view = LocalView {
        version: 1,
        rows: vec![row("sub", true), row("a.txt", false)],
        crumbs: vec!["D:".into(), "site".into()],
        status: "2 items · 1 couldn't be read".into(),
        can_back: false,
        can_forward: true,
        error: None,
    };
    ui.set_local_selected(0);
    apply_local(&ui, &view, Change::Listed);
    assert!(!ui.get_local_can_go_back() && ui.get_local_can_go_forward());
    assert_eq!(ui.get_local_rows().row_count(), 2);
    assert_eq!(strings(ui.get_local_crumbs()), ["D:", "site"]);
    assert_eq!(ui.get_status_left(), "2 items · 1 couldn't be read");
    assert_eq!(ui.get_local_selected(), -1);
}

#[test]
fn sorting_keeps_the_selected_row_selected() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let listed = RemoteView {
        rows: vec![row("a", false), row("b", false), row("c", false)],
        ..RemoteView::default()
    };
    apply_remote(&ui, &listed, Change::Listed);
    ui.set_remote_selected(2);
    ui.set_status_left("dropped".into());
    let sorted = RemoteView {
        rows: vec![row("c", false), row("b", false), row("a", false)],
        ..RemoteView::default()
    };
    apply_remote(&ui, &sorted, Change::Sorted);
    assert_eq!(row_names(&ui), ["c", "b", "a"]);
    assert_eq!(ui.get_remote_selected(), 0);
    // A sort is not news for the status line.
    assert_eq!(ui.get_status_left(), "dropped");
}

#[test]
fn site_rows_are_updated_in_place() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let item = |name: &str, healthy| SiteItemData {
        id: name.into(),
        name: name.into(),
        subtitle: "Bunny · Singapore".into(),
        mono: "B".into(),
        healthy,
    };
    sync_sites(&ui, &[item("a", true), item("b", true)]);
    let model = ui.get_sites();
    assert_eq!(model.row_count(), 2);

    // Same sites, one turned unhealthy: the rows change, the model (and the row that may hold
    // the keyboard focus) stays.
    sync_sites(&ui, &[item("a", false), item("b", true)]);
    assert!(ui.get_sites() == model);
    assert!(!model.row_data(0).unwrap().healthy);

    // A new site replaces the list.
    sync_sites(&ui, &[item("a", false), item("b", true), item("c", true)]);
    assert_eq!(ui.get_sites().row_count(), 3);
}

/// Ruling P22: Back and Forward go to the pane last used: a click in This PC sends them there,
/// a click in the cloud pane (or nothing yet) to the cloud pane.
#[test]
fn back_and_forward_act_on_the_pane_last_used() {
    let b = bound(empty());
    // This PC goes into drive C: (through the session, as a click would).
    let drives = b.session.local_view();
    let c = drives.rows.iter().position(|r| r.name == "C:").unwrap();
    let ticket = b.session.local_ticket();
    let inside =
        b.rt.block_on(b.session.open_local(ticket, c, drives.version));
    assert_eq!(inside.crumbs, ["C:"]);
    assert!(inside.can_back);

    // The cloud pane is the one at first: Back there has nowhere to go and This PC stays put.
    assert!(!b.ui.get_local_pane_active());
    b.ui.invoke_go_back();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(b.session.local_view().crumbs, ["C:"]);

    // After using This PC, Back takes it back to the drives.
    b.ui.invoke_sort_local(0);
    assert!(b.ui.get_local_pane_active());
    b.ui.invoke_go_back();
    wait_for("This PC to go back", || {
        b.session.local_view().crumbs.is_empty().then_some(())
    });
    b.ui.invoke_go_forward();
    wait_for("This PC to go forward", || {
        (b.session.local_view().crumbs == ["C:"]).then_some(())
    });

    // Using the cloud pane makes it the one again.
    b.ui.invoke_remote_up();
    assert!(!b.ui.get_local_pane_active());
}
