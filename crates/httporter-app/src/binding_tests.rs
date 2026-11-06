// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests of the binding: the window's callbacks reach the session, and views reach the window.

use super::*;
use crate::session::fakes::{FakeFactory, Gated, SpyStore, fixed_now, session_with};
use crate::viewmodel::{FileKindData, RowData};
use httporter_core::{AuthHint, MockOp, MockProvider, ProviderError, StorageProvider};
use httporter_platform::Store;
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
        bytes: 1,
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
            saved.container.as_str(),
            saved.region.as_str()
        ),
        ("My site", "testzone", "sg")
    );
}

#[test]
fn an_invalid_form_shows_its_field_error_and_starts_nothing() {
    let b = bound(empty());
    // Task 24: every problem is shown at once, each under its own field.
    b.ui.invoke_add_site_test("".into(), "".into(), 0, "".into());
    assert_eq!(
        b.ui.get_add_site_zone_error(),
        "Enter the storage zone name."
    );
    assert_eq!(
        b.ui.get_add_site_password_error(),
        "Enter the zone's password."
    );
    assert_eq!(b.ui.get_add_site_region_error(), "");
    assert!(!b.ui.get_add_site_busy());

    b.ui.invoke_add_site_save("name".into(), "zone".into(), -1, "pw".into());
    assert_eq!(b.ui.get_add_site_region_error(), "Choose a region.");
    assert_eq!(b.ui.get_add_site_zone_error(), "");
    assert_eq!(b.ui.get_add_site_password_error(), "");
    assert!(!b.ui.get_add_site_busy());

    // An S3 form's problems go under the S3 fields.
    let draft = SiteDraft {
        provider: 1,
        ..SiteDraft::default()
    };
    b.ui.invoke_add_site_save_s3(draft, "".into());
    let errors = b.ui.get_add_site_s3_errors();
    assert_eq!(
        (
            errors.region.as_str(),
            errors.bucket.as_str(),
            errors.key_id.as_str(),
            errors.secret.as_str(),
            errors.account_id.as_str()
        ),
        (
            "Choose a region.",
            "Enter the bucket name.",
            "Enter the access key ID.",
            "Enter the secret access key.",
            ""
        )
    );
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
    ui.set_remote_status("3 items".into());
    let shown = Mutex::new(Shown {
        remote_error: Some("Couldn't sign in to old-zone.".into()),
        local_error: Some("Couldn't open D:\\x: Access is denied.".into()),
        ..Shown::default()
    });
    start_opening_site(&ui, &shown, None);
    assert_eq!(ui.get_remote_rows().row_count(), 0);
    assert_eq!(ui.get_remote_crumbs().row_count(), 0);
    assert_eq!(ui.get_remote_selected(), -1);
    // Task 25 (item 1): nothing is known of the new site yet, so the status bar says it is
    // connecting rather than keep the old site's count.
    assert_eq!(ui.get_status_left(), "Connecting…");
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
    // One client for the test made before saving, one for Test Connection.
    wait_for("the test", || {
        (factory
            .configs()
            .iter()
            .filter(|c| c.0 == "other-zone")
            .count()
            == 2)
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
        loading: false,
        error: None,
        ..RemoteView::default()
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
        error: Some("Couldn't reach x. Check your internet connection and try again.".into()),
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
        loading: false,
        error: None,
    };
    ui.set_local_selected(0);
    apply_local(&ui, &view, Change::Listed);
    assert!(!ui.get_local_can_go_back() && ui.get_local_can_go_forward());
    assert_eq!(ui.get_local_rows().row_count(), 2);
    assert_eq!(strings(ui.get_local_crumbs()), ["D:", "site"]);
    assert_eq!(ui.get_local_status(), "2 items · 1 couldn't be read");
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
    ui.set_status_message("dropped".into());
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
        provider: SiteProvider::Bunny,
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

/// Ledger 129: the row's tile is the provider's, whatever the site is called.
#[test]
fn site_rows_carry_the_provider_tile() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let item = |name: &str, provider: SiteProvider| SiteItemData {
        id: name.into(),
        name: name.into(),
        subtitle: String::new(),
        provider,
        healthy: true,
    };
    sync_sites(
        &ui,
        &[
            item("R2 backup", SiteProvider::Bunny),
            item("example-shop.com", SiteProvider::S3(S3Preset::CloudflareR2)),
            item("archive", SiteProvider::S3(S3Preset::Other)),
        ],
    );
    let rows: Vec<(ProviderTile, String)> = ui
        .get_sites()
        .iter()
        .map(|row| (row.provider, row.mono.to_string()))
        .collect();
    assert_eq!(
        rows,
        [
            (ProviderTile::Bunny, "B".to_owned()),
            (ProviderTile::R2, "R2".to_owned()),
            (ProviderTile::Other, "…".to_owned()),
        ]
    );
}

/// Every provider tile has its own colour from the theme (mockup `providers`).
#[test]
fn each_provider_tile_has_its_theme_colour() {
    use crate::Theme;
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let theme = ui.global::<Theme>();
    let presets = [
        S3Preset::AwsS3,
        S3Preset::CloudflareR2,
        S3Preset::BackblazeB2,
        S3Preset::Wasabi,
        S3Preset::DigitalOceanSpaces,
        S3Preset::Minio,
        S3Preset::Other,
    ];
    let expected = [
        theme.get_provider_s3(),
        theme.get_provider_r2(),
        theme.get_provider_b2(),
        theme.get_provider_wasabi(),
        theme.get_provider_do(),
        theme.get_provider_minio(),
        theme.get_provider_other(),
    ];
    assert_eq!(
        theme.invoke_provider_tint(slint_tile(SiteProvider::Bunny)),
        theme.get_provider_bunny()
    );
    for (preset, colour) in presets.into_iter().zip(expected) {
        let tile = slint_tile(SiteProvider::S3(preset));
        assert_eq!(theme.invoke_provider_tint(tile), colour, "{preset:?}");
    }
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
        b.rt.block_on(b.session.open_local(ticket, c, drives.version, |_| {}));
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

// ---- Final fix wave ----

const AUTH: &str = "Couldn't sign in to testzone. Check the password, zone name and region.";

fn valid(zone: &str) -> ValidSite {
    ValidSite {
        name: zone.into(),
        zone: zone.into(),
        region_code: "sg".into(),
        password: "pw".into(),
    }
}

/// Important #1: Add Site (the sheet's default button, also pressed by Return in a field) tests
/// the connection first, and a rejected password saves nothing.
#[test]
fn add_site_with_a_rejected_password_saves_nothing() {
    let mock = Arc::new(MockProvider::new(Default::default()));
    let b = bound(mock.clone());
    let rejected = ProviderError::Auth {
        hint: AuthHint::Unknown,
    };
    mock.fail_next(MockOp::List, rejected, 1);
    // References to the storage: this test's and the factory's. A running test holds a third.
    let idle = Arc::strong_count(&mock);
    b.ui.invoke_add_site_save("".into(), "testzone".into(), 4, "wrong".into());
    assert!(b.ui.get_add_site_busy(), "busy while the test runs");
    wait_for("the zone to be listed", || {
        (mock.calls(MockOp::List) == 1).then_some(())
    });
    wait_for("the test to end", || {
        (Arc::strong_count(&mock) == idle).then_some(())
    });
    assert!(b.session.sites().is_empty());
    assert_eq!(
        b.factory.configs(),
        [(
            "testzone".to_owned(),
            "sg.storage.bunnycdn.com".to_owned(),
            "wrong".to_owned()
        )]
    );
}

/// The sheet stays busy while Add Site tests the zone, and Cancel gives the test up: the site is
/// never saved.
#[test]
fn cancel_gives_up_on_adding_a_site_while_it_is_tested() {
    // Its root listing is held back for good, like a server that stopped answering.
    let hanging = Gated::new(MockProvider::new(Default::default()), "");
    let b = bound(hanging.clone());
    let idle = Arc::strong_count(&hanging);
    b.ui.invoke_add_site_save("".into(), "testzone".into(), 4, "pw".into());
    assert!(b.ui.get_add_site_busy());
    wait_for("the test to start", || {
        (hanging.waiting() == 1).then_some(())
    });
    assert!(
        b.session.sites().is_empty(),
        "nothing is saved before the test"
    );

    b.ui.invoke_add_site_cancel();
    assert!(!b.ui.get_add_site_busy());
    wait_for("the test to be given up", || {
        (Arc::strong_count(&hanging) == idle).then_some(())
    });
    assert_eq!(hanging.waiting(), 0);
    assert!(b.session.sites().is_empty());
}

#[test]
fn a_site_that_fails_its_test_keeps_the_sheet_open_and_says_why() {
    let b = bound(empty());
    b.ui.set_add_site_open(true);
    b.ui.set_add_site_busy(true);
    show_added(&b.ui, &b.session, true, Err(AUTH.into()));
    assert!(b.ui.get_add_site_open());
    assert!(!b.ui.get_add_site_busy());
    assert_eq!(b.ui.get_add_site_result(), AUTH);
    assert!(!b.ui.get_add_site_result_ok());
    assert_eq!(b.ui.get_sites().row_count(), 0);

    // After a cancel (or once the sheet has been reopened) the old answer is not shown.
    b.ui.set_add_site_result("".into());
    b.ui.set_add_site_busy(true);
    show_added(&b.ui, &b.session, false, Err(AUTH.into()));
    assert_eq!(b.ui.get_add_site_result(), "");
    assert!(b.ui.get_add_site_busy());
}

#[test]
fn a_site_that_passes_its_test_is_saved_once_and_opened() {
    let mock = Arc::new(MockProvider::new(Default::default()));
    let b = bound(mock.clone());
    b.ui.set_add_site_open(true);
    b.ui.invoke_add_site_save("".into(), "testzone".into(), 4, "pw".into());
    let saved = wait_for("the save", || b.session.sites().first().cloned());
    assert_eq!(mock.calls(MockOp::List), 1, "one listing: the test");
    assert_eq!(b.session.sites().len(), 1);

    show_added(&b.ui, &b.session, true, Ok(saved));
    assert!(!b.ui.get_add_site_open());
    assert!(!b.ui.get_add_site_busy());
    assert_eq!(b.ui.get_sites().row_count(), 1);
    assert_eq!(b.ui.get_selected_site(), 0);
    wait_for("the new site to open", || {
        (b.factory.configs().len() == 2).then_some(())
    });
}

/// Task 16 review: Cancel gives up a test that is running, but a site whose password is already
/// being written is saved all the same (the sheet is gone, the site exists), and listed once.
#[test]
fn cancel_does_not_stop_a_site_that_is_being_saved() {
    testing::init_no_event_loop();
    let rt = Runtime::new().unwrap();
    let creds = Arc::new(SpyStore::default());
    let hold = creds.hold_set();
    let session = Session::new(
        Store::open_in_memory().unwrap(),
        creds.clone(),
        FakeFactory::new(empty()),
        fixed_now,
    );
    let ui = AppWindow::new().unwrap();
    let session = install(&ui, rt.handle().clone(), session);

    // The test passes (the zone lists), and the password is on its way to the credential store.
    ui.invoke_add_site_save("".into(), "testzone".into(), 4, "pw".into());
    hold.wait_entered();
    assert!(ui.get_add_site_busy());
    ui.invoke_add_site_cancel();
    assert!(!ui.get_add_site_busy());
    assert!(session.sites().is_empty(), "the write has not finished");

    hold.release();
    wait_for("the site to be listed", || {
        (!session.sites().is_empty()).then_some(())
    });
    assert_eq!(session.sites().len(), 1, "listed once");
    assert_eq!(session.site_items().len(), 1);
}

/// Minor 2: a loading view marks the pane and leaves its rows and selection alone; the answer
/// clears it.
#[test]
fn a_loading_view_marks_the_pane_and_leaves_its_rows() {
    let b = bound(empty());
    let shown = Mutex::new(Shown::default());
    let listed = RemoteView {
        version: 1,
        rows: vec![row("a", false), row("b", false)],
        ..RemoteView::default()
    };
    show_remote(&b.ui, &shown, &b.session, &listed, Change::Listed);
    b.ui.set_remote_selected(1);
    let loading = RemoteView {
        version: 2,
        loading: true,
        ..listed.clone()
    };
    show_remote(&b.ui, &shown, &b.session, &loading, Change::Loading);
    assert!(b.ui.get_remote_loading());
    assert!(!b.ui.get_local_loading());
    assert_eq!(row_names(&b.ui), ["a", "b"]);
    assert_eq!(b.ui.get_remote_selected(), 1);
    let done = RemoteView {
        version: 3,
        rows: vec![row("c", false)],
        ..RemoteView::default()
    };
    show_remote(&b.ui, &shown, &b.session, &done, Change::Listed);
    assert!(!b.ui.get_remote_loading());
    assert_eq!(row_names(&b.ui), ["c"]);

    let loading = LocalView {
        version: 9,
        rows: vec![row("C:", true)],
        loading: true,
        ..LocalView::default()
    };
    show_local(&b.ui, &shown, &b.session, &loading, Change::Loading);
    assert!(b.ui.get_local_loading());
    let failed = LocalView {
        version: 10,
        loading: false,
        error: Some("Couldn't open D:\\x: Access is denied.".into()),
        ..loading
    };
    show_local(&b.ui, &shown, &b.session, &failed, Change::Listed);
    assert!(!b.ui.get_local_loading());
}

/// Minor 3: F5 and Ctrl+R list the pane last used again, by the same rule as Back and Forward.
#[test]
fn refresh_lists_the_pane_last_used_again() {
    let mock = Arc::new(MockProvider::new(Default::default()).with_file("a.txt", b"a"));
    let b = bound(mock.clone());
    b.rt.block_on(b.session.save_site(&valid("testzone")))
        .unwrap();
    let ticket = b.session.remote_ticket();
    b.rt.block_on(b.session.select_site(ticket, 0, |_| {}));
    let calls = mock.calls(MockOp::List);

    // The cloud pane is the one at first.
    assert!(!b.ui.get_local_pane_active());
    let local = b.session.local_view().version;
    b.ui.invoke_refresh();
    wait_for("the cloud folder to be listed again", || {
        (mock.calls(MockOp::List) == calls + 1).then_some(())
    });

    // Once This PC has been used, it is the one refreshed.
    b.ui.set_local_pane_active(true);
    b.ui.invoke_refresh();
    wait_for("This PC to be listed again", || {
        (b.session.local_view().version >= local + 2).then_some(())
    });
    assert_eq!(mock.calls(MockOp::List), calls + 1);
}

/// Minor 3: a refreshed folder keeps the selected row (found by name) and updates the count.
#[test]
fn a_refresh_keeps_the_selected_row_by_name() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let listed = RemoteView {
        rows: vec![row("a", false), row("b", false), row("c", false)],
        ..RemoteView::default()
    };
    apply_remote(&ui, &listed, Change::Listed);
    ui.set_remote_selected(1);
    let refreshed = RemoteView {
        rows: vec![
            row("a", false),
            row("a2", false),
            row("b", false),
            row("c", false),
        ],
        ..RemoteView::default()
    };
    apply_remote(&ui, &refreshed, Change::Refreshed);
    assert_eq!(ui.get_remote_selected(), 2);
    assert_eq!(ui.get_status_left(), "4 items");
}

/// Minor 11: each pane has its own count, and the status bar shows the one of the pane last used.
#[test]
fn the_status_bar_counts_the_items_of_the_pane_last_used() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let local = LocalView {
        rows: vec![row("a", false), row("b", false), row("c", false)],
        status: "3 items".into(),
        ..LocalView::default()
    };
    let remote = |n: usize| RemoteView {
        rows: (0..n).map(|i| row(&format!("r{i}"), false)).collect(),
        ..RemoteView::default()
    };
    apply_local(&ui, &local, Change::Listed);
    apply_remote(&ui, &remote(2), Change::Listed);
    assert!(!ui.get_local_pane_active());
    assert_eq!(ui.get_status_left(), "2 items");
    ui.set_local_pane_active(true);
    assert_eq!(ui.get_status_left(), "3 items");

    // A listing in the other pane does not take the status bar over.
    apply_remote(&ui, &remote(5), Change::Listed);
    assert_eq!(ui.get_status_left(), "3 items");
    ui.set_local_pane_active(false);
    assert_eq!(ui.get_status_left(), "5 items");

    // A message (what a drop did) shows until that pane is listed again.
    ui.set_status_message("dropped".into());
    assert_eq!(ui.get_status_left(), "dropped");
    apply_remote(&ui, &remote(1), Change::Listed);
    assert_eq!(ui.get_status_left(), "1 item");
}

