//! Tests of the session controller, against a mock storage, an in-memory database and an
//! in-memory credential store.

use super::fakes::*;
use super::*;
use httporter_core::{MockOp, MockProvider, RetryPolicy, UploadOptions, UploadSource};
use httporter_platform::{CredentialKey, MemoryStore};
use std::time::Duration;
use tokio::time::timeout;

fn valid(zone: &str, region: &str, password: &str) -> ValidSite {
    ValidSite {
        name: zone.into(),
        zone: zone.into(),
        region_code: region.into(),
        password: password.into(),
    }
}

fn session_with_mock(mock: MockProvider) -> (Session, Arc<MemoryStore>) {
    let (session, creds, _) = session_with(Arc::new(mock));
    (session, creds)
}

/// "index.html" and "wp-content/x.txt".
fn site_mock() -> Arc<MockProvider> {
    Arc::new(
        MockProvider::new(Default::default())
            .with_file("index.html", b"<html></html>")
            .with_file("wp-content/x.txt", b"x"),
    )
}

/// A session with the site "testzone" (Singapore, password "pw") saved, browsing `mock`.
async fn saved(mock: &Arc<MockProvider>) -> Session {
    let (s, _, _) = session_with(mock.clone());
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    s
}

fn names(rows: &[RowData]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

fn remote_entry_names(s: &Session) -> Vec<String> {
    lock(&s.remote)
        .open
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .map(|e| e.name().to_owned())
        .collect()
}

fn local_entry_names(s: &Session) -> Vec<String> {
    lock(&s.local)
        .entries
        .iter()
        .map(|e| e.name.clone())
        .collect()
}

/// What the cloud pane shows now.
fn current_remote(s: &Session) -> RemoteView {
    s.remote_view_of(&lock(&s.remote))
}

/// Sends the This PC pane to `dir` (`None`: the drives).
async fn navigate_local(s: &Session, dir: Option<PathBuf>) -> LocalView {
    s.navigate_local(s.local_ticket(), Step::Go, move |_| Some(dir), |_| {})
        .await
}

// The window's way of asking: a ticket taken at the moment of asking and, for rows and crumbs,
// the version of the view on screen (here, the pane as it is).

async fn select(s: &Session, index: usize) -> RemoteView {
    s.select_site(s.remote_ticket(), index, |_| {}).await
}

async fn open_remote(s: &Session, row: usize) -> RemoteView {
    let seen = lock(&s.remote).version;
    s.open_remote(s.remote_ticket(), row, seen, |_| {}).await
}

async fn remote_up(s: &Session) -> RemoteView {
    s.remote_up(s.remote_ticket(), |_| {}).await
}

async fn remote_crumb(s: &Session, index: usize) -> RemoteView {
    let seen = lock(&s.remote).version;
    s.remote_crumb(s.remote_ticket(), index, seen, |_| {}).await
}

async fn remote_back(s: &Session) -> RemoteView {
    s.remote_back(s.remote_ticket(), |_| {}).await
}

async fn remote_forward(s: &Session) -> RemoteView {
    s.remote_forward(s.remote_ticket(), |_| {}).await
}

async fn remote_refresh(s: &Session) -> RemoteView {
    s.remote_refresh(s.remote_ticket(), |_| {}).await
}

async fn open_local(s: &Session, row: usize) -> LocalView {
    let seen = lock(&s.local).version;
    s.open_local(s.local_ticket(), row, seen, |_| {}).await
}

async fn local_up(s: &Session) -> LocalView {
    s.local_up(s.local_ticket(), |_| {}).await
}

async fn local_crumb(s: &Session, index: usize) -> LocalView {
    let seen = lock(&s.local).version;
    s.local_crumb(s.local_ticket(), index, seen, |_| {}).await
}

async fn local_back(s: &Session) -> LocalView {
    s.local_back(s.local_ticket(), |_| {}).await
}

async fn local_forward(s: &Session) -> LocalView {
    s.local_forward(s.local_ticket(), |_| {}).await
}

async fn local_refresh(s: &Session) -> LocalView {
    s.local_refresh(s.local_ticket(), |_| {}).await
}

/// `view` without its version, to compare what two views show.
fn shows(view: &LocalView) -> LocalView {
    LocalView {
        version: 0,
        ..view.clone()
    }
}

/// How long anything that must not wait for a held-back listing may take: far longer than it
/// needs, far shorter than forever.
const PROMPTLY: Duration = Duration::from_secs(5);

/// Waits until `gated` holds a listing back.
async fn until_held(gated: &Gated) {
    timeout(PROMPTLY, async {
        while gated.waiting() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the listing never reached the storage");
}

const AUTH: &str = "Couldn't sign in to testzone. Check the password, zone name and region.";
const UNREACHABLE: &str =
    "Couldn't reach sg.storage.bunnycdn.com. Check your internet connection and try again.";

// ---- The brief's tests ----

#[tokio::test]
async fn save_site_keeps_password_out_of_the_database() {
    let (s, creds) = session_with_mock(MockProvider::new(Default::default()));
    let rec = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    assert_eq!(
        creds.get(&CredentialKey::site(&rec.id)).unwrap().as_deref(),
        Some("pw")
    );
    assert_eq!(s.sites()[0].zone, "testzone");
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
    mock.fail_next(MockOp::List, ProviderError::Auth, 1);
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
    mock.fail_next(MockOp::List, ProviderError::Auth, 1);
    select(&s, 0).await;
    let item = &s.site_items()[0];
    assert!(!item.healthy);
    assert_eq!(item.subtitle, "Bunny · Singapore");
    assert_eq!(item.mono, "B");
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
    mock.fail_next(MockOp::List, ProviderError::Auth, 1);
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

#[test]
fn item_counts() {
    assert_eq!(items(0), "0 items");
    assert_eq!(items(1), "1 item");
    assert_eq!(items(282), "282 items");
    assert_eq!(items_status(3, 0), "3 items");
    assert_eq!(items_status(3, 2), "3 items · 2 couldn't be read");
}

// ---- Cloud browsing ----

/// Note for Task 15: a row index is an index into the entry list, whatever the sort.
#[tokio::test]
async fn remote_rows_stay_in_entry_order_after_a_sort_change() {
    let mock = Arc::new(
        MockProvider::new(Default::default())
            .with_file("small.txt", &[0; 10])
            .with_file("big.txt", &[0; 5000])
            .with_file("mid.txt", &[0; 300])
            .with_folder("alpha")
            .with_folder("zeta"),
    );
    let s = saved(&mock).await;
    let view = select(&s, 0).await;
    assert_eq!(names(&view.rows), remote_entry_names(&s));

    let view = s.sort_remote(SortKey::Size, false);
    assert_eq!(
        names(&view.rows),
        ["alpha", "zeta", "big.txt", "mid.txt", "small.txt"]
    );
    assert_eq!(names(&view.rows), remote_entry_names(&s));

    let view = s.sort_remote(SortKey::Name, false);
    assert_eq!(
        names(&view.rows),
        ["zeta", "alpha", "small.txt", "mid.txt", "big.txt"]
    );
    assert_eq!(names(&view.rows), remote_entry_names(&s));

    // Row 0 is "zeta" now, so that is the folder that opens.
    let view = open_remote(&s, 0).await;
    assert_eq!(view.crumbs, ["testzone", "zeta"]);
    // The sort holds for the next listing too.
    let view = remote_up(&s).await;
    assert_eq!(names(&view.rows)[0], "zeta");
    assert_eq!(names(&view.rows), remote_entry_names(&s));
}

#[tokio::test]
async fn opening_a_file_or_a_row_past_the_end_does_nothing() {
    let mock = site_mock();
    let s = saved(&mock).await;
    let listed = select(&s, 0).await;
    let calls = mock.calls(MockOp::List);
    assert_eq!(open_remote(&s, 1).await, listed);
    assert_eq!(open_remote(&s, 7).await, listed);
    assert_eq!(remote_up(&s).await, listed);
    assert_eq!(mock.calls(MockOp::List), calls);
}

#[tokio::test]
async fn crumbs_go_to_that_ancestor() {
    let mock = Arc::new(
        MockProvider::new(Default::default()).with_file("wp-content/uploads/2026/a.txt", b"a"),
    );
    let s = saved(&mock).await;
    select(&s, 0).await;
    open_remote(&s, 0).await;
    open_remote(&s, 0).await;
    let deep = open_remote(&s, 0).await;
    assert_eq!(deep.crumbs, ["testzone", "wp-content", "uploads", "2026"]);

    // The last crumb is the current folder; it and anything past it do nothing.
    let calls = mock.calls(MockOp::List);
    assert_eq!(remote_crumb(&s, 3).await, deep);
    assert_eq!(remote_crumb(&s, 9).await, deep);
    assert_eq!(mock.calls(MockOp::List), calls);

    assert_eq!(remote_crumb(&s, 1).await.crumbs, ["testzone", "wp-content"]);
    let root = remote_crumb(&s, 0).await;
    assert_eq!(root.crumbs, ["testzone"]);
    assert_eq!(names(&root.rows), ["wp-content"]);
}

#[tokio::test]
async fn a_folder_that_is_gone_shows_an_error_and_stays_put() {
    let mock = site_mock();
    let s = saved(&mock).await;
    select(&s, 0).await;
    mock.fail_next(MockOp::List, ProviderError::NotFound, 1);
    let view = open_remote(&s, 0).await;
    assert_eq!(
        view.error.as_deref(),
        Some("Couldn't find /wp-content in testzone.")
    );
    assert_eq!(view.crumbs, ["testzone"]);
    assert_eq!(names(&view.rows), ["wp-content", "index.html"]);
    // Ruling P24: the server answered, so the pill stays connected and the site healthy.
    assert_eq!(view.connection, "HTTPS · 0 ms");
    assert!(view.connection_ok);
    assert_eq!(view.status_right, "HTTP/2 · sg.storage.bunnycdn.com");
    assert!(s.site_items()[0].healthy);

    // Once it can be listed again, it opens and the error goes.
    let view = open_remote(&s, 0).await;
    assert_eq!(view.crumbs, ["testzone", "wp-content"]);
    assert_eq!(view.error, None);
    assert_eq!(view.connection, "HTTPS · 0 ms");
}

#[tokio::test]
async fn other_errors_name_the_host() {
    let mock = site_mock();
    let s = saved(&mock).await;
    mock.fail_next(
        MockOp::List,
        ProviderError::Transient("connection reset".into()),
        1,
    );
    let view = select(&s, 0).await;
    assert_eq!(view.error.as_deref(), Some(UNREACHABLE));
}

/// Final review Minor 1: every kind of failure is told in plain words, with no error labels and
/// no "Couldn't reach" when the server did answer.
#[tokio::test]
async fn each_kind_of_failure_is_told_in_plain_words() {
    let mock = site_mock();
    let s = saved(&mock).await;
    let busy = ProviderError::RateLimited { retry_after: None };
    let odd = ProviderError::Permanent("unexpected response from server (not JSON)".into());
    for (error, said) in [
        (
            busy,
            "sg.storage.bunnycdn.com is busy. Try again in a moment.",
        ),
        (odd, "sg.storage.bunnycdn.com sent an unexpected response."),
        (
            ProviderError::Transient("operation timed out".into()),
            UNREACHABLE,
        ),
    ] {
        mock.fail_next(MockOp::List, error, 1);
        assert_eq!(select(&s, 0).await.error.as_deref(), Some(said));
    }
    mock.fail_next(MockOp::List, ProviderError::Permanent("x".into()), 1);
    let tested = s.test_connection(&valid("testzone", "sg", "pw")).await;
    assert_eq!(
        tested.text,
        "sg.storage.bunnycdn.com sent an unexpected response."
    );
}

#[test]
fn every_error_kind_has_its_own_plain_words() {
    let dir = RemotePath::parse("wp-content").unwrap();
    let list = Attempt::List(&dir);
    let said = |error: ProviderError, attempt| {
        describe(&error, "testzone", "sg.storage.bunnycdn.com", attempt)
    };
    let cases = [
        (said(ProviderError::Auth, list), AUTH),
        (
            said(ProviderError::NotFound, list),
            "Couldn't find /wp-content in testzone.",
        ),
        (
            said(ProviderError::RateLimited { retry_after: None }, list),
            "sg.storage.bunnycdn.com is busy. Try again in a moment.",
        ),
        (
            said(ProviderError::Transient("reset".into()), list),
            UNREACHABLE,
        ),
        (
            said(ProviderError::Permanent("unexpected (418)".into()), list),
            "sg.storage.bunnycdn.com sent an unexpected response.",
        ),
        (
            said(ProviderError::ChecksumMismatch, list),
            "sg.storage.bunnycdn.com received a damaged copy. Try again.",
        ),
        (
            said(
                ProviderError::Refused("the zone root is a folder".into()),
                list,
            ),
            "The zone root is a folder.",
        ),
        (
            said(
                ProviderError::Permanent(
                    "the storage password cannot be sent as an HTTP header".into(),
                ),
                Attempt::Setup,
            ),
            "Couldn't set up a connection to sg.storage.bunnycdn.com. The storage password cannot \
             be sent as an HTTP header.",
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(text, expected);
        for label in ["error:", "transient", "permanent", "refused:"] {
            assert!(!text.to_lowercase().contains(label), "{text}");
        }
    }
    // A long detail from the client is cut short.
    let long = said(ProviderError::Permanent("x".repeat(500)), Attempt::Setup);
    assert!(long.chars().count() < 250, "{long}");
}

#[tokio::test]
async fn with_no_site_selected_the_cloud_pane_is_empty() {
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let empty = |view: RemoteView| RemoteView { version: 0, ..view } == RemoteView::default();
    assert!(empty(select(&s, 0).await));
    assert!(empty(open_remote(&s, 0).await));
    assert!(empty(remote_up(&s).await));
    assert!(empty(remote_crumb(&s, 0).await));
    assert!(empty(s.sort_remote(SortKey::Size, true)));
}

#[tokio::test]
async fn switching_to_a_site_that_fails_drops_the_old_listing() {
    let mock = site_mock();
    let (s, _, _) = session_with(mock.clone());
    s.save_site(&valid("alpha", "sg", "pw")).await.unwrap();
    s.save_site(&valid("beta", "de", "pw")).await.unwrap();
    assert_eq!(select(&s, 0).await.rows.len(), 2);
    mock.fail_next(MockOp::List, ProviderError::Auth, 1);
    let view = select(&s, 1).await;
    assert!(view.rows.is_empty());
    assert_eq!(view.crumbs, ["beta"]);
    assert_eq!(view.endpoint, "Bunny Storage · beta");
    // Only beta failed.
    let health: Vec<bool> = s.site_items().iter().map(|i| i.healthy).collect();
    assert_eq!(health, [true, false]);
}

// ---- This PC ----

#[tokio::test]
async fn the_local_pane_starts_at_the_drives() {
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let view = s.local_view();
    assert!(view.crumbs.is_empty());
    assert!(
        view.rows.iter().any(|r| r.name == "C:" && r.folder),
        "{view:?}"
    );
    assert_eq!(view.error, None);
}

#[tokio::test]
async fn local_browsing_opens_folders_and_goes_up() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"abc").unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("sub").join("inner.txt"), b"x").unwrap();
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));

    let top = navigate_local(&s, Some(tmp.path().to_path_buf())).await;
    assert_eq!(names(&top.rows), ["sub", "a.txt"]);
    assert_eq!(top.status, "2 items");
    assert_eq!(top.crumbs, crate::viewmodel::local_crumbs(tmp.path()));

    let sub = open_local(&s, 0).await;
    assert_eq!(names(&sub.rows), ["inner.txt"]);
    assert_eq!(sub.crumbs.last().map(String::as_str), Some("sub"));
    assert_eq!(sub.status, "1 item");

    // A file does not open.
    assert_eq!(open_local(&s, 0).await, sub);
    assert_eq!(shows(&local_up(&s).await), shows(&top));
}

