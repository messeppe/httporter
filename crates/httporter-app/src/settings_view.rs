// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Settings window's Transfers form: what the window holds, as plain values, and the checks
//! made before they become [`TransferSettings`].
//!
//! The window (`ui/components/settings_window.slint`) only holds what the user sees: a number, a
//! position in a drop-down, the text of a field. [`to_form`] turns the stored settings into that,
//! and [`from_form`] checks it and turns it back, so the window never needs to know what a
//! `TransferSettings` is. Every edit applies at once (the window has no OK button): [`wire`]
//! reads the form on each edit, shows a message under the field that is wrong, or hands the new
//! settings on.
//!
//! The words come from `copy.rs`.
//!
//! A proxy password never reaches the settings: a custom proxy address with a user name or
//! password in it is refused, and the form's `Debug` output leaves such an address out.
//!
//! The Publish tab lists the Bunny account keys kept for purging the CDN, each with Forget Key.
//! The keys belong to Publish, which hands this module a [`KeyKeeper`] when the window is wired
//! ([`set_key_keeper`]); the tab asks it for the list whenever it is opened. No key ever reaches
//! the window: only the account's name and id.
//!
//! Both windows' `Theme` follow Windows' text size and "Animation effects" ([`Look`]): read at
//! start, and again whenever Windows may have changed them ([`system_settings_changed`]).

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use httporter_core::{ConflictPolicy, HttpProtocol, ProxyMode, TransferSettings};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::{AppWindow, ProfileItem, SettingsWindow, Theme, copy};

/// The name of the field a refused form is refused for ([`from_form`]'s error).
pub const FIELD_BANDWIDTH: &str = "bandwidth";
pub const FIELD_PROXY: &str = "proxy";

/// The most simultaneous transfers the window offers, and the fewest.
const MAX_SIMULTANEOUS: i32 = 128;
const MIN_SIMULTANEOUS: i32 = 1;

/// A bandwidth limit is typed in MB/s, meaning 1,048,576 bytes a second (the sizes in the rest of
/// the app are binary too).
const BYTES_PER_MB: f64 = 1_048_576.0;
/// What the limit field offers while the limit is off (the mockup shows 10 MB/s greyed out).
const SUGGESTED_LIMIT_MB: &str = "10";

/// The drop-downs' choices in the order the window lists them: retries "5 times, 3 times, Never";
/// timeouts "120, 60, 300 seconds"; HTTP "when available, 1.1 only". The two conflict lists are in
/// different orders (the mockup puts Skip before Keep both for uploads and after it for
/// downloads).
const RETRIES: [u8; 3] = [5, 3, 0];
const TIMEOUTS: [u32; 3] = [120, 60, 300];
const PROTOCOLS: [HttpProtocol; 2] = [HttpProtocol::Http2WhenAvailable, HttpProtocol::Http1Only];
const UPLOAD_CONFLICTS: [ConflictPolicy; 5] = [
    ConflictPolicy::Ask,
    ConflictPolicy::ReplaceIfDifferent,
    ConflictPolicy::Skip,
    ConflictPolicy::Replace,
    ConflictPolicy::KeepBoth,
];
const DOWNLOAD_CONFLICTS: [ConflictPolicy; 5] = [
    ConflictPolicy::Ask,
    ConflictPolicy::ReplaceIfDifferent,
    ConflictPolicy::KeepBoth,
    ConflictPolicy::Replace,
    ConflictPolicy::Skip,
];

/// The proxy drop-down's positions.
const PROXY_SYSTEM: i32 = 0;
const PROXY_NONE: i32 = 1;
const PROXY_CUSTOM: i32 = 2;