// ---- Task 24: Welcome, the site menu, and the banner's way out ----

/// How many elements of the window carry this accessible label.
fn labelled(ui: &AppWindow, label: &str) -> usize {
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, label).count()
}

const TAGLINE: &str = "Fast, safe file transfers to cloud storage over HTTP.";

#[test]
fn welcome_shows_only_without_sites() {
    let b = bound(empty());
    b.ui.show().unwrap();
    // Every control of the second round of Add Site is wired (Ruling Q10).
    assert!(b.ui.get_sites_v2_ready());
    assert_eq!(labelled(&b.ui, TAGLINE), 1);
    assert_eq!(b.ui.get_selected_site(), -1);

    // With a site saved, the panes show instead.
    let (session, _creds, _factory) = session_with(empty());
    b.rt.block_on(session.save_site(&valid("testzone")))
        .unwrap();
    let ui = AppWindow::new().unwrap();
    install(&ui, b.rt.handle().clone(), session);
    ui.show().unwrap();
    assert_eq!(labelled(&ui, TAGLINE), 0);
    assert_eq!(ui.get_selected_site(), 0);
}

#[test]
fn the_site_menu_duplicates_and_removes_after_asking() {
    testing::init_no_event_loop();
    let rt = Runtime::new().unwrap();
    let (session, creds, _factory) = session_with(empty());
    let site = rt.block_on(session.save_site(&valid("testzone"))).unwrap();
    let ui = AppWindow::new().unwrap();
    let session = install(&ui, rt.handle().clone(), session);

    // Duplicate: a copy of the site and its password.
    ui.invoke_site_duplicate(0);
    let copy = wait_for("the copy", || session.sites().get(1).cloned());
    assert_eq!(copy.name, "testzone copy");
    let secret = |id: &httporter_core::SiteId| {
        httporter_platform::CredentialStore::get(
            &*creds,
            &httporter_platform::CredentialKey::site(id),
        )
        .unwrap()
    };
    assert_eq!(secret(&copy.id).as_deref(), Some("pw"));

    // Remove… asks first; Cancel keeps the site.
    ui.invoke_site_remove(1);
    assert!(ui.get_remove_site_open());
    assert_eq!(ui.get_remove_site_title(), "Remove “testzone copy”?");
    assert_eq!(
        ui.get_remove_site_body(),
        "HTTPorter forgets this site and its saved password. Files on Bunny Storage aren't touched."
    );
    ui.set_remove_site_open(false);
    ui.invoke_remove_site_cancel();
    // Confirming after a cancel removes nothing: the alert's site was let go.
    ui.invoke_remove_site_confirm();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(session.sites().len(), 2);

    // Remove: the site and its password go; the other stays.
    ui.invoke_site_remove(1);
    ui.set_remove_site_open(false);
    ui.invoke_remove_site_confirm();
    wait_for("the removal", || (session.sites().len() == 1).then_some(()));
    assert_eq!(session.sites()[0].id, site.id);
    assert_eq!(secret(&copy.id), None);
    assert_eq!(secret(&site.id).as_deref(), Some("pw"));
}