#[tokio::test]
async fn local_rows_stay_in_entry_order_after_a_sort_change() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("small.txt"), [0; 10]).unwrap();
    std::fs::write(tmp.path().join("big.txt"), [0; 5000]).unwrap();
    std::fs::create_dir(tmp.path().join("alpha")).unwrap();
    std::fs::create_dir(tmp.path().join("zeta")).unwrap();
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let view = navigate_local(&s, Some(tmp.path().to_path_buf())).await;
    assert_eq!(names(&view.rows), local_entry_names(&s));

    let view = s.sort_local(SortKey::Size, false);
    assert_eq!(names(&view.rows), ["alpha", "zeta", "big.txt", "small.txt"]);
    assert_eq!(names(&view.rows), local_entry_names(&s));

    let view = s.sort_local(SortKey::Name, false);
    assert_eq!(names(&view.rows), ["zeta", "alpha", "small.txt", "big.txt"]);
    assert_eq!(names(&view.rows), local_entry_names(&s));
    let view = open_local(&s, 0).await;
    assert_eq!(view.crumbs.last().map(String::as_str), Some("zeta"));
}

#[test]
fn a_local_crumb_stands_for_its_folder() {
    let dir = Path::new(r"C:\Users\me\site");
    assert_eq!(local_ancestor(dir, 0), Some(PathBuf::from(r"C:\")));
    assert_eq!(local_ancestor(dir, 2), Some(PathBuf::from(r"C:\Users\me")));
    assert_eq!(local_ancestor(dir, 3), None);
    assert_eq!(local_ancestor(dir, 8), None);
    let share = Path::new(r"\\server\share\dir");
    assert_eq!(
        local_ancestor(share, 0),
        Some(PathBuf::from(r"\\server\share\"))
    );
    assert_eq!(local_ancestor(share, 1), None);
}

#[tokio::test]
async fn local_crumbs_go_to_that_ancestor() {
    let tmp = tempfile::tempdir().unwrap();
    let deep = tmp.path().join("a").join("b");
    std::fs::create_dir_all(&deep).unwrap();
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let view = navigate_local(&s, Some(deep)).await;
    let a = view.crumbs.len() - 2;
    let view = local_crumb(&s, a).await;
    assert_eq!(view.crumbs.last().map(String::as_str), Some("a"));
    assert_eq!(names(&view.rows), ["b"]);
    // The current folder does nothing.
    let last = view.crumbs.len() - 1;
    assert_eq!(local_crumb(&s, last).await, view);
}

#[tokio::test]
async fn up_from_a_drive_goes_to_the_drives() {
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let drives = s.local_view();
    let c = drives.rows.iter().position(|r| r.name == "C:").unwrap();
    let view = open_local(&s, c).await;
    assert_eq!(view.crumbs, ["C:"]);
    let up = local_up(&s).await;
    assert_eq!((&up.rows, &up.crumbs), (&drives.rows, &drives.crumbs));
    assert!(up.can_back, "Back returns to C:");
}

#[tokio::test]
async fn an_unreadable_local_folder_shows_an_error_and_stays_put() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"abc").unwrap();
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let before = navigate_local(&s, Some(tmp.path().to_path_buf())).await;
    let missing = tmp.path().join("missing");
    let view = navigate_local(&s, Some(missing.clone())).await;
    let error = view.error.clone().unwrap();
    assert!(
        error.starts_with(&format!("Couldn't open {}: ", missing.display())),
        "{error}"
    );
    assert_eq!(view.rows, before.rows);
    assert_eq!(view.crumbs, before.crumbs);
}

#[tokio::test]
async fn a_notice_stays_for_the_banner() {
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    assert_eq!(s.notice(), None);
    s.set_notice("The site database could not be opened.".into());
    assert_eq!(
        s.notice().as_deref(),
        Some("The site database could not be opened.")
    );
}

// ---- Ruling P21: nothing waits for a cloud listing, and a late one is dropped ----

#[tokio::test]
async fn views_are_numbered_so_a_late_one_can_be_told_apart() {
    let mock = site_mock();
    let s = saved(&mock).await;
    let listed = select(&s, 0).await;
    let sorted = s.sort_remote(SortKey::Size, false);
    assert!(sorted.version > listed.version);
    // Nothing changes, and neither does the number.
    assert_eq!(open_remote(&s, 1).await.version, sorted.version);
    let opened = open_remote(&s, 0).await;
    assert!(opened.version > sorted.version);

    let local = s.local_view();
    let resorted = s.sort_local(SortKey::Size, true);
    assert!(resorted.version > local.version);
}

/// Ruling P21 (1): the This PC pane goes about its business while a cloud listing hangs.
#[tokio::test]
async fn local_browsing_does_not_wait_for_a_cloud_listing() {
    let gated = Gated::new(
        MockProvider::new(Default::default()).with_file("a.txt", b"a"),
        "",
    );
    let (s, _, _) = session_with(gated.clone());
    let s = Arc::new(s);
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    let hanging = tokio::spawn({
        let s = s.clone();
        async move { select(&s, 0).await }
    });
    until_held(&gated).await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("sub").join("deeper")).unwrap();
    let top = timeout(PROMPTLY, navigate_local(&s, Some(tmp.path().to_path_buf())))
        .await
        .expect("listing a local folder waited for the cloud");
    assert_eq!(names(&top.rows), ["sub"]);
    let sub = timeout(PROMPTLY, open_local(&s, 0))
        .await
        .expect("open waited");
    assert_eq!(sub.crumbs.last().map(String::as_str), Some("sub"));
    timeout(PROMPTLY, open_local(&s, 0))
        .await
        .expect("open waited");
    let crumb = sub.crumbs.len() - 1;
    let back = timeout(PROMPTLY, local_crumb(&s, crumb))
        .await
        .expect("a crumb waited");
    assert_eq!(back.crumbs, sub.crumbs);
    timeout(PROMPTLY, local_up(&s)).await.expect("up waited");
    s.sort_local(SortKey::Size, false);
    let _ = s.local_view();

    // The cloud listing is still out, and nothing it holds was needed.
    assert_eq!(gated.waiting(), 1);
    assert!(!hanging.is_finished());
    hanging.abort();
}

/// Ruling P21 (2): the site list, saving a site and testing one do not wait for a cloud listing.
#[tokio::test]
async fn sites_can_be_saved_and_tested_while_a_cloud_listing_hangs() {
    let gated = Gated::new(MockProvider::new(Default::default()), "");
    let (s, _, factory) = session_with(Arc::new(
        MockProvider::new(Default::default()).with_file("index.html", b"x"),
    ));
    factory.serve("hanging-zone", gated.clone());
    let s = Arc::new(s);
    s.save_site(&valid("hanging-zone", "sg", "pw"))
        .await
        .unwrap();
    let hanging = tokio::spawn({
        let s = s.clone();
        async move { select(&s, 0).await }
    });
    until_held(&gated).await;

    let saved = timeout(PROMPTLY, s.save_site(&valid("other-zone", "de", "pw")))
        .await
        .expect("saving a site waited for the cloud")
        .unwrap();
    assert!(s.sites().iter().any(|site| site.id == saved.id));
    assert_eq!(s.site_items().len(), 2);
    let tested = timeout(
        PROMPTLY,
        s.test_connection(&valid("other-zone", "de", "pw")),
    )
    .await
    .expect("testing a site waited for the cloud");
    assert!(tested.ok, "{}", tested.text);

    assert_eq!(gated.waiting(), 1);
    assert!(!hanging.is_finished());
    hanging.abort();
}

/// Ruling P21 (3): a listing of a site the user has left is dropped when it finally arrives.
#[tokio::test]
async fn a_late_listing_from_a_site_the_user_left_is_dropped() {
    let gated = Gated::new(
        MockProvider::new(Default::default()).with_file("from-a.txt", b"a"),
        "",
    );
    let (s, _, factory) = session_with(Arc::new(
        MockProvider::new(Default::default()).with_file("from-b.txt", b"b"),
    ));
    factory.serve("zone-a", gated.clone());
    let s = Arc::new(s);
    s.save_site(&valid("zone-a", "sg", "pw")).await.unwrap();
    s.save_site(&valid("zone-b", "de", "pw")).await.unwrap();

    let late = tokio::spawn({
        let s = s.clone();
        async move { select(&s, 0).await }
    });
    until_held(&gated).await;
    let b = timeout(PROMPTLY, select(&s, 1))
        .await
        .expect("opening another site waited for the held-back listing");
    assert_eq!(names(&b.rows), ["from-b.txt"]);

    // Site A's answer finally comes, and it is a failure: neither its rows nor its error nor its
    // health may reach the pane or the sidebar.
    gated.mock().fail_next(MockOp::List, ProviderError::Auth, 1);
    gated.open();
    let answer = timeout(PROMPTLY, late).await.unwrap().unwrap();
    assert_eq!(answer, b, "the late answer reports the pane as it is");
    assert_eq!(current_remote(&s), b);
    assert_eq!(remote_entry_names(&s), ["from-b.txt"]);
    assert!(s.site_items().iter().all(|site| site.healthy));
}

/// Ruling P21 (3): the same for a folder the user has left within the site.
#[tokio::test]
async fn a_late_listing_from_a_folder_the_user_left_is_dropped() {
    let gated = Gated::new(
        MockProvider::new(Default::default())
            .with_file("fast/quick.txt", b"q")
            .with_file("slow/late.txt", b"l"),
        "slow",
    );
    let (s, _, _) = session_with(gated.clone());
    let s = Arc::new(s);
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    let root = select(&s, 0).await;
    assert_eq!(names(&root.rows), ["fast", "slow"]);

    let late = tokio::spawn({
        let s = s.clone();
        async move { open_remote(&s, 1).await }
    });
    until_held(&gated).await;
    let fast = timeout(PROMPTLY, open_remote(&s, 0))
        .await
        .expect("opening another folder waited for the held-back listing");
    assert_eq!(fast.crumbs, ["testzone", "fast"]);

    gated.open();
    let answer = timeout(PROMPTLY, late).await.unwrap().unwrap();
    assert_eq!(answer, fast);
    assert_eq!(current_remote(&s).crumbs, ["testzone", "fast"]);
    assert_eq!(remote_entry_names(&s), ["quick.txt"]);
}

// ---- Fix round 1 ----

/// Two sites: "zone-a" (from-a.txt, served for zone-a) and "zone-b" (from-b.txt, the default).
async fn two_sites() -> (Arc<Session>, Arc<FakeFactory>) {
    let (s, _, factory) = session_with(Arc::new(
        MockProvider::new(Default::default()).with_file("from-b.txt", b"b"),
    ));
    factory.serve(
        "zone-a",
        Arc::new(MockProvider::new(Default::default()).with_file("from-a.txt", b"a")),
    );
    s.save_site(&valid("zone-a", "sg", "pw")).await.unwrap();
    s.save_site(&valid("zone-b", "de", "pw")).await.unwrap();
    (Arc::new(s), factory)
}

/// Review finding: an earlier selection never overtakes a later one. The user picks site A,
/// then site B; A's client takes long to make (held in the factory), so B is done first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_earlier_selection_never_overtakes_a_later_one() {
    let (s, factory) = two_sites().await;
    let hold = factory.hold("zone-a");
    // The window takes the tickets in the order of the clicks.
    let (a, b) = (s.remote_ticket(), s.remote_ticket());
    let first = tokio::spawn({
        let s = s.clone();
        async move { s.select_site(a, 0, |_| {}).await }
    });
    hold.entered().await;
    let shown = timeout(PROMPTLY, s.select_site(b, 1, |_| {}))
        .await
        .expect("site B waited for site A's client");
    assert_eq!(names(&shown.rows), ["from-b.txt"]);

    hold.release();
    let late = timeout(PROMPTLY, first).await.unwrap().unwrap();
    assert_eq!(late, shown, "A's late answer reports the pane as it is");
    assert_eq!(current_remote(&s), shown);
    assert_eq!(current_remote(&s).endpoint, "Bunny Storage · zone-b");
    assert_eq!(remote_entry_names(&s), ["from-b.txt"]);
}

/// The same race the other way round: B's task runs to the end before A's has started.
#[tokio::test]
async fn a_selection_whose_task_starts_late_is_dropped() {
    let (s, factory) = two_sites().await;
    let (a, b) = (s.remote_ticket(), s.remote_ticket());
    let shown = s.select_site(b, 1, |_| {}).await;
    let late = s
        .select_site(a, 0, |_| unreachable!("A must not switch the pane"))
        .await;
    assert_eq!(late, shown);
    assert_eq!(current_remote(&s), shown);
    assert!(
        factory
            .configs()
            .iter()
            .all(|(zone, _, _)| zone != "zone-a")
    );
}

/// Review Minor #2: the switch to a site is shown at once, before its client is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_switch_to_a_site_is_shown_before_it_connects() {
    let mock = site_mock();
    let (s, _, factory) = session_with(mock.clone());
    let s = Arc::new(s);
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    let hold = factory.hold("testzone");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let ticket = s.remote_ticket();
    let task = tokio::spawn({
        let s = s.clone();
        async move {
            s.select_site(ticket, 0, move |view| {
                let _ = tx.send(view);
            })
            .await
        }
    });
    let switched = timeout(PROMPTLY, rx)
        .await
        .expect("the switch waited for the client")
        .unwrap();
    hold.entered().await;
    assert!(switched.rows.is_empty());
    assert_eq!(switched.crumbs, ["testzone"]);
    assert_eq!(switched.connection, "Connecting…");
    assert!(switched.connection_ok);
    assert_eq!(switched.endpoint, "Bunny Storage · testzone");
    assert_eq!(switched.chip, "https://sg.storage.bunnycdn.com");
    assert_eq!(switched.status_right, "sg.storage.bunnycdn.com");
    assert_eq!(switched.error, None);
    assert!(switched.loading, "the root listing is on its way");

    hold.release();
    let listed = timeout(PROMPTLY, task).await.unwrap().unwrap();
    assert!(listed.version > switched.version);
    assert!(!listed.loading);
    assert_eq!(listed.connection, "HTTPS · 0 ms");
    assert_eq!(names(&listed.rows), ["wp-content", "index.html"]);
}

