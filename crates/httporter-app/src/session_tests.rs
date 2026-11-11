// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests of the session's sites: the tile a site gets, adding and saving one (the password in the
//! credential store, nothing secret in the database), Test Connection, and the plain words for
//! what went wrong. Browsing the panes is in `session_browse_tests.rs`, which shares the helpers
//! below.

use super::fakes::*;
use super::sites::TestResult;
use super::*;
use crate::add_site::ValidSite;
use crate::viewmodel::RowData;
use chrono::Utc;
use httporter_bunny::BunnyConfig;
use httporter_core::{
    AuthHint, MockOp, MockProvider, ProviderKind, RetryPolicy, S3Preset, SiteId, SiteRecord,
};
use httporter_platform::{CredentialKey, MemoryStore};
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;

pub(super) fn valid(zone: &str, region: &str, password: &str) -> ValidSite {
    ValidSite {
        name: zone.into(),
        zone: zone.into(),
        region_code: region.into(),
        password: password.into(),
    }
}

pub(super) fn session_with_mock(mock: MockProvider) -> (Session, Arc<MemoryStore>) {
    let (session, creds, _) = session_with(Arc::new(mock));
    (session, creds)
}

/// "index.html" and "wp-content/x.txt".
pub(super) fn site_mock() -> Arc<MockProvider> {
    Arc::new(
        MockProvider::new(Default::default())
            .with_file("index.html", b"<html></html>")
            .with_file("wp-content/x.txt", b"x"),
    )
}

/// A session with the site "testzone" (Singapore, password "pw") saved, browsing `mock`.
pub(super) async fn saved(mock: &Arc<MockProvider>) -> Session {
    let (s, _, _) = session_with(mock.clone());
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    s
}