/// The Transfers form as the window holds it. The `_index` fields are positions in a drop-down
/// (see the lists above); an index the window can't produce falls back to the default choice.
#[derive(Clone, PartialEq, Eq)]
pub struct SettingsFormData {
    /// Transfers at once, per site; kept within 1 to 128.
    pub simultaneous: i32,
    pub adaptive: bool,
    /// 0: 5 times, 1: 3 times, 2: never.
    pub retries_index: i32,
    pub bandwidth_on: bool,
    /// The limit as typed, in MB/s. Only read while `bandwidth_on`.
    pub bandwidth_mb: String,
    /// 0: HTTP/2 when available, 1: HTTP/1.1 only.
    pub protocol_index: i32,
    pub keep_alive: bool,
    /// 0: 120 seconds, 1: 60, 2: 300.
    pub timeout_index: i32,
    /// 0: Windows settings, 1: no proxy, 2: custom.
    pub proxy_index: i32,
    /// The custom proxy's address as typed. Only read for a custom proxy.
    pub proxy_url: String,
    pub verify: bool,
    pub upload_conflicts_index: i32,
    pub download_conflicts_index: i32,
}

// Not derived: a derived `Debug` would print a password typed into the proxy address in a failed
// assertion or a log line. As with `ProxyMode`, an address with an `@` anywhere is left out whole.
impl fmt::Debug for SettingsFormData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let proxy_url = if self.proxy_url.contains('@') {
            "<redacted>"
        } else {
            &self.proxy_url
        };
        f.debug_struct("SettingsFormData")
            .field("simultaneous", &self.simultaneous)
            .field("adaptive", &self.adaptive)
            .field("retries_index", &self.retries_index)
            .field("bandwidth_on", &self.bandwidth_on)
            .field("bandwidth_mb", &self.bandwidth_mb)
            .field("protocol_index", &self.protocol_index)
            .field("keep_alive", &self.keep_alive)
            .field("timeout_index", &self.timeout_index)
            .field("proxy_index", &self.proxy_index)
            .field("proxy_url", &proxy_url)
            .field("verify", &self.verify)
            .field("upload_conflicts_index", &self.upload_conflicts_index)
            .field("download_conflicts_index", &self.download_conflicts_index)
            .finish()
    }
}

/// The position of `value` in `options`; 0 (the first choice, the default one) when it isn't there.
fn index_of<T: PartialEq>(options: &[T], value: &T) -> i32 {
    options
        .iter()
        .position(|option| option == value)
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(0)
}

/// The option at `index`, or `fallback` for a position outside the list.
fn pick<T: Copy>(options: &[T], index: i32, fallback: T) -> T {
    usize::try_from(index)
        .ok()
        .and_then(|index| options.get(index))
        .copied()
        .unwrap_or(fallback)
}

/// `bytes` a second as the MB/s the limit field shows: the shortest text that reads back as
/// exactly the same number of bytes, so that an unrelated edit never changes a stored limit.
fn megabytes(bytes: u64) -> String {
    (bytes as f64 / BYTES_PER_MB).to_string()
}

/// The form for `settings`. A stored value the window has no choice for (a retry count of 4, a
/// timeout of 45 s) shows as the nearest choice it has, or the first: the next edit stores that.
pub fn to_form(settings: &TransferSettings) -> SettingsFormData {
    let limit = settings.bandwidth_limit_bytes();
    let (proxy_index, proxy_url) = match &settings.proxy {
        ProxyMode::System => (PROXY_SYSTEM, String::new()),
        ProxyMode::None => (PROXY_NONE, String::new()),
        ProxyMode::Custom(url) => (PROXY_CUSTOM, url.clone()),
    };
    SettingsFormData {
        simultaneous: i32::try_from(settings.simultaneous)
            .unwrap_or(MAX_SIMULTANEOUS)
            .clamp(MIN_SIMULTANEOUS, MAX_SIMULTANEOUS),
        adaptive: settings.adaptive,
        retries_index: match settings.retries {
            0 => 2,
            1..=3 => 1,
            _ => 0,
        },
        bandwidth_on: limit.is_some(),
        bandwidth_mb: limit.map_or_else(|| SUGGESTED_LIMIT_MB.to_owned(), megabytes),
        protocol_index: index_of(&PROTOCOLS, &settings.protocol),
        keep_alive: settings.keep_alive,
        timeout_index: index_of(&TIMEOUTS, &settings.request_timeout_secs),
        proxy_index,
        proxy_url,
        verify: settings.verify_checksums,
        upload_conflicts_index: index_of(&UPLOAD_CONFLICTS, &settings.upload_conflicts),
        download_conflicts_index: index_of(&DOWNLOAD_CONFLICTS, &settings.download_conflicts),
    }
}