/// A request that goes nowhere (a file, the folder already shown) takes no effect, so it does not
/// spoil a listing that is on its way.
#[tokio::test]
async fn a_request_that_goes_nowhere_does_not_cancel_one_on_its_way() {
    let gated = Gated::new(
        MockProvider::new(Default::default())
            .with_file("slow/late.txt", b"l")
            .with_file("readme.txt", b"r"),
        "slow",
    );
    let (s, _, _) = session_with(gated.clone());
    let s = Arc::new(s);
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    select(&s, 0).await;
    let slow = tokio::spawn({
        let s = s.clone();
        async move { open_remote(&s, 0).await }
    });
    until_held(&gated).await;
    open_remote(&s, 1).await; // a file
    remote_crumb(&s, 0).await; // the folder already shown
    remote_up(&s).await; // nowhere above the root
    remote_back(&s).await; // nothing to go back to
    gated.open();
    let view = timeout(PROMPTLY, slow).await.unwrap().unwrap();
    assert_eq!(view.crumbs, ["testzone", "slow"]);
}

/// Ruling P23: a click names the version of the rows it was on; if they have changed since (here,
/// a new sort), it is ignored rather than applied to whatever row has that index now.
#[tokio::test]
async fn a_click_on_rows_that_have_changed_since_is_ignored() {
    let mock = Arc::new(
        MockProvider::new(Default::default())
            .with_folder("alpha")
            .with_folder("zeta"),
    );
    let s = saved(&mock).await;
    let seen = select(&s, 0).await;
    assert_eq!(names(&seen.rows), ["alpha", "zeta"]);
    let resorted = s.sort_remote(SortKey::Name, false);
    let calls = mock.calls(MockOp::List);
    let ignored = s
        .open_remote(s.remote_ticket(), 0, seen.version, |_| {})
        .await;
    assert_eq!(ignored, resorted);
    let ignored = s
        .remote_crumb(s.remote_ticket(), 0, seen.version, |_| {})
        .await;
    assert_eq!(ignored, resorted);
    assert_eq!(mock.calls(MockOp::List), calls);
    let opened = s
        .open_remote(s.remote_ticket(), 0, resorted.version, |_| {})
        .await;
    assert_eq!(opened.crumbs, ["testzone", "zeta"]);

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("alpha")).unwrap();
    std::fs::create_dir(tmp.path().join("zeta")).unwrap();
    let seen = navigate_local(&s, Some(tmp.path().to_path_buf())).await;
    let resorted = s.sort_local(SortKey::Name, false);
    let ignored = s
        .open_local(s.local_ticket(), 0, seen.version, |_| {})
        .await;
    assert_eq!(ignored, resorted);
    let ignored = s
        .local_crumb(s.local_ticket(), 0, seen.version, |_| {})
        .await;
    assert_eq!(ignored, resorted);
    let opened = s
        .open_local(s.local_ticket(), 0, resorted.version, |_| {})
        .await;
    assert_eq!(opened.crumbs.last().map(String::as_str), Some("zeta"));
}