#[test]
fn the_banner_offers_edit_and_remove_for_a_site_that_cannot_sign_in() {
    testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_sites(slint::ModelRc::new(slint::VecModel::from(vec![SiteItem {
        name: "testzone".into(),
        provider: ProviderTile::Bunny,
        ..SiteItem::default()
    }])));
    ui.set_selected_site(0);
    ui.set_error_text("No password saved for testzone. Edit the site to enter it again.".into());
    ui.set_remote_offer_edit(true);
    let told = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    ui.on_site_edit({
        let told = told.clone();
        move |i| told.borrow_mut().push(format!("edit {i}"))
    });
    ui.on_site_remove({
        let told = told.clone();
        move |i| told.borrow_mut().push(format!("remove {i}"))
    });
    ui.show().unwrap();
    let button = |label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label)
            .find(|e| e.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Button))
    };
    // Not before the second round of Add Site is wired.
    assert!(button("Edit Site…").is_none());
    ui.set_sites_v2_ready(true);
    button("Edit Site…")
        .unwrap()
        .invoke_accessible_default_action();
    button("Remove Site…")
        .unwrap()
        .invoke_accessible_default_action();
    assert_eq!(told.borrow().as_slice(), ["edit 0", "remove 0"]);
    // Only for an error that editing the site fixes.
    ui.set_remote_offer_edit(false);
    assert!(button("Edit Site…").is_none());
}