/// A speed typed in MB/s as bytes a second, or `None` when it isn't a speed above zero (nothing,
/// words, a negative number, "inf", "NaN", or something that rounds to no bytes at all, which
/// would mean no limit).
fn parse_limit(text: &str) -> Option<u64> {
    let mb: f64 = text.trim().parse().ok()?;
    let bytes = (mb * BYTES_PER_MB).round();
    (mb.is_finite() && bytes >= 1.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// The form checked and turned into settings. A refused form is `Err((field, message))`: the
/// field is [`FIELD_BANDWIDTH`] or [`FIELD_PROXY`], and the message is what to show under it.
///
/// - Simultaneous transfers are kept within 1 to 128.
/// - The limit must be a number of MB/s above 0 (only while the limit is on).
/// - A custom proxy needs an address, and the address may not hold a user name or password: any
///   `@` in it is refused (a password may hold a `/`, `?` or `#`, so no part of the address is
///   safe to tell apart), because settings are stored in a database that must never hold a secret.
///
/// The bandwidth check comes first when both are wrong.
pub fn from_form(f: &SettingsFormData) -> Result<TransferSettings, (String, String)> {
    let defaults = TransferSettings::default();

    let bandwidth_limit = if f.bandwidth_on {
        let bytes = parse_limit(&f.bandwidth_mb).ok_or_else(|| {
            (
                FIELD_BANDWIDTH.to_owned(),
                copy::BANDWIDTH_INVALID.to_owned(),
            )
        })?;
        Some(bytes)
    } else {
        None
    };

    let proxy = match f.proxy_index {
        PROXY_NONE => ProxyMode::None,
        PROXY_CUSTOM => {
            let url = f.proxy_url.trim();
            let refused = |message: &str| (FIELD_PROXY.to_owned(), message.to_owned());
            if url.is_empty() {
                return Err(refused(copy::PROXY_URL_MISSING));
            }
            if url.contains('@') {
                return Err(refused(copy::PROXY_HAS_PASSWORD));
            }
            ProxyMode::Custom(url.to_owned())
        }
        _ => ProxyMode::System,
    };

    Ok(TransferSettings {
        simultaneous: u32::try_from(f.simultaneous.clamp(MIN_SIMULTANEOUS, MAX_SIMULTANEOUS))
            .unwrap_or(defaults.simultaneous),
        adaptive: f.adaptive,
        retries: pick(&RETRIES, f.retries_index, defaults.retries),
        bandwidth_limit,
        protocol: pick(&PROTOCOLS, f.protocol_index, defaults.protocol),
        keep_alive: f.keep_alive,
        request_timeout_secs: pick(&TIMEOUTS, f.timeout_index, defaults.request_timeout_secs),
        proxy,
        verify_checksums: f.verify,
        upload_conflicts: pick(
            &UPLOAD_CONFLICTS,
            f.upload_conflicts_index,
            defaults.upload_conflicts,
        ),
        download_conflicts: pick(
            &DOWNLOAD_CONFLICTS,
            f.download_conflicts_index,
            defaults.download_conflicts,
        ),
    })
}

// ---- The window ----

/// Puts `settings` in the window's form and clears the messages under its fields; and lists the
/// account keys again if the Publish tab is the one shown.
pub fn show(window: &SettingsWindow, settings: &TransferSettings) {
    follow_look(window);
    show_form(window, &to_form(settings));
    if window.get_current_tab() == PUBLISH_TAB {
        keep_keys(window, None);
    }
}

/// Puts `form` in the window and clears the messages under its fields.
fn show_form(window: &SettingsWindow, form: &SettingsFormData) {
    window.set_simultaneous(form.simultaneous);
    window.set_adaptive(form.adaptive);
    window.set_retries_index(form.retries_index);
    window.set_bandwidth_on(form.bandwidth_on);
    window.set_bandwidth_mb(form.bandwidth_mb.as_str().into());
    window.set_protocol_index(form.protocol_index);
    window.set_keep_alive(form.keep_alive);
    window.set_timeout_index(form.timeout_index);
    window.set_proxy_index(form.proxy_index);
    window.set_proxy_url(form.proxy_url.as_str().into());
    window.set_verify(form.verify);
    window.set_upload_conflicts_index(form.upload_conflicts_index);
    window.set_download_conflicts_index(form.download_conflicts_index);
    clear_errors(window);
}

/// What the window holds now.
fn read_form(window: &SettingsWindow) -> SettingsFormData {
    SettingsFormData {
        simultaneous: window.get_simultaneous(),
        adaptive: window.get_adaptive(),
        retries_index: window.get_retries_index(),
        bandwidth_on: window.get_bandwidth_on(),
        bandwidth_mb: window.get_bandwidth_mb().to_string(),
        protocol_index: window.get_protocol_index(),
        keep_alive: window.get_keep_alive(),
        timeout_index: window.get_timeout_index(),
        proxy_index: window.get_proxy_index(),
        proxy_url: window.get_proxy_url().to_string(),
        verify: window.get_verify(),
        upload_conflicts_index: window.get_upload_conflicts_index(),
        download_conflicts_index: window.get_download_conflicts_index(),
    }
}

fn clear_errors(window: &SettingsWindow) {
    window.set_bandwidth_error(SharedString::new());
    window.set_proxy_error(SharedString::new());
}

/// Shows `message` under the field `field` names ([`from_form`]'s error), and nothing under the
/// other.
fn show_error(window: &SettingsWindow, field: &str, message: &str) {
    clear_errors(window);
    match field {
        FIELD_BANDWIDTH => window.set_bandwidth_error(message.into()),
        FIELD_PROXY => window.set_proxy_error(message.into()),
        _ => {}
    }
}

/// Connects the window's callbacks. Each edit is read back and checked: a refused form shows
/// its message under the field and changes nothing, an accepted one clears the messages and goes
/// to `apply`, which stores and uses the settings. Restore Defaults puts the default settings in
/// the form and goes to `apply` with them. The Publish tab's go to the [`KeyKeeper`].
pub fn wire(window: &SettingsWindow, apply: impl Fn(TransferSettings) + 'static) {
    wire_publish(window);
    let apply = Rc::new(apply);

    let weak = window.as_weak();
    let on_changed = Rc::clone(&apply);
    window.on_changed(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        match from_form(&read_form(&window)) {
            Ok(settings) => {
                clear_errors(&window);
                on_changed(settings);
            }
            Err((field, message)) => show_error(&window, &field, &message),
        }
    });

    let weak = window.as_weak();
    window.on_restore_defaults(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let defaults = TransferSettings::default();
        show_form(&window, &to_form(&defaults));
        apply(defaults);
    });
}