/// Ruling P22: Back and Forward retrace the folders of the cloud pane, Finder-style.
#[tokio::test]
async fn back_and_forward_retrace_the_cloud_folders() {
    let mock = Arc::new(
        MockProvider::new(Default::default())
            .with_file("a/b/x.txt", b"x")
            .with_file("c/y.txt", b"y"),
    );
    let s = saved(&mock).await;
    let root = select(&s, 0).await;
    assert!(!root.can_back && !root.can_forward);
    let a = open_remote(&s, 0).await;
    assert_eq!(a.crumbs, ["testzone", "a"]);
    assert!(a.can_back && !a.can_forward);
    open_remote(&s, 0).await;

    let back = remote_back(&s).await;
    assert_eq!(back.crumbs, ["testzone", "a"]);
    assert!(back.can_back && back.can_forward);
    let back = remote_back(&s).await;
    assert_eq!(back.crumbs, ["testzone"]);
    assert!(!back.can_back && back.can_forward);
    let forward = remote_forward(&s).await;
    assert_eq!(forward.crumbs, ["testzone", "a"]);
    assert!(forward.can_back && forward.can_forward);

    // Going somewhere new drops the way forward; Back then returns to where it came from.
    let up = remote_up(&s).await;
    assert_eq!(up.crumbs, ["testzone"]);
    assert!(up.can_back && !up.can_forward);
    assert_eq!(remote_back(&s).await.crumbs, ["testzone", "a"]);

    // With nowhere to go, Forward does nothing.
    let here = remote_forward(&s).await;
    assert!(!here.can_forward);
    assert_eq!(remote_forward(&s).await, here);
}

