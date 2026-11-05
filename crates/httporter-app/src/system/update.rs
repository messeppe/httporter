//! The opt-in update check (§16): the release feed of `RELEASE_REPO` on GitHub, read at start
//! (at most once in 24 h) or on Check Now. No silent install, no telemetry: one GET with the
//! app's name and version as its user agent, nothing else.
//!
//! - **The request:** `GET https://api.github.com/repos/{repo}/releases/latest` with
//!   `User-Agent: HTTPorter/<version>` and `Accept: application/vnd.github+json`, a 5 s timeout,
//!   HTTPS only, through the proxy of Settings › Transfers.
//! - **The answer:** `tag_name` (a leading `v` stripped), `html_url` (an `https://` page) and the
//!   first 600 characters of `body`.
//! - **A newer release:** a toast "HTTPorter X is available" with View Release, which opens the
//!   page in the browser, and the note under Check Now shows the notes. HTTPorter downloads
//!   nothing.
//! - **A failure** (offline, a 403 or 404, an answer not understood, no answer in 5 s) goes only to
//!   the activity log and the note: no toast, no alert.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use httporter_core::{ActivityKind, ProxyMode, UiState};
use httporter_platform::win;

use super::copy;
use crate::comfort::toast::{self, ToastAction, ToastKind};
use crate::manage::Manager;
use crate::manage::activity_view;

/// The release repository (`owner/repo`) this build checks; None turns the check off.
pub const RELEASE_REPO: Option<&str> = option_env!("HTTPORTER_RELEASE_REPO");

/// This build's version, as the release feed's tags name versions.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GitHub API.
const API: &str = "https://api.github.com";
/// The whole request, connecting included.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How much of the release notes the note shows.
const NOTES_MAX: usize = 600;
/// A release's answer is a few kB; one past this is not a release.
const BODY_MAX: usize = 2 * 1024 * 1024;
/// The check at start runs at most this often.
const EVERY_SECS: i64 = 24 * 60 * 60;

/// The newest release: its version (a leading `v` stripped), its page and the start of its
/// notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub notes: String,
}

/// The client for the check: the user agent, the 5 s timeout, HTTPS only, and the proxy of
/// Settings › Transfers (the system's, none, or the one typed).
pub fn client(proxy: &ProxyMode) -> Result<reqwest::Client, String> {
    // reqwest brings no TLS crypto (`rustls-no-provider`): ring's, as the providers use. Later
    // calls are no-ops.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let builder = reqwest::Client::builder()
        .user_agent(format!("HTTPorter/{VERSION}"))
        .timeout(TIMEOUT)
        .https_only(true);
    let builder = match proxy {
        // reqwest's default: the Windows settings.
        ProxyMode::System => builder,
        ProxyMode::None => builder.no_proxy(),
        ProxyMode::Custom(url) => {
            // The URL may hold a password, so the message doesn't repeat it.
            let proxy =
                reqwest::Proxy::all(url.as_str()).map_err(|_| copy::UPDATE_BAD_PROXY.to_owned())?;
            builder.proxy(proxy)
        }
    };
    builder.build().map_err(copy::update_request_failed)
}

