//! Toast notifications.
//!
//! A desktop program that is not installed through an MSIX package shows toasts under an
//! "app user model id" (AUMID) it registers itself: a per-user registry key, so no admin
//! rights are needed. The same id is given to the process, so the toast, the taskbar button
//! and the notification settings page all show one HTTPorter.

use std::iter::once;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{
    NotificationSetting, ToastNotification, ToastNotificationManager,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::core::HSTRING;

use super::{flash, with_com};

/// The id HTTPorter's notifications (and process) are known by.
const AUMID: &str = "HTTPorter.App";

/// Where the id is registered, relative to `HKEY_CURRENT_USER`.
const AUMID_KEY: &str = r"Software\Classes\AppUserModelId\HTTPorter.App";

/// The values stored under [`AUMID_KEY`] (all `REG_SZ`): the name the notification shows,
/// and the colour (ARGB hex, the accent blue) behind its icon.
fn aumid_values() -> [(&'static str, &'static str); 2] {
    [
        ("DisplayName", "HTTPorter"),
        ("IconBackgroundColor", "FF0A84FF"),
    ]
}

/// `text` made safe to put between XML tags or in an attribute. Characters XML 1.0 can't
/// hold at all (control characters other than tab, newline and carriage return, and
/// U+FFFE/U+FFFF) are dropped: one in a file name would otherwise make the whole toast fail.
fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// The toast: a title line and a body line, in the system's standard layout.
fn toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\">\
         <text>{}</text><text>{}</text>\
         </binding></visual></toast>",
        escape_xml(title),
        escape_xml(body)
    )
}

/// Registers HTTPorter's notification identity (the app user model id) and gives it to the
/// process.
///
/// Call it once at startup, before any window is shown: Windows reads the process id when
/// it creates a window's taskbar button, so setting it later can leave the button grouped
/// under the old id. [`notify`] calls it too, so a program that skips the startup call still
/// gets its toasts, just with the late id.
///
/// It is idempotent: only the first successful call per process does anything, and writing
/// the same registry values again on every start repairs a key that was deleted or edited.
/// The identity only matters for notifications and taskbar grouping, so a caller may ignore
/// the `Err` (the next [`notify`] retries and reports the same failure).
pub fn ensure_app_identity() -> Result<(), String> {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.load(Ordering::Acquire) {
        return Ok(());
    }

    let key = HSTRING::from(AUMID_KEY);
    for (name, value) in aumid_values() {
        let data: Vec<u16> = value.encode_utf16().chain(once(0)).collect();
        // SAFETY: `data` outlives the call and the length passed is its size in bytes.
        // The call creates the key if it is missing.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &HSTRING::from(name),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * size_of::<u16>()) as u32,
            )
        }
        .ok()
        .map_err(|e| format!("can't register the notification identity ({name}): {e}"))?;
    }

    // SAFETY: plain call with a valid string.
    unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID)) }
        .map_err(|e| format!("can't set the program's notification identity: {e}"))?;

    DONE.store(true, Ordering::Release);
    Ok(())
}

fn why_not_shown(setting: NotificationSetting) -> &'static str {
    match setting {
        NotificationSetting::DisabledForApplication => "notifications are turned off for HTTPorter",
        NotificationSetting::DisabledForUser => "notifications are turned off for this user",
        NotificationSetting::DisabledByGroupPolicy => {
            "notifications are turned off by group policy"
        }
        NotificationSetting::DisabledByManifest => "notifications are turned off by the manifest",
        _ => "notifications are not available",
    }
}

/// Shows a toast notification (title and one line of text) from HTTPorter.
///
/// The error says why nothing was shown: the identity couldn't be registered, the system
/// refused, or the user switched HTTPorter's notifications off. The caller then falls back
/// to [`flash`]; [`notify_or_flash`] does both.
pub fn notify(title: &str, body: &str) -> Result<(), String> {
    ensure_app_identity()?;
    let xml = HSTRING::from(toast_xml(title, body));
    let setting = with_com(|| {
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
        let setting = notifier.Setting()?;
        if setting == NotificationSetting::Enabled {
            let doc = XmlDocument::new()?;
            doc.LoadXml(&xml)?;
            notifier.Show(&ToastNotification::CreateToastNotification(&doc)?)?;
        }
        Ok(setting)
    })
    .map_err(|e| format!("can't show the notification: {e}"))?;
    match setting {
        NotificationSetting::Enabled => Ok(()),
        other => Err(why_not_shown(other).to_owned()),
    }
}

/// [`notify`], and if that fails for any reason, [`flash`]es the window `hwnd` instead so
/// the user still gets a nudge.
pub fn notify_or_flash(hwnd: isize, title: &str, body: &str) {
    if notify(title, body).is_err() {
        flash(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_xml_escapes_text() {
        let xml = toast_xml(r#"A & B <c>"#, r#"say "hi" it's"#);
        assert_eq!(
            xml,
            "<toast><visual><binding template=\"ToastGeneric\">\
             <text>A &amp; B &lt;c&gt;</text>\
             <text>say &quot;hi&quot; it&apos;s</text>\
             </binding></visual></toast>"
        );
    }

    #[test]
    fn notification_xml_drops_characters_xml_cannot_hold() {
        let xml = toast_xml("a\u{0}b\u{8}c\u{FFFE}d", "tab\there\nnew\r");
        assert!(xml.contains("<text>abcd</text>"));
        assert!(xml.contains("<text>tab\there\nnew\r</text>"));
    }

    #[test]
    fn notification_xml_keeps_unicode() {
        let xml = toast_xml("Загружено", "файлов: 3 ✓");
        assert!(xml.contains("<text>Загружено</text>"));
        assert!(xml.contains("<text>файлов: 3 ✓</text>"));
    }

    #[test]
    fn aumid_registration_values() {
        assert_eq!(
            aumid_values(),
            [
                ("DisplayName", "HTTPorter"),
                ("IconBackgroundColor", "FF0A84FF")
            ]
        );
        // The key and the id the toast is shown under must agree.
        assert!(AUMID_KEY.ends_with(AUMID));
        assert_eq!(AUMID, "HTTPorter.App");
    }
}