#[tokio::test]
async fn a_back_that_fails_keeps_the_history() {
    let mock = site_mock();
    let s = saved(&mock).await;
    select(&s, 0).await;
    open_remote(&s, 0).await;
    mock.fail_next(MockOp::List, ProviderError::NotFound, 1);
    let failed = remote_back(&s).await;
    assert_eq!(failed.crumbs, ["testzone", "wp-content"]);
    assert!(failed.can_back && !failed.can_forward);
    assert!(failed.error.is_some());
    let back = remote_back(&s).await;
    assert_eq!(back.crumbs, ["testzone"]);
    assert_eq!(back.error, None);
}

#[tokio::test]
async fn switching_site_starts_a_new_history() {
    let mock = Arc::new(MockProvider::new(Default::default()).with_file("f/x.txt", b"x"));
    let (single, _, _) = session_with(mock);
    single
        .save_site(&valid("zone-1", "sg", "pw"))
        .await
        .unwrap();
    single
        .save_site(&valid("zone-2", "de", "pw"))
        .await
        .unwrap();
    select(&single, 0).await;
    let inside = open_remote(&single, 0).await;
    assert!(inside.can_back);
    let other = select(&single, 1).await;
    assert!(!other.can_back && !other.can_forward);
    assert_eq!(remote_back(&single).await, other);
}