/// `GET https://api.github.com/repos/{repo}/releases/latest`; the error is for the activity log
/// and the note.
pub async fn latest(client: &reqwest::Client, repo: &str) -> Result<Release, String> {
    if !valid_repo(repo) {
        return Err(copy::UPDATE_BAD_REPO.to_owned());
    }
    let url = format!("{API}/repos/{repo}/releases/latest");
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(request_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(copy::update_status(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|n| n > BODY_MAX as u64)
    {
        return Err(copy::UPDATE_TOO_BIG.to_owned());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if body.len() + chunk.len() > BODY_MAX {
            return Err(copy::UPDATE_TOO_BIG.to_owned());
        }
        body.extend_from_slice(&chunk);
    }
    parse(&body)
}

/// `owner/repo`, each part GitHub's letters, digits, `-`, `_` and `.` (not `.` or `..`).
fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let part_ok = |part: Option<&str>| {
        part.is_some_and(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.len() <= 100
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    };
    part_ok(parts.next()) && part_ok(parts.next()) && parts.next().is_none()
}

/// What a request error was, in words: no answer in time, no connection, or the reason.
fn request_error(e: reqwest::Error) -> String {
    if e.is_timeout() {
        copy::UPDATE_TIMEOUT.to_owned()
    } else if e.is_connect() {
        copy::UPDATE_OFFLINE.to_owned()
    } else {
        // Without the URL: it is the public feed, and adds nothing.
        copy::update_request_failed(e.without_url())
    }
}

/// The release in a `releases/latest` answer: a version, an `https://` page, and the start of
/// the notes (which may be missing).
fn parse(body: &[u8]) -> Result<Release, String> {
    let malformed = || copy::UPDATE_MALFORMED.to_owned();
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| malformed())?;
    let text = |name: &str| value.get(name).and_then(serde_json::Value::as_str);
    let tag = text("tag_name").ok_or_else(malformed)?.trim();
    let version = tag.strip_prefix(['v', 'V']).unwrap_or(tag);
    let url = text("html_url").ok_or_else(malformed)?.trim();
    let https = url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
    if version.is_empty() || !https || url.len() <= 8 {
        return Err(malformed());
    }
    let notes: String = text("body")
        .unwrap_or_default()
        .replace("\r\n", "\n")
        .chars()
        .take(NOTES_MAX)
        .collect();
    Ok(Release {
        version: version.to_owned(),
        url: url.to_owned(),
        notes: notes.trim().to_owned(),
    })
}

/// Whether `candidate` is a newer version than `current` (a pre-release never is). Both are
/// `MAJOR[.MINOR[.PATCH]]`, a leading `v` allowed, build metadata (`+…`) ignored; anything else
/// is never newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (version(candidate), version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// `[major, minor, patch]` of a release version; None for a pre-release or anything else.
fn version(text: &str) -> Option<[u64; 3]> {
    let text = text.trim();
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    let text = text.split_once('+').map_or(text, |(version, _)| version);
    let mut parts = [0u64; 3];
    for (count, part) in text.split('.').enumerate() {
        if count == 3 || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        parts[count] = part.parse().ok()?;
    }
    Some(parts)
}

// ---- The check, at start and on Check Now ----

/// A check is on its way: another press waits for it.
static CHECKING: AtomicBool = AtomicBool::new(false);
/// What the last check of this run said, for the note under Check Now.
static NOTE: Mutex<String> = Mutex::new(String::new());

/// The note under Check Now: the last check's result, or why the rows are off.
pub(super) fn note() -> String {
    if RELEASE_REPO.is_none() {
        return copy::NO_RELEASE_ADDRESS.to_owned();
    }
    NOTE.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn set_note(text: String) {
    *NOTE.lock().unwrap_or_else(PoisonError::into_inner) = text;
    super::show_update_note();
}

/// Whether the check at start runs now (`now` in Unix seconds): only when it is turned on, the
/// build has a release address, and the last check is more than 24 h ago (or in the future: the
/// clock was changed).
pub(super) fn due(state: &UiState, now: i64) -> bool {
    let age = now.saturating_sub(state.last_update_check);
    RELEASE_REPO.is_some() && state.check_updates && !(0..=EVERY_SECS).contains(&age)
}

/// Reads the release feed off the UI thread and shows what it says: a toast for a newer release,
/// the note always, the activity log for a failure. The time of the try is saved first, so that
/// the check at start runs at most once a day, failed or not. A second call while one runs does
/// nothing.
pub(super) fn check(m: &Arc<Manager>) {
    let Some(repo) = RELEASE_REPO else {
        return;
    };
    if CHECKING.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = UiState {
        last_update_check: chrono::Utc::now().timestamp(),
        ..m.session.ui_state()
    };
    m.session.save_ui_state(&state);
    set_note(copy::UPDATE_CHECKING.to_owned());
    let proxy = m.session.settings().proxy;
    let ui = m.link.ui().clone();
    m.link.rt().spawn(async move {
        let result = match client(&proxy) {
            Ok(client) => latest(&client, repo).await,
            Err(e) => Err(e),
        };
        let _ = slint::invoke_from_event_loop(move || {
            CHECKING.store(false, Ordering::SeqCst);
            match result {
                Ok(release) if is_newer(&release.version, VERSION) => {
                    set_note(copy::update_note(&release.version, &release.notes));
                    if let Some(ui) = ui.upgrade() {
                        let url = release.url;
                        let view = ToastAction {
                            label: copy::VIEW_RELEASE.to_owned(),
                            run: Box::new(move || {
                                let _ = win::open_url(&url);
                            }),
                        };
                        toast::show(
                            &ui,
                            ToastKind::Info,
                            copy::update_available(&release.version),
                            Some(view),
                        );
                    }
                }
                Ok(_) => set_note(copy::up_to_date(VERSION)),
                Err(e) => {
                    set_note(copy::update_failed(&e));
                    activity_view::event(
                        ActivityKind::Site,
                        "",
                        "",
                        false,
                        copy::update_failed_log(&e),
                        0,
                    );
                }
            }
        });
    });
}