// ---- The Publish tab ----

/// The Publish tab's position in the row of tabs.
const PUBLISH_TAB: i32 = 4;

/// What the Publish tab does with the account keys: forgets the one given (its account id), if
/// any, then shows the keys kept ([`show_keys`]). Publish's; it works off the UI thread.
pub type KeyKeeper = Rc<dyn Fn(&SettingsWindow, Option<String>)>;

thread_local! {
    // The UI thread's: the Settings window lives there.
    static KEY_KEEPER: RefCell<Option<KeyKeeper>> = const { RefCell::new(None) };
}

/// Hands the Publish tab its [`KeyKeeper`]. Without one (the window's tests) the tab stays empty.
pub fn set_key_keeper(keeper: KeyKeeper) {
    KEY_KEEPER.with(|slot| *slot.borrow_mut() = Some(keeper));
}

fn keep_keys(window: &SettingsWindow, forget: Option<String>) {
    let keeper = KEY_KEEPER.with(|slot| slot.borrow().clone());
    if let Some(keeper) = keeper {
        keeper(window, forget);
    }
}

/// Lists `keys` in the Publish tab (`name` the account, `meta` a line about it, `id` its account
/// id), with `error` under them: what went wrong forgetting one, empty for nothing.
pub fn show_keys(window: &SettingsWindow, keys: Vec<ProfileItem>, error: &str) {
    window.set_bunny_keys(ModelRc::new(VecModel::from(keys)));
    window.set_bunny_keys_error(error.into());
}