/// Ruling P22: the same for the This PC pane, where the list of drives is a place too.
#[tokio::test]
async fn back_and_forward_retrace_the_local_folders() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("sub").join("deeper")).unwrap();
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    assert!(!s.local_view().can_back);
    let top = navigate_local(&s, Some(tmp.path().to_path_buf())).await;
    assert!(top.can_back && !top.can_forward);
    open_local(&s, 0).await;

    let back = local_back(&s).await;
    assert_eq!(back.crumbs, top.crumbs);
    assert!(back.can_back && back.can_forward);
    let drives = local_back(&s).await;
    assert!(drives.crumbs.is_empty());
    assert!(!drives.can_back && drives.can_forward);
    let forward = local_forward(&s).await;
    assert_eq!(forward.crumbs, top.crumbs);

    let up = local_up(&s).await;
    assert!(up.can_back && !up.can_forward);
    assert_eq!(local_back(&s).await.crumbs, top.crumbs);
}

/// Ruling P24: a folder that is not there is not a connection failure (see also
/// `a_folder_that_is_gone_shows_an_error_and_stays_put`); a failed connection still is.
#[tokio::test]
async fn only_a_failed_connection_turns_the_pill_orange() {
    let mock = site_mock();
    let s = saved(&mock).await;
    select(&s, 0).await;
    mock.fail_next(MockOp::List, ProviderError::NotFound, 1);
    let missing = open_remote(&s, 0).await;
    assert!(missing.connection_ok);
    assert!(s.site_items()[0].healthy);
    mock.fail_next(
        MockOp::List,
        ProviderError::Transient("connection reset".into()),
        1,
    );
    let offline = open_remote(&s, 0).await;
    assert!(!offline.connection_ok);
    assert_eq!(offline.connection, "Not connected");
    assert!(!s.site_items()[0].healthy);
}

