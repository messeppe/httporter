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

use std::fmt;
use std::rc::Rc;

use httporter_core::{ConflictPolicy, HttpProtocol, ProxyMode, TransferSettings};
use slint::{ComponentHandle, SharedString};

use crate::{SettingsWindow, copy};

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

/// Puts `settings` in the window's form and clears the messages under its fields.
pub fn show(window: &SettingsWindow, settings: &TransferSettings) {
    show_form(window, &to_form(settings));
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

/// Connects the window's two callbacks. Each edit is read back and checked: a refused form shows
/// its message under the field and changes nothing, an accepted one clears the messages and goes
/// to `apply`, which stores and uses the settings. Restore Defaults puts the default settings in
/// the form and goes to `apply` with them.
pub fn wire(window: &SettingsWindow, apply: impl Fn(TransferSettings) + 'static) {
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

#[cfg(test)]
#[path = "settings_view_tests.rs"]
mod tests;