/// Opening the Publish tab lists the keys; Forget Key forgets one and lists them again.
fn wire_publish(window: &SettingsWindow) {
    let weak = window.as_weak();
    window.on_publish_shown(move || {
        if let Some(window) = weak.upgrade() {
            keep_keys(&window, None);
        }
    });
    let weak = window.as_weak();
    window.on_forget_key(move |id| {
        if let Some(window) = weak.upgrade() {
            keep_keys(&window, Some(id.to_string()));
        }
    });
}

// ---- Windows' text size and "Animation effects" (§9.1) ----

/// What a window's `Theme` follows from Windows: the text size (Settings › Accessibility › Text
/// size, 1.0 to 2.25) and whether to animate ("Animation effects").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub text_scale: f32,
    pub motion: bool,
}

impl Look {
    /// What `Theme` assumes when told nothing: 100 %, with animations.
    pub const PLAIN: Look = Look {
        text_scale: 1.0,
        motion: true,
    };

    /// As Windows has them now. The tests' windows keep [`Look::PLAIN`], whatever the PC that
    /// runs them is set to, so that their sizes are the same everywhere.
    pub fn now() -> Self {
        if cfg!(test) {
            Self::PLAIN
        } else {
            Self {
                text_scale: httporter_platform::win::text_scale(),
                motion: httporter_platform::win::animations_enabled(),
            }
        }
    }

    fn put(self, theme: Theme<'_>) {
        theme.set_text_scale(self.text_scale);
        theme.set_motion(self.motion);
    }
}

/// The windows that follow the [`Look`], and the one they were last given.
#[derive(Default)]
struct Looks {
    shown: Option<Look>,
    main: Option<slint::Weak<AppWindow>>,
    settings: Option<slint::Weak<SettingsWindow>>,
}

thread_local! {
    // The UI thread's: the windows live there.
    static LOOKS: RefCell<Looks> = RefCell::default();
}

/// Makes the main window follow Windows' text size and "Animation effects" from now on.
pub fn install_look(ui: &AppWindow) {
    let look = Look::now();
    look.put(ui.global::<Theme>());
    LOOKS.with(|looks| {
        let mut looks = looks.borrow_mut();
        looks.shown = Some(look);
        looks.main = Some(ui.as_weak());
    });
}

/// The Settings window follows the same [`Look`] as the main window (its `Theme` is its own).
fn follow_look(window: &SettingsWindow) {
    let look = LOOKS.with(|looks| {
        let mut looks = looks.borrow_mut();
        looks.settings = Some(window.as_weak());
        looks.shown
    });
    look.unwrap_or_else(Look::now).put(window.global::<Theme>());
}

/// Windows may have changed its text size or "Animation effects" (its settings changed, or the
/// window came back to the front): both are read again, and if either changed, every window's
/// `Theme` gets them.
pub fn system_settings_changed() {
    let look = Look::now();
    LOOKS.with(|looks| {
        let mut looks = looks.borrow_mut();
        if looks.shown == Some(look) {
            return;
        }
        looks.shown = Some(look);
        if let Some(ui) = looks.main.as_ref().and_then(slint::Weak::upgrade) {
            look.put(ui.global::<Theme>());
        }
        if let Some(window) = looks.settings.as_ref().and_then(slint::Weak::upgrade) {
            look.put(window.global::<Theme>());
        }
    });
}

#[cfg(test)]
#[path = "settings_view_tests.rs"]
mod tests;