// ---- Final fix wave ----

/// Important #1: Add Site lists the zone first (as Test Connection does) and saves nothing when
/// the password is rejected.
#[tokio::test]
async fn a_site_whose_password_is_rejected_is_not_added() {
    let mock = site_mock();
    let (s, creds, factory) = session_with(mock.clone());
    mock.fail_next(MockOp::List, ProviderError::Auth, 1);
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
        ProviderError::Transient("operation timed out".into()),
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
/// deletes keep the default policy.
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
        assert_eq!(
            cfg.list_retry,
            Some(RetryPolicy {
                delays: vec![Duration::from_secs(1), Duration::from_secs(2)]
            })
        );
        assert_eq!(cfg.retry, default.retry);
        assert_eq!(cfg.transfer_idle_timeout, default.transfer_idle_timeout);
        assert_eq!(cfg.transfer_concurrency, default.transfer_concurrency);
        assert_eq!(cfg.delete_concurrency, default.delete_concurrency);
    }
}

/// Minor 2: while a folder's listing is out the pane says it is loading, and keeps showing the
/// folder it is in; the answer clears it.
#[tokio::test]
async fn a_cloud_pane_is_loading_while_its_listing_is_out() {
    let gated = Gated::new(
        MockProvider::new(Default::default())
            .with_file("slow/late.txt", b"l")
            .with_file("readme.txt", b"r"),
        "slow",
    );
    let (s, _, _) = session_with(gated.clone());
    let s = Arc::new(s);
    s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    let root = select(&s, 0).await;
    assert!(!root.loading);

    let (tx, rx) = tokio::sync::oneshot::channel();
    let seen = root.version;
    let ticket = s.remote_ticket();
    let task = tokio::spawn({
        let s = s.clone();
        async move {
            s.open_remote(ticket, 0, seen, move |view| {
                let _ = tx.send(view);
            })
            .await
        }
    });
    let loading = timeout(PROMPTLY, rx).await.unwrap().unwrap();
    until_held(&gated).await;
    assert!(loading.loading);
    assert!(loading.version > root.version, "a view the window takes");
    assert_eq!((&loading.rows, &loading.crumbs), (&root.rows, &root.crumbs));
    assert_eq!(current_remote(&s), loading);
    // Anything that goes nowhere meanwhile reports the pane as loading still.
    assert!(remote_up(&s).await.loading);

    gated.open();
    let listed = timeout(PROMPTLY, task).await.unwrap().unwrap();
    assert!(!listed.loading);
    assert_eq!(listed.crumbs, ["testzone", "slow"]);
}

