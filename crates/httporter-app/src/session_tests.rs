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
    assert_eq!(view.status_right, "HTTP/2 · sg.storage.bunnycdn.com");
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
        Some("No password saved for testzone. Remove this site and add it again.")
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

#[tokio::test]
async fn a_site_that_cannot_be_reached_is_not_added() {
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    mock.fail_next(
        MockOp::List,
        ProviderError::Network("operation timed out".into()),
        1,
    );
    let refused = s
        .add_site(&valid("testzone", "sg", "pw"))
        .await
        .unwrap_err();
    assert_eq!(refused, UNREACHABLE);
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

/// Minor 2: a listing someone is waiting for gives up after 20 s and is tried at most three
/// times, for browsing, Test Connection and the test before a site is added alike. Transfers and
/// deletes keep the default policy. Ruling F4: the interactive policy also reports a long
/// `Retry-After` at once rather than waiting it out (pinned against Bunny in
/// `an_interactive_listing_does_not_wait_out_a_long_retry_after`).
#[tokio::test]
async fn listings_someone_waits_for_give_up_sooner() {
    let mock = site_mock();
    let (s, _, factory) = session_with(mock.clone());
    s.add_site(&valid("testzone", "sg", "pw")).await.unwrap();
    s.test_connection(&valid("testzone", "sg", "pw")).await;
    select(&s, 0).await;
    let configs = factory.full_configs();
    assert_eq!(configs.len(), 3, "added, tested, opened");
    let default = BunnyConfig::new("testzone", "sg.storage.bunnycdn.com", "pw");
    for cfg in configs {
        assert_eq!(cfg.list_timeout, Duration::from_secs(20));
        assert_eq!(cfg.list_retry, Some(RetryPolicy::interactive()));
        assert_eq!(cfg.retry, default.retry);
        assert_eq!(cfg.transfer_idle_timeout, default.transfer_idle_timeout);
        assert_eq!(cfg.transfer_concurrency, default.transfer_concurrency);
        assert_eq!(cfg.delete_concurrency, default.delete_concurrency);
    }
}