pub(super) fn names(rows: &[RowData]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

pub(super) fn remote_entry_names(s: &Session) -> Vec<String> {
    lock(&s.remote)
        .open
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .map(|e| e.name().to_owned())
        .collect()
}

pub(super) fn local_entry_names(s: &Session) -> Vec<String> {
    lock(&s.local)
        .entries
        .iter()
        .map(|e| e.name.clone())
        .collect()
}

/// What the cloud pane shows now.
pub(super) fn current_remote(s: &Session) -> RemoteView {
    s.remote_view_of(&lock(&s.remote))
}

/// Sends the This PC pane to `dir` (`None`: the drives).
pub(super) async fn navigate_local(s: &Session, dir: Option<PathBuf>) -> LocalView {
    s.navigate_local(s.local_ticket(), Step::Go, move |_| Some(dir), |_| {})
        .await
}

// The window's way of asking: a ticket taken at the moment of asking and, for rows and crumbs,
// the version of the view on screen (here, the pane as it is).

pub(super) async fn select(s: &Session, index: usize) -> RemoteView {
    s.select_site(s.remote_ticket(), index, |_| {}).await
}

pub(super) async fn open_remote(s: &Session, row: usize) -> RemoteView {
    let seen = lock(&s.remote).version;
    s.open_remote(s.remote_ticket(), row, seen, |_| {}).await
}

pub(super) async fn remote_up(s: &Session) -> RemoteView {
    s.remote_up(s.remote_ticket(), |_| {}).await
}

pub(super) async fn remote_crumb(s: &Session, index: usize) -> RemoteView {
    let seen = lock(&s.remote).version;
    s.remote_crumb(s.remote_ticket(), index, seen, |_| {}).await
}

pub(super) async fn remote_back(s: &Session) -> RemoteView {
    s.remote_back(s.remote_ticket(), |_| {}).await
}

pub(super) async fn remote_forward(s: &Session) -> RemoteView {
    s.remote_forward(s.remote_ticket(), |_| {}).await
}

pub(super) async fn remote_refresh(s: &Session) -> RemoteView {
    s.remote_refresh(s.remote_ticket(), |_| {}).await
}

pub(super) async fn open_local(s: &Session, row: usize) -> LocalView {
    let seen = lock(&s.local).version;
    s.open_local(s.local_ticket(), row, seen, |_| {}).await
}

pub(super) async fn local_up(s: &Session) -> LocalView {
    s.local_up(s.local_ticket(), |_| {}).await
}

pub(super) async fn local_crumb(s: &Session, index: usize) -> LocalView {
    let seen = lock(&s.local).version;
    s.local_crumb(s.local_ticket(), index, seen, |_| {}).await
}

pub(super) async fn local_back(s: &Session) -> LocalView {
    s.local_back(s.local_ticket(), |_| {}).await
}

pub(super) async fn local_forward(s: &Session) -> LocalView {
    s.local_forward(s.local_ticket(), |_| {}).await
}

pub(super) async fn local_refresh(s: &Session) -> LocalView {
    s.local_refresh(s.local_ticket(), |_| {}).await
}

/// `view` without its version, to compare what two views show.
pub(super) fn shows(view: &LocalView) -> LocalView {
    LocalView {
        version: 0,
        ..view.clone()
    }
}

/// How long anything that must not wait for a held-back listing may take: far longer than it
/// needs, far shorter than forever.
pub(super) const PROMPTLY: Duration = Duration::from_secs(5);

/// Waits until `gated` holds a listing back.
pub(super) async fn until_held(gated: &Gated) {
    timeout(PROMPTLY, async {
        while gated.waiting() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the listing never reached the storage");
}

pub(super) const AUTH: &str =
    "Couldn't sign in to testzone. Check the password, zone name and region.";
pub(super) const UNREACHABLE: &str =
    "Couldn't reach sg.storage.bunnycdn.com. Check your internet connection and try again.";

/// The credentials were turned down, for no reason given (as Bunny always does).
pub(super) fn rejected() -> ProviderError {
    ProviderError::Auth {
        hint: AuthHint::Unknown,
    }
}

// ---- The brief's tests ----

/// Ledger 129: a site's tile comes from its provider, whatever the site is called.
#[test]
fn a_sites_tile_comes_from_its_provider_not_its_name() {
    let bunny = SiteRecord {
        id: SiteId::new(),
        name: "R2 mirror".into(),
        provider: ProviderKind::Bunny,
        container: "mirror".into(),
        region: "sg".into(),
        host: "sg.storage.bunnycdn.com".into(),
        s3: None,
        created_at: Utc::now(),
    };
    assert_eq!(SiteProvider::of(&bunny), SiteProvider::Bunny);
    assert_eq!(SiteProvider::of(&bunny).mono(), "B");

    let wasabi = SiteRecord {
        name: "B".into(),
        provider: ProviderKind::S3,
        s3: Some(httporter_core::S3Site {
            preset: S3Preset::Wasabi,
            endpoint: "https://s3.wasabisys.com".into(),
            path_style: false,
        }),
        ..bunny.clone()
    };
    assert_eq!(
        SiteProvider::of(&wasabi),
        SiteProvider::S3(S3Preset::Wasabi)
    );
    assert_eq!(SiteProvider::of(&wasabi).mono(), "W");

    let no_details = SiteRecord { s3: None, ..wasabi };
    assert_eq!(
        SiteProvider::of(&no_details),
        SiteProvider::S3(S3Preset::Other)
    );
}

#[tokio::test]
async fn save_site_keeps_password_out_of_the_database() {
    let (s, creds) = session_with_mock(MockProvider::new(Default::default()));
    let rec = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    assert_eq!(
        creds.get(&CredentialKey::site(&rec.id)).unwrap().as_deref(),
        Some("pw")
    );
    assert_eq!(s.sites()[0].container, "testzone");
    assert_eq!(s.sites()[0].host, "sg.storage.bunnycdn.com");
}

#[tokio::test]
async fn select_site_lists_root() {
    let mock = site_mock();
    let s = saved(&mock).await;
    let view = select(&s, 0).await;
    assert_eq!(names(&view.rows), ["wp-content", "index.html"]);
    assert_eq!(view.crumbs, ["testzone"]);
    assert_eq!(view.connection, "HTTPS · 0 ms");
    // Task 23: the site's connections in the engine are counted too (32 at the defaults).
    assert_eq!(
        view.status_right,
        "HTTP/2 · sg.storage.bunnycdn.com · 32 connections ready"
    );
    assert_eq!(view.endpoint, "Bunny Storage · testzone");
    assert_eq!(view.chip, "https://sg.storage.bunnycdn.com");
    assert_eq!(view.error, None);
}

#[tokio::test]
async fn auth_error_shows_exact_message() {
    let mock = site_mock();
    let s = saved(&mock).await;
    mock.fail_next(MockOp::List, rejected(), 1);
    let view = select(&s, 0).await;
    assert_eq!(view.error.as_deref(), Some(AUTH));
    assert!(view.rows.is_empty());
    assert_eq!(view.crumbs, ["testzone"]);
    assert_eq!(view.connection, "Not connected");
    assert_eq!(view.status_right, "");
    assert_eq!(mock.calls(MockOp::List), 1);
}

#[tokio::test]
async fn navigate_into_folder_and_back() {
    let mock = site_mock();
    let s = saved(&mock).await;
    select(&s, 0).await;
    let view = open_remote(&s, 0).await;
    assert_eq!(view.crumbs, ["testzone", "wp-content"]);
    assert_eq!(names(&view.rows), ["x.txt"]);
    let view = remote_up(&s).await;
    assert_eq!(view.crumbs, ["testzone"]);
    assert_eq!(names(&view.rows), ["wp-content", "index.html"]);
}

#[tokio::test]
async fn failed_site_turns_unhealthy() {
    let mock = site_mock();
    let s = saved(&mock).await;
    mock.fail_next(MockOp::List, rejected(), 1);
    select(&s, 0).await;
    let item = &s.site_items()[0];
    assert!(!item.healthy);
    assert_eq!(item.subtitle, "Bunny · Singapore");
    assert_eq!(item.provider, SiteProvider::Bunny);
    assert_eq!(item.provider.mono(), "B");
    assert_eq!(item.name, "testzone");
    assert_eq!(item.id, s.sites()[0].id.0);

    // The next request that works makes it healthy again.
    select(&s, 0).await;
    assert!(s.site_items()[0].healthy);
}

#[tokio::test]
async fn test_connection_text() {
    let (s, _) = session_with_mock(
        MockProvider::new(Default::default())
            .with_file("index.html", b"x")
            .with_file("wp-content/x.txt", b"x"),
    );
    let r = s.test_connection(&valid("testzone", "sg", "pw")).await;
    assert!(r.ok);
    assert_eq!(
        r.text,
        "Connected over HTTPS · HTTP/2 · 0 ms · 2 items in /"
    );
}

// ---- Sites and passwords ----

#[tokio::test]
async fn a_site_counts_as_healthy_until_a_request_fails() {
    let mock = site_mock();
    let s = saved(&mock).await;
    assert!(s.site_items()[0].healthy);
}

#[tokio::test]
async fn test_connection_reports_a_rejected_password() {
    let mock = site_mock();
    let (s, _, factory) = session_with(mock.clone());
    mock.fail_next(MockOp::List, rejected(), 1);
    let r = s.test_connection(&valid("testzone", "sg", "pw")).await;
    assert_eq!(
        r,
        TestResult {
            ok: false,
            text: AUTH.into()
        }
    );
    // The test used exactly what was typed.
    assert_eq!(
        factory.configs(),
        [(
            "testzone".to_owned(),
            "sg.storage.bunnycdn.com".to_owned(),
            "pw".to_owned()
        )]
    );
}

#[tokio::test]
async fn the_site_password_comes_from_the_credential_store() {
    let mock = site_mock();
    let (s, creds, factory) = session_with(mock.clone());
    let rec = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    creds.set(&CredentialKey::site(&rec.id), "pw-2").unwrap();
    select(&s, 0).await;
    assert_eq!(
        factory.configs(),
        [(
            "testzone".to_owned(),
            "sg.storage.bunnycdn.com".to_owned(),
            "pw-2".to_owned()
        )]
    );
}

#[tokio::test]
async fn a_missing_password_is_reported_without_a_request() {
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    let rec = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    creds.delete(&CredentialKey::site(&rec.id)).unwrap();
    let view = select(&s, 0).await;
    assert_eq!(
        view.error.as_deref(),
        Some("No password saved for testzone. Edit the site to enter it again.")
    );
    assert!(view.rows.is_empty());
    assert!(!view.loading, "nothing is on its way");
    assert_eq!(mock.calls(MockOp::List), 0);
}

#[tokio::test]
async fn sites_are_listed_by_name_with_their_region() {
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    s.save_site(&valid("zeta", "de", "pw")).await.unwrap();
    let alpha = s.save_site(&valid("alpha", "sg", "pw")).await.unwrap();
    let items = s.site_items();
    let shown: Vec<(&str, &str)> = items
        .iter()
        .map(|i| (i.name.as_str(), i.subtitle.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("alpha", "Bunny · Singapore"),
            ("zeta", "Bunny · Frankfurt")
        ]
    );
    assert_eq!(s.sites()[0].id, alpha.id);
    assert_eq!(alpha.region, "sg");
}

#[tokio::test]
async fn an_unknown_region_is_not_saved() {
    let (s, creds) = session_with_mock(MockProvider::new(Default::default()));
    assert!(s.save_site(&valid("z", "xx", "pw")).await.is_err());
    assert!(s.sites().is_empty());
    assert_eq!(format!("{creds:?}"), "MemoryStore { entries: 0, .. }");
}

#[test]
fn the_sheet_endpoint_follows_zone_and_region() {
    assert_eq!(
        endpoint_preview("testzone", 4),
        "https://sg.storage.bunnycdn.com/testzone/"
    );
    assert_eq!(
        endpoint_preview(" z ", 0),
        "https://storage.bunnycdn.com/z/"
    );
    assert_eq!(endpoint_preview("", 0), "https://storage.bunnycdn.com/");
    assert_eq!(endpoint_preview("z", -1), "");
    assert_eq!(endpoint_preview("z", 99), "");
}

// ---- Slow calls stay off the runtime ----

/// Ledger 248 and 252: the credential store (Windows Credential Manager), the site database (the
/// disk) and building a client (TLS set-up) are slow, so none of them is called on the thread
/// that runs the tasks. A current-thread runtime has only that one, so any thread the calls were
/// made on that is not this one is a blocking thread.
#[tokio::test]
async fn credentials_the_database_and_clients_stay_off_the_runtime_thread() {
    let here = std::thread::current().id();
    let creds = Arc::new(SpyStore::default());
    let factory = FakeFactory::new(site_mock());
    let s = Session::new(
        Store::open_in_memory().unwrap(),
        creds.clone(),
        factory.clone(),
        fixed_now,
    );

    // A client is made to test the zone; the password and the site are then stored (one call for
    // each, from the same blocking task as the database write) …
    assert!(s.test_connection(&valid("testzone", "sg", "pw")).await.ok);
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    // … and opening the site reads the password and makes another client.
    select(&s, 0).await;

    for (what, threads) in [
        ("the credential store", creds.threads()),
        ("the client factory", factory.threads()),
    ] {
        assert_eq!(threads.len(), 2, "{what} was called twice");
        assert!(
            threads.iter().all(|thread| *thread != here),
            "{what} was called on the runtime thread"
        );
    }
}

// ---- A save that has started is finished ----

/// Task 16 review: Cancel gives up the task that is saving a site, but not the write. Once the
/// password is on its way to the credential store the site is written, so it is also listed, once.
/// (Before the task was split the save had no await in it, so nothing could stop it half done.)
#[tokio::test]
async fn a_save_that_has_started_lists_the_site_even_if_it_is_given_up() {
    let creds = Arc::new(SpyStore::default());
    let factory = FakeFactory::new(site_mock());
    let s = Arc::new(Session::new(
        Store::open_in_memory().unwrap(),
        creds.clone(),
        factory,
        fixed_now,
    ));
    let hold = creds.hold_set();
    let saving = tokio::spawn({
        let s = s.clone();
        async move { s.save_site(&valid("testzone", "sg", "pw")).await }
    });
    // The password is being written, on a blocking thread, and the write is slow.
    hold.entered().await;
    assert!(
        s.sites().is_empty(),
        "nothing is listed before the write is done"
    );

    // Cancel: the task that waits for the save is dropped at its await.
    saving.abort();
    assert!(saving.await.unwrap_err().is_cancelled());
    hold.release();

    timeout(PROMPTLY, async {
        while s.sites().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the site was written but never listed");
    let sites = s.sites();
    assert_eq!(sites.len(), 1, "listed once");
    assert_eq!(s.site_items().len(), 1, "and so in the sidebar's rows");
    assert_eq!(
        lock(&s.store).list_sites().unwrap().sites,
        sites,
        "as in the database"
    );
    assert_eq!(
        creds
            .get(&CredentialKey::site(&sites[0].id))
            .unwrap()
            .as_deref(),
        Some("pw")
    );
}

// ---- Adding a site (final fix wave) ----

/// Important #1: Add Site lists the zone first (as Test Connection does) and saves nothing when
/// the password is rejected.
#[tokio::test]
async fn a_site_whose_password_is_rejected_is_not_added() {
    let mock = site_mock();
    let (s, creds, factory) = session_with(mock.clone());
    mock.fail_next(MockOp::List, rejected(), 1);
    let refused = s
        .add_site(&valid("testzone", "sg", "pw"))
        .await
        .unwrap_err();
    assert_eq!(refused, AUTH);
    assert!(s.sites().is_empty());
    assert!(s.site_items().is_empty());
    assert_eq!(format!("{creds:?}"), "MemoryStore { entries: 0, .. }");
    assert_eq!(mock.calls(MockOp::List), 1);
    // The test used exactly what was typed.
    assert_eq!(
        factory.configs(),
        [(
            "testzone".to_owned(),
            "sg.storage.bunnycdn.com".to_owned(),
            "pw".to_owned()
        )]
    );
}

/// The test before a site is added retries as browsing does (Task 23): a server that stays
/// unreachable through the three attempts is not added. The clock is paused, so the waits
/// between the attempts (1 s, then 2 s) pass at once.
#[tokio::test(start_paused = true)]
async fn a_site_that_cannot_be_reached_is_not_added() {
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    mock.fail_next(
        MockOp::List,
        ProviderError::Network("operation timed out".into()),
        3,
    );
    let refused = s
        .add_site(&valid("testzone", "sg", "pw"))
        .await
        .unwrap_err();
    assert_eq!(refused, UNREACHABLE);
    assert_eq!(mock.calls(MockOp::List), 3);
    assert!(s.sites().is_empty());
    assert_eq!(format!("{creds:?}"), "MemoryStore { entries: 0, .. }");
}

#[tokio::test]
async fn a_site_that_answers_is_added_once() {
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    let added = s.add_site(&valid("testzone", "sg", "pw")).await.unwrap();
    assert_eq!(s.sites(), std::slice::from_ref(&added));
    assert_eq!(mock.calls(MockOp::List), 1);
    assert_eq!(
        creds
            .get(&CredentialKey::site(&added.id))
            .unwrap()
            .as_deref(),
        Some("pw")
    );
    assert_eq!(format!("{creds:?}"), "MemoryStore { entries: 1, .. }");
}

#[tokio::test]
async fn adding_a_site_with_an_unknown_region_tries_nothing() {
    let mock = site_mock();
    let (s, _, factory) = session_with(mock.clone());
    let refused = s
        .add_site(&valid("testzone", "xx", "pw"))
        .await
        .unwrap_err();
    assert_eq!(refused, "Choose a region.");
    assert!(s.sites().is_empty());
    assert!(factory.configs().is_empty());
}

/// Minor 2, after Task 23: every client is made as the site's transfers use it
/// (`BunnyConfig::for_site`: one attempt per call, a 20 s listing timeout, the Transfers
/// settings), and a listing someone waits for (browsing, Test Connection, the test before a site
/// is added) retries a little itself with `list_interactive`: three attempts at most. The I1 hack
/// that put the interactive policy in the client's configuration is gone. The clock is paused, so
/// the waits between attempts pass at once.
#[tokio::test(start_paused = true)]
async fn listings_someone_waits_for_give_up_sooner() {
    let mock = site_mock();
    let (s, _, factory) = session_with(mock.clone());
    s.add_site(&valid("testzone", "sg", "pw")).await.unwrap();
    s.test_connection(&valid("testzone", "sg", "pw")).await;
    select(&s, 0).await;
    let configs = factory.full_configs();
    assert_eq!(configs.len(), 3, "added, tested, opened");
    let wanted = BunnyConfig::for_site(
        "testzone",
        "sg.storage.bunnycdn.com",
        "pw",
        &httporter_core::TransferSettings::default(),
    );
    for cfg in configs {
        assert_eq!(cfg.list_timeout, Duration::from_secs(20));
        assert_eq!(cfg.list_retry, None, "no retry policy inside the client");
        assert_eq!(cfg.retry, RetryPolicy::immediate(0), "one attempt per call");
        assert_eq!(cfg.transfer_idle_timeout, wanted.transfer_idle_timeout);
        assert_eq!(cfg.transfer_concurrency, wanted.transfer_concurrency);
        assert_eq!(cfg.delete_concurrency, wanted.delete_concurrency);
    }

    // Browsing: two passing failures are retried away; the third attempt is the last.
    let network = || ProviderError::Network("reset".into());
    let before = mock.calls(MockOp::List);
    mock.fail_next(MockOp::List, network(), 2);
    assert_eq!(remote_refresh(&s).await.error, None);
    assert_eq!(mock.calls(MockOp::List) - before, 3);
    mock.fail_next(MockOp::List, network(), 3);
    assert_eq!(remote_refresh(&s).await.error.as_deref(), Some(UNREACHABLE));
    assert_eq!(mock.calls(MockOp::List) - before, 6);

    // Test Connection alike.
    mock.fail_next(MockOp::List, network(), 2);
    assert!(s.test_connection(&valid("testzone", "sg", "pw")).await.ok);
    mock.fail_next(MockOp::List, network(), 3);
    let tested = s.test_connection(&valid("testzone", "sg", "pw")).await;
    assert_eq!(tested.text, UNREACHABLE);
    assert_eq!(mock.calls(MockOp::List) - before, 12);
}

// ---- Task 24: editing, duplicating and removing sites; S3; the account key; Import ----

use crate::add_site::SiteForm;
use httporter_bunny::{ZoneInfo, ZoneList};
use httporter_platform::{SiteSecret, site_conflicts_key};

/// The password saved for `id`, as the credential store has it.
fn saved_secret(creds: &MemoryStore, id: &SiteId) -> Option<String> {
    creds.get(&CredentialKey::site(id)).unwrap()
}

#[tokio::test]
async fn edit_changes_name_and_password_and_rebuilds_client() {
    let mock = site_mock();
    let (s, creds, factory) = session_with(mock.clone());
    let old = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    select(&s, 0).await;
    assert_eq!(factory.configs().len(), 1);
    let client = s.remote_place().unwrap().provider;

    let edited = s
        .save_form(
            SiteForm::bunny("testzone", 4, "pw-2"),
            Some(old.id.clone()),
            "Shop".into(),
        )
        .await
        .unwrap();
    // The same site: its id and creation time stay.
    assert_eq!((&edited.id, edited.created_at), (&old.id, old.created_at));
    assert_eq!(edited.name, "Shop");
    assert_eq!(s.sites(), std::slice::from_ref(&edited));
    assert_eq!(s.site_items()[0].name, "Shop");
    assert_eq!(saved_secret(&creds, &old.id).as_deref(), Some("pw-2"));
    // The new password was tested before it was saved …
    assert_eq!(factory.configs()[1].2, "pw-2");
    // … and the site's client is built again with it.
    assert!(!s.hub().is_current(&old.id, &client));
    select(&s, 0).await;
    let configs = factory.configs();
    assert_eq!(configs.len(), 3);
    assert_eq!(configs[2].2, "pw-2");

    // A password the zone refuses changes nothing.
    mock.fail_next(MockOp::List, rejected(), 1);
    let refused = s
        .save_form(
            SiteForm::bunny("testzone", 4, "bad"),
            Some(old.id.clone()),
            "Other".into(),
        )
        .await
        .unwrap_err();
    assert_eq!(refused, AUTH);
    assert_eq!(s.sites()[0].name, "Shop");
    assert_eq!(saved_secret(&creds, &old.id).as_deref(), Some("pw-2"));
}

#[tokio::test]
async fn edit_with_empty_secret_keeps_it() {
    let mock = site_mock();
    let (s, creds, factory) = session_with(mock.clone());
    let old = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    let edited = s
        .save_form(
            SiteForm::bunny("testzone", 4, ""),
            Some(old.id.clone()),
            "Renamed".into(),
        )
        .await
        .unwrap();
    assert_eq!(edited.name, "Renamed");
    assert_eq!(saved_secret(&creds, &old.id).as_deref(), Some("pw"));
    // The test before the save signed in with the saved password.
    assert_eq!(factory.configs().last().unwrap().2, "pw");

    // A site removed meanwhile has no secret to keep.
    s.remove_site(&old.id).unwrap();
    let gone = s
        .save_form(
            SiteForm::bunny("testzone", 4, ""),
            Some(old.id),
            String::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(gone, "That site is no longer saved.");
}

#[tokio::test]
async fn duplicate_copies_secret_under_new_id() {
    let (s, creds) = session_with_mock(MockProvider::new(Default::default()));
    let old = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    let copy = s.duplicate_site(&old.id).unwrap();
    assert_ne!(copy.id, old.id);
    assert_eq!(copy.name, "testzone copy");
    assert_eq!(
        (&copy.container, &copy.region, &copy.host, copy.provider),
        (&old.container, &old.region, &old.host, old.provider)
    );
    assert_eq!(saved_secret(&creds, &copy.id).as_deref(), Some("pw"));
    assert_eq!(saved_secret(&creds, &old.id).as_deref(), Some("pw"));
    let names: Vec<String> = s.sites().into_iter().map(|site| site.name).collect();
    assert_eq!(names, ["testzone", "testzone copy"]);
    // A site that is gone can't be copied.
    assert_eq!(
        s.duplicate_site(&SiteId::new()).unwrap_err(),
        "That site is no longer saved."
    );
}

/// Ruling Q23: Remove forgets the site's secret, its row and its remembered conflict choices,
/// and drops its client; the files on the storage are never touched. (Its transfers are
/// cancelled by the window first: `transfers_tests::removing_a_site_cancels_its_transfers`.)
#[tokio::test]
async fn remove_deletes_row_and_credential_and_cancels_jobs() {
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    let gone = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    let kept = s.save_site(&valid("zeta", "de", "pw-z")).await.unwrap();
    select(&s, 0).await;
    let client = s.remote_place().unwrap().provider;
    for upload in [true, false] {
        s.remember_conflicts(&gone.id, upload, httporter_core::ConflictPolicy::Skip)
            .await
            .unwrap();
        s.remember_conflicts(&kept.id, upload, httporter_core::ConflictPolicy::Replace)
            .await
            .unwrap();
    }

    s.remove_site(&gone.id).unwrap();
    assert_eq!(s.sites(), std::slice::from_ref(&kept));
    assert_eq!(
        lock(&s.store).list_sites().unwrap().sites,
        std::slice::from_ref(&kept)
    );
    assert_eq!(saved_secret(&creds, &gone.id), None);
    assert_eq!(saved_secret(&creds, &kept.id).as_deref(), Some("pw-z"));
    assert!(!s.hub().is_current(&gone.id, &client));
    for upload in [true, false] {
        let key = site_conflicts_key(&gone.id, upload);
        let left: Option<httporter_core::ConflictPolicy> = lock(&s.store).settings(&key);
        assert_eq!(left, None, "{key}");
        assert_eq!(
            s.conflict_policy(&kept.id, upload).await,
            httporter_core::ConflictPolicy::Replace
        );
    }
    // Nothing was deleted on the storage.
    assert_eq!(mock.calls(MockOp::DeleteFile), 0);
    assert_eq!(mock.calls(MockOp::DeleteFolder), 0);
    assert!(
        mock.contents(&httporter_core::RemotePath::parse("index.html").unwrap())
            .is_some()
    );
}

/// An Amazon S3 form for bucket `media` in `eu-west-1`.
fn aws_form(secret: &str) -> SiteForm {
    aws_form_with("eu-west-1", "AKIDEXAMPLE", secret)
}

/// An Amazon S3 form for bucket `media`.
fn aws_form_with(region: &str, key_id: &str, secret: &str) -> SiteForm {
    SiteForm::S3 {
        preset: S3Preset::AwsS3,
        region: region.into(),
        account_id: String::new(),
        endpoint: String::new(),
        path_style: false,
        bucket: "media".into(),
        key_id: key_id.into(),
        secret: secret.into(),
    }
}

#[tokio::test]
async fn s3_site_saved_after_successful_test_with_secret_json() {
    let mock = site_mock();
    let (s, creds, factory) = session_with(mock.clone());
    let site = s
        .save_form(aws_form("sk-1"), None, String::new())
        .await
        .unwrap();
    assert_eq!(mock.calls(MockOp::List), 1, "tested first");
    assert_eq!(site.name, "media");
    assert_eq!(site.provider, ProviderKind::S3);
    assert_eq!(
        (
            site.container.as_str(),
            site.region.as_str(),
            site.host.as_str()
        ),
        ("media", "eu-west-1", "s3.eu-west-1.amazonaws.com")
    );
    assert_eq!(
        site.s3,
        Some(httporter_core::S3Site {
            preset: S3Preset::AwsS3,
            endpoint: "https://s3.eu-west-1.amazonaws.com".into(),
            path_style: false,
        })
    );
    // The key pair is one JSON value in the credential store, nowhere else.
    let stored = saved_secret(&creds, &site.id).unwrap();
    assert_eq!(
        stored,
        r#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"sk-1"}"#
    );
    assert_eq!(
        SiteSecret::from_stored(ProviderKind::S3, &stored).unwrap(),
        SiteSecret::S3 {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "sk-1".into(),
        }
    );
    assert_eq!(
        s.site_items()[0].subtitle,
        "Amazon S3 · eu-west-1",
        "the sidebar names the service and region"
    );

    // Opening it builds the S3 client with the saved key, the address and the service's part size.
    let view = select(&s, 0).await;
    assert_eq!(view.error, None);
    let cfg = factory.s3_configs().last().cloned().unwrap();
    assert_eq!(cfg.bucket, "media");
    assert_eq!(
        (
            cfg.creds.access_key_id.as_str(),
            cfg.creds.secret_access_key.as_str()
        ),
        ("AKIDEXAMPLE", "sk-1")
    );
    assert_eq!(
        cfg.endpoint.base.as_str(),
        "https://s3.eu-west-1.amazonaws.com/"
    );
    assert_eq!(cfg.part_size, httporter_s3::part_size_for(S3Preset::AwsS3));
    assert_eq!(s.saved_key_id(&site), "AKIDEXAMPLE");

    // Editing with the secret left empty keeps it; a new access key ID is taken.
    let form = aws_form_with("eu-west-1", "AKIDNEW", "");
    s.save_form(form, Some(site.id.clone()), String::new())
        .await
        .unwrap();
    assert_eq!(
        SiteSecret::from_stored(ProviderKind::S3, &saved_secret(&creds, &site.id).unwrap())
            .unwrap(),
        SiteSecret::S3 {
            access_key_id: "AKIDNEW".into(),
            secret_access_key: "sk-1".into(),
        }
    );

    // A failed test saves nothing.
    mock.fail_next(MockOp::List, rejected(), 1);
    let refused = s
        .save_form(aws_form("sk-2"), None, "Second".into())
        .await
        .unwrap_err();
    assert_eq!(
        refused,
        "Couldn't sign in to media. Check the access key ID, the secret access key and the bucket."
    );
    assert_eq!(s.sites().len(), 1);
}

#[tokio::test]
async fn s3_auth_errors_name_the_field() {
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    for (error, said) in [
        (
            ProviderError::Auth {
                hint: AuthHint::AccessKey,
            },
            "The access key ID wasn't recognized.",
        ),
        (
            ProviderError::Auth {
                hint: AuthHint::Secret,
            },
            "The secret access key doesn't match the access key ID.",
        ),
        (
            ProviderError::Auth {
                hint: AuthHint::Region,
            },
            "The region doesn't match the bucket. Check the region.",
        ),
        (
            ProviderError::ClockSkew,
            "Your PC's clock is off. Turn on “Set time automatically” in Windows Settings.",
        ),
    ] {
        mock.fail_next(MockOp::List, error, 1);
        assert_eq!(
            s.save_form(aws_form("sk"), None, String::new())
                .await
                .unwrap_err(),
            said
        );
    }
    assert!(s.sites().is_empty());
    assert_eq!(format!("{creds:?}"), "MemoryStore { entries: 0, .. }");
    // The words of the first three name the field to blame; the clock is nobody's field.
    use crate::add_site::{field, field_blamed};
    assert_eq!(
        field_blamed(crate::copy::S3_ACCESS_KEY_UNKNOWN),
        Some(field::KEY_ID)
    );
    assert_eq!(
        field_blamed(crate::copy::S3_SECRET_WRONG),
        Some(field::SECRET)
    );
    assert_eq!(
        field_blamed(crate::copy::S3_REGION_WRONG),
        Some(field::REGION)
    );
    assert_eq!(field_blamed(crate::copy::CLOCK_OFF), None);
}

#[tokio::test]
async fn choose_lists_the_buckets_the_key_can_see() {
    let (s, _, factory) = session_with(site_mock());
    factory.script_buckets(Ok(vec!["zeta".into(), "Alpha".into(), "media".into()]));
    let names = s.list_buckets(aws_form("sk"), None).await.unwrap();
    assert_eq!(names, ["Alpha", "media", "zeta"]);
    let cfg = factory.s3_configs().last().cloned().unwrap();
    assert_eq!(cfg.bucket, "", "the service root");
    assert_eq!(cfg.creds.secret_access_key, "sk");
    // A key that may not list buckets: type the name instead.
    factory.script_buckets(Err(ProviderError::Auth {
        hint: AuthHint::Unknown,
    }));
    assert_eq!(
        s.list_buckets(aws_form("sk"), None).await.unwrap_err(),
        "This key can't list buckets. Type the bucket name instead."
    );
    // A form whose address can't be made says why.
    let no_region = aws_form_with("", "AKIDEXAMPLE", "sk");
    assert_eq!(
        s.list_buckets(no_region, None).await.unwrap_err(),
        "Choose a region."
    );
}

fn zone_info(name: &str, region: &str, password: &str) -> ZoneInfo {
    ZoneInfo {
        id: 1,
        name: name.into(),
        region: region.into(),
        host: httporter_bunny::storage_host(region).unwrap(),
        password: password.into(),
        read_only_password: "ro".into(),
        pull_zones: Vec::new(),
    }
}

#[tokio::test]
async fn account_key_adds_ticked_zones_and_forgets_key_by_default() {
    let (s, creds, factory) = session_with(site_mock());
    let list = ZoneList {
        zones: vec![
            zone_info("exampleshop", "sg", "pw-a"),
            zone_info("archive", "de", "pw-b"),
        ],
        unusable: vec![httporter_bunny::UnusableZone {
            name: "broken".into(),
            reason: "no password".into(),
        }],
    };
    factory.script_zones(Ok(list.clone()));
    let found = s.find_zones("key-1".into()).await.unwrap();
    assert_eq!(found, list);
    assert_eq!(factory.zone_keys(), ["key-1"]);

    // Only the zone ticked is added, with its own password; the key is not kept.
    let added = s
        .add_zones(vec![found.zones[0].clone()], None)
        .await
        .unwrap();
    assert_eq!(added.len(), 1);
    assert_eq!(
        (
            added[0].name.as_str(),
            added[0].container.as_str(),
            added[0].region.as_str(),
            added[0].host.as_str()
        ),
        (
            "exampleshop",
            "exampleshop",
            "sg",
            "sg.storage.bunnycdn.com"
        )
    );
    assert_eq!(saved_secret(&creds, &added[0].id).as_deref(), Some("pw-a"));
    assert_eq!(format!("{creds:?}"), "MemoryStore { entries: 1, .. }");
    assert!(lock(&s.store).list_accounts().unwrap().is_empty());
    assert_eq!(s.site_items()[0].subtitle, "Bunny · Singapore");

    // An account that refuses the key, and one with nothing to add, say so.
    factory.script_zones(Err(ProviderError::Auth {
        hint: AuthHint::Unknown,
    }));
    assert_eq!(
        s.find_zones("bad".into()).await.unwrap_err(),
        "Bunny didn't accept that account key. Copy it again from the Bunny dashboard → \
         Account settings → API key."
    );
    assert_eq!(
        crate::copy::NO_ZONES,
        "This account has no storage zones that can be added."
    );
}

#[tokio::test]
async fn keep_key_stores_account_credential() {
    let (s, creds, _) = session_with(site_mock());
    let added = s
        .add_zones(
            vec![zone_info("a", "sg", "pw-a"), zone_info("b", "de", "pw-b")],
            Some(" key-1 ".into()),
        )
        .await
        .unwrap();
    assert_eq!(added.len(), 2);
    let accounts = lock(&s.store).list_accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "Bunny account");
    assert_eq!(accounts[0].provider, ProviderKind::Bunny);
    assert_eq!(
        creds
            .get(&CredentialKey::account(&accounts[0].id))
            .unwrap()
            .as_deref(),
        Some("key-1")
    );
    // Two zone passwords and the key.
    assert_eq!(format!("{creds:?}"), "MemoryStore { entries: 3, .. }");
}

#[tokio::test]
async fn import_env_prefills_bunny() {
    let (s, creds, factory) = session_with(site_mock());
    let import =
        crate::import::parse_env("BUNNY_ZONE=exampleshop\nBUNNY_KEY=pw-env\nBUNNY_REGION=sg\n")
            .unwrap();
    let draft = crate::import::env_draft(&import);
    assert_eq!(
        (draft.provider, draft.zone.as_str(), draft.region_index),
        (0, "exampleshop", 4)
    );
    // The sheet reports the fields with the password left empty: the imported one is used, and
    // tested before the site is saved.
    let form = SiteForm::bunny(&draft.zone, draft.region_index, "").or_secret(&import.password);
    let site = s.save_form(form, None, String::new()).await.unwrap();
    assert_eq!(factory.configs()[0].2, "pw-env");
    assert_eq!(
        (site.container.as_str(), site.region.as_str()),
        ("exampleshop", "sg")
    );
    assert_eq!(saved_secret(&creds, &site.id).as_deref(), Some("pw-env"));
}

#[tokio::test]
async fn aws_profile_prefills_s3() {
    let (s, creds, factory) = session_with(site_mock());
    let profiles = crate::import::parse_aws_profiles(
        "[work]\naws_access_key_id=AKIDWORK\naws_secret_access_key=sk-work\n",
        "[profile work]\nregion=eu-west-1\n",
    );
    let draft = crate::import::aws_draft(&profiles[0]);
    assert_eq!(
        (draft.provider, draft.region.as_str(), draft.key_id.as_str()),
        (1, "eu-west-1", "AKIDWORK")
    );
    // The user types the bucket; the secret stays with the app.
    let typed = crate::SiteDraft {
        bucket: "media".into(),
        ..draft
    };
    let form = SiteForm::from_s3_draft(&typed, "")
        .unwrap()
        .or_secret(&profiles[0].secret);
    let site = s.save_form(form, None, String::new()).await.unwrap();
    assert_eq!(factory.s3_configs()[0].creds.secret_access_key, "sk-work");
    assert_eq!(
        (site.container.as_str(), site.region.as_str()),
        ("media", "eu-west-1")
    );
    assert_eq!(
        SiteSecret::from_stored(ProviderKind::S3, &saved_secret(&creds, &site.id).unwrap())
            .unwrap(),
        SiteSecret::S3 {
            access_key_id: "AKIDWORK".into(),
            secret_access_key: "sk-work".into(),
        }
    );
}

#[tokio::test]
async fn missing_password_message_offers_edit() {
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    let site = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    creds.delete(&CredentialKey::site(&site.id)).unwrap();
    let view = select(&s, 0).await;
    assert_eq!(
        view.error.as_deref(),
        Some("No password saved for testzone. Edit the site to enter it again.")
    );
    assert!(
        s.remote_offers_edit(),
        "Edit Site… and Remove Site… are offered"
    );

    // A sign-in that fails offers them too; a server that can't be reached does not.
    creds.set(&CredentialKey::site(&site.id), "pw").unwrap();
    s.hub().invalidate(&site.id);
    mock.fail_next(MockOp::List, rejected(), 1);
    select(&s, 0).await;
    assert!(s.remote_offers_edit());
    mock.fail_next(MockOp::List, ProviderError::NotFound, 1);
    let view = select(&s, 0).await;
    assert!(view.error.is_some());
    assert!(!s.remote_offers_edit());
    // Once it opens, nothing is offered.
    select(&s, 0).await;
    assert!(!s.remote_offers_edit());
}

#[tokio::test]
async fn unreadable_saved_sites_are_noted() {
    assert_eq!(
        crate::copy::sites_skipped(1),
        "1 saved site couldn't be read."
    );
    assert_eq!(
        crate::copy::sites_skipped(3),
        "3 saved sites couldn't be read."
    );
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    assert_eq!(s.notice(), None);
    lock(&s.book).unreadable = 2;
    assert_eq!(
        s.notice().as_deref(),
        Some("2 saved sites couldn't be read.")
    );
    s.set_notice("Couldn't open the site database.".into());
    assert_eq!(
        s.notice().as_deref(),
        Some("Couldn't open the site database.\n2 saved sites couldn't be read.")
    );
}