#[tokio::test]
async fn a_failed_listing_clears_the_loading_state_too() {
    let mock = site_mock();
    let s = saved(&mock).await;
    select(&s, 0).await;
    mock.fail_next(MockOp::List, ProviderError::Auth, 1);
    let failed = open_remote(&s, 0).await;
    assert_eq!(failed.error.as_deref(), Some(AUTH));
    assert!(!failed.loading);
    mock.fail_next(MockOp::List, ProviderError::Auth, 1);
    let failed = select(&s, 0).await;
    assert!(failed.error.is_some() && !failed.loading);
}

#[tokio::test]
async fn the_local_pane_is_loading_while_its_listing_is_out() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let before = s.local_view();
    let mut started = None;
    let dir = tmp.path().to_path_buf();
    let listed = s
        .navigate_local(
            s.local_ticket(),
            Step::Go,
            move |_| Some(Some(dir)),
            |view| started = Some(view),
        )
        .await;
    let started = started.expect("the pane said it was loading");
    assert!(started.loading);
    assert!(started.version > before.version);
    assert_eq!(
        started.rows, before.rows,
        "the drives stay until the answer"
    );
    assert!(!listed.loading);
    assert_eq!(names(&listed.rows), ["a.txt"]);

    // A folder that can't be read clears it as well.
    let failed = navigate_local(&s, Some(tmp.path().join("missing"))).await;
    assert!(failed.error.is_some() && !failed.loading);
}

/// Minor 3: Refresh lists the folder shown again, and leaves the history as it is.
#[tokio::test]
async fn refresh_lists_the_cloud_folder_again_and_keeps_the_history() {
    let mock = Arc::new(MockProvider::new(Default::default()).with_file("a/x.txt", b"x"));
    let s = saved(&mock).await;
    select(&s, 0).await;
    open_remote(&s, 0).await;
    let root = remote_back(&s).await;
    assert!(!root.can_back && root.can_forward);
    mock.upload(
        &RemotePath::parse("fresh.txt").unwrap(),
        UploadSource::Bytes(b"new".to_vec().into()),
        &UploadOptions {
            content_type: "text/plain".into(),
            sha256: None,
        },
    )
    .await
    .unwrap();

    let calls = mock.calls(MockOp::List);
    let refreshed = remote_refresh(&s).await;
    assert_eq!(mock.calls(MockOp::List), calls + 1);
    assert_eq!(names(&refreshed.rows), ["a", "fresh.txt"]);
    assert_eq!(refreshed.crumbs, root.crumbs);
    assert_eq!(
        (refreshed.can_back, refreshed.can_forward),
        (root.can_back, root.can_forward)
    );
    assert!(refreshed.version > root.version);
    assert_eq!(remote_forward(&s).await.crumbs, ["testzone", "a"]);
}

#[tokio::test]
async fn refresh_with_no_site_or_no_client_does_nothing() {
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let empty = current_remote(&s);
    assert_eq!(remote_refresh(&s).await, empty);

    // A site with no password has no client to list with.
    let mock = site_mock();
    let (s, creds, _) = session_with(mock.clone());
    let rec = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    creds.delete(&CredentialKey::site(&rec.id)).unwrap();
    let failed = select(&s, 0).await;
    assert_eq!(remote_refresh(&s).await, failed);
    assert_eq!(mock.calls(MockOp::List), 0);
}

#[tokio::test]
async fn refresh_lists_the_local_folder_again_and_keeps_the_history() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    let (s, _) = session_with_mock(MockProvider::new(Default::default()));
    let top = navigate_local(&s, Some(tmp.path().to_path_buf())).await;
    std::fs::write(tmp.path().join("b.txt"), b"b").unwrap();
    let refreshed = local_refresh(&s).await;
    assert_eq!(names(&refreshed.rows), ["a.txt", "b.txt"]);
    assert_eq!(refreshed.crumbs, top.crumbs);
    assert_eq!(refreshed.status, "2 items");
    assert_eq!(
        (refreshed.can_back, refreshed.can_forward),
        (top.can_back, top.can_forward)
    );
    // Back still goes where it went before.
    assert!(local_back(&s).await.crumbs.is_empty());
    // The list of drives can be refreshed too.
    assert!(local_refresh(&s).await.rows.iter().any(|r| r.name == "C:"));
}
