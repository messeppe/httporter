//! Tests of the Settings window's Transfers form: `to_form` and `from_form` (round trips, the
//! checks, Ruling Q19's refusal of a proxy password), and the window itself, headless (its tabs,
//! Restore Defaults, and `wire`).

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};

use super::*;

fn settings() -> TransferSettings {
    TransferSettings::default()
}

#[test]
fn form_round_trips_defaults() {
    let form = to_form(&settings());
    assert_eq!(
        form,
        SettingsFormData {
            simultaneous: 32,
            adaptive: true,
            retries_index: 0,
            bandwidth_on: false,
            // What the field offers while the limit is off (the mockup's 10 MB/s).
            bandwidth_mb: "10".into(),
            protocol_index: 0,
            keep_alive: true,
            timeout_index: 0,
            proxy_index: 0,
            proxy_url: String::new(),
            verify: true,
            upload_conflicts_index: 0,
            download_conflicts_index: 0,
        }
    );
    assert_eq!(from_form(&form), Ok(settings()));
}

#[test]
fn a_fully_custom_form_round_trips() {
    let custom = TransferSettings {
        simultaneous: 96,
        adaptive: false,
        retries: 0,
        bandwidth_limit: Some(2_621_440),
        protocol: HttpProtocol::Http1Only,
        keep_alive: false,
        request_timeout_secs: 300,
        proxy: ProxyMode::Custom("http://proxy:8080".into()),
        verify_checksums: false,
        upload_conflicts: ConflictPolicy::KeepBoth,
        download_conflicts: ConflictPolicy::KeepBoth,
    };
    let form = to_form(&custom);
    assert_eq!(
        (form.retries_index, form.protocol_index, form.timeout_index),
        (2, 1, 2)
    );
    assert_eq!(
        (form.proxy_index, form.proxy_url.as_str()),
        (2, "http://proxy:8080")
    );
    assert_eq!(
        (form.bandwidth_on, form.bandwidth_mb.as_str()),
        (true, "2.5")
    );
    // The two conflict lists are in different orders: Keep both is 4 for uploads, 2 for
    // downloads; Skip is the other way round.
    assert_eq!(
        (form.upload_conflicts_index, form.download_conflicts_index),
        (4, 2)
    );
    assert_eq!(from_form(&form), Ok(custom.clone()));

    let skip = TransferSettings {
        upload_conflicts: ConflictPolicy::Skip,
        download_conflicts: ConflictPolicy::Skip,
        retries: 3,
        request_timeout_secs: 60,
        proxy: ProxyMode::None,
        ..custom
    };
    let form = to_form(&skip);
    assert_eq!(
        (form.upload_conflicts_index, form.download_conflicts_index),
        (2, 4)
    );
    assert_eq!(
        (form.retries_index, form.timeout_index, form.proxy_index),
        (1, 1, 1)
    );
    assert_eq!(from_form(&form), Ok(skip));
    for (policy, upload, download) in [
        (ConflictPolicy::Ask, 0, 0),
        (ConflictPolicy::ReplaceIfDifferent, 1, 1),
        (ConflictPolicy::Replace, 3, 3),
    ] {
        let s = TransferSettings {
            upload_conflicts: policy,
            download_conflicts: policy,
            ..settings()
        };
        let form = to_form(&s);
        assert_eq!(
            (form.upload_conflicts_index, form.download_conflicts_index),
            (upload, download),
            "{policy:?}"
        );
        assert_eq!(from_form(&form), Ok(s));
    }
}

#[test]
fn an_odd_stored_limit_comes_back_to_the_byte() {
    for bytes in [5_000_000, 1, 1_048_577, 123_456_789] {
        let s = TransferSettings {
            bandwidth_limit: Some(bytes),
            ..settings()
        };
        assert_eq!(
            from_form(&to_form(&s)).unwrap().bandwidth_limit,
            Some(bytes),
            "{bytes}"
        );
    }
    // A stored 0 means no limit.
    let zero = TransferSettings {
        bandwidth_limit: Some(0),
        ..settings()
    };
    assert!(!to_form(&zero).bandwidth_on);
}

fn with_limit(text: &str) -> SettingsFormData {
    SettingsFormData {
        bandwidth_on: true,
        bandwidth_mb: text.into(),
        ..to_form(&settings())
    }
}

#[test]
fn bandwidth_must_be_a_positive_number() {
    let refused = Err((
        "bandwidth".to_owned(),
        "Enter a speed above 0 MB/s.".to_owned(),
    ));
    assert_eq!(from_form(&with_limit("0")), refused);
    assert_eq!(
        from_form(&with_limit("2.5")).unwrap().bandwidth_limit,
        Some(2_621_440)
    );
    assert_eq!(
        from_form(&with_limit(" 10 ")).unwrap().bandwidth_limit,
        Some(10_485_760)
    );
    for text in ["", "abc", "-1", "inf", "NaN", "1e-9", "2,5", "1e30"] {
        assert_eq!(from_form(&with_limit(text)), refused, "{text:?}");
    }
    // While the limit is off, the text is not read.
    let off = SettingsFormData {
        bandwidth_on: false,
        ..with_limit("abc")
    };
    assert_eq!(from_form(&off).unwrap().bandwidth_limit, None);
}

fn with_proxy(url: &str) -> SettingsFormData {
    SettingsFormData {
        proxy_index: 2,
        proxy_url: url.into(),
        ..to_form(&settings())
    }
}

#[test]
fn custom_proxy_needs_a_url() {
    let refused = Err((
        "proxy".to_owned(),
        "Enter a proxy address such as http://proxy:8080.".to_owned(),
    ));
    assert_eq!(from_form(&with_proxy("")), refused);
    assert_eq!(from_form(&with_proxy("   ")), refused);
    assert_eq!(
        from_form(&with_proxy(" http://proxy:8080 ")).unwrap().proxy,
        ProxyMode::Custom("http://proxy:8080".into())
    );
    // The address of a proxy not chosen is not read.
    let system = SettingsFormData {
        proxy_index: 0,
        ..with_proxy("")
    };
    assert_eq!(from_form(&system).unwrap().proxy, ProxyMode::System);
    let none = SettingsFormData {
        proxy_index: 1,
        ..with_proxy("")
    };
    assert_eq!(from_form(&none).unwrap().proxy, ProxyMode::None);
    // A bad bandwidth is said first when both are wrong.
    let both = SettingsFormData {
        bandwidth_on: true,
        bandwidth_mb: "0".into(),
        ..with_proxy("")
    };
    assert_eq!(from_form(&both).unwrap_err().0, "bandwidth");
}

/// Ruling Q19: a proxy address with a user name or password is refused with the exact copy, and
/// never shown by the form's `Debug`.
#[test]
fn a_proxy_address_with_credentials_is_refused() {
    let refused = Err((
        "proxy".to_owned(),
        "HTTPorter can't store a proxy password yet. Use a proxy without one, or Use Windows \
         settings."
            .to_owned(),
    ));
    for url in [
        "user:hunter2@host",
        "http://user:hunter2@host:8080",
        "http://:hunter2@host",
        "http://user:hun/ter2@host",
        "http://user:hun?ter2@host",
        "http://user:hun#ter2@host",
        "http://user@host",
    ] {
        assert_eq!(from_form(&with_proxy(url)), refused, "{url}");
        let shown = format!("{:?} {:#?}", with_proxy(url), with_proxy(url));
        assert!(
            !shown.contains("hunter2") && !shown.contains("hun"),
            "{shown}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
    }
    // An address without one is shown as it is.
    assert!(format!("{:?}", with_proxy("http://proxy:8080")).contains("http://proxy:8080"));
}

#[test]
fn simultaneous_clamped_1_to_128() {
    let with = |n: i32| {
        from_form(&SettingsFormData {
            simultaneous: n,
            ..to_form(&settings())
        })
        .unwrap()
        .simultaneous
    };
    assert_eq!(with(0), 1);
    assert_eq!(with(-5), 1);
    assert_eq!(with(i32::MIN), 1);
    assert_eq!(with(1), 1);
    assert_eq!(with(96), 96);
    assert_eq!(with(128), 128);
    assert_eq!(with(200), 128);
    assert_eq!(with(i32::MAX), 128);
    // A stored number outside the range shows as the nearest the window has.
    let shown = |n: u32| {
        to_form(&TransferSettings {
            simultaneous: n,
            ..settings()
        })
        .simultaneous
    };
    assert_eq!((shown(0), shown(999), shown(u32::MAX)), (1, 128, 128));
}

#[test]
fn indices_outside_the_lists_fall_back_to_the_defaults() {
    let form = SettingsFormData {
        retries_index: 7,
        protocol_index: -1,
        timeout_index: 3,
        proxy_index: 9,
        upload_conflicts_index: 5,
        download_conflicts_index: -2,
        ..to_form(&settings())
    };
    assert_eq!(from_form(&form), Ok(settings()));
    // Stored values the window has no choice for show as the nearest one, or the first.
    let odd = |retries: u8, timeout: u32| {
        let form = to_form(&TransferSettings {
            retries,
            request_timeout_secs: timeout,
            ..settings()
        });
        (form.retries_index, form.timeout_index)
    };
    assert_eq!(odd(4, 45), (0, 0));
    assert_eq!(odd(1, 120), (1, 0));
    assert_eq!(odd(2, 300), (1, 2));
    assert_eq!(odd(9, 60), (0, 1));
}

// ---- The window ----

fn window() -> SettingsWindow {
    thread_local! {
        static PLATFORM_SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !PLATFORM_SET.replace(true) {
        testing::init_no_event_loop();
    }
    let w = SettingsWindow::new().unwrap();
    w.show().unwrap();
    w
}

fn count(w: &SettingsWindow, label: &str) -> usize {
    ElementHandle::find_by_accessible_label(w, label).count()
}

fn find(w: &SettingsWindow, role: AccessibleRole, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} {label:?}"))
}

fn press(w: &SettingsWindow, role: AccessibleRole, label: &str) {
    find(w, role, label).invoke_accessible_default_action();
    slint::platform::update_timers_and_animations();
}

const NOTE: &str = "These settings arrive in a later version.";
/// Controls only the Transfers tab has.
const TRANSFERS_CONTROLS: [&str; 5] = [
    "Fewer",
    "Retries",
    "Restore Defaults",
    "Verify uploads",
    "Simultaneous transfers",
];

#[test]
fn only_transfers_tab_has_controls() {
    let w = window();
    // It opens on Transfers.
    assert_eq!(w.get_current_tab(), 2);
    assert_eq!(
        find(&w, AccessibleRole::Tab, "Transfers").accessible_item_selected(),
        Some(true)
    );
    assert_eq!(count(&w, NOTE), 0);
    for control in TRANSFERS_CONTROLS {
        assert!(count(&w, control) > 0, "{control}");
    }
    // Publish has its own controls (the account keys, I3): no note there.
    press(&w, AccessibleRole::Tab, "Publish");
    assert_eq!(count(&w, NOTE), 0);
    for control in TRANSFERS_CONTROLS {
        assert_eq!(count(&w, control), 0, "Publish: {control}");
    }
    for tab in ["General", "Sites", "Security", "Advanced"] {
        let button = find(&w, AccessibleRole::Tab, tab);
        // Every tab can be chosen, so the note can be read.
        assert_ne!(button.accessible_enabled(), Some(false), "{tab}");
        press(&w, AccessibleRole::Tab, tab);
        // Only Security is still to come (I5); General, Sites and Advanced have their own (I4).
        let note = usize::from(tab == "Security");
        assert_eq!(count(&w, NOTE), note, "{tab}");
        for control in TRANSFERS_CONTROLS {
            assert_eq!(count(&w, control), 0, "{tab}: {control}");
        }
        assert_eq!(
            find(&w, AccessibleRole::Tab, tab).accessible_item_selected(),
            Some(true)
        );
        assert!(
            ElementHandle::find_by_accessible_label(&w, tab)
                .any(|e| e.accessible_role() == Some(AccessibleRole::TabPanel)),
            "{tab}: the panel is named after its tab"
        );
    }
    press(&w, AccessibleRole::Tab, "Transfers");
    assert_eq!(count(&w, NOTE), 0);
    assert!(count(&w, "Restore Defaults") > 0);
}

#[test]
fn the_transfers_tab_shows_the_mockups_words() {
    let w = window();
    for text in [
        "Simultaneous transfers",
        "Per site. Bunny Storage allows up to 96 from one computer; S3 services up to 128.",
        "Slow down when a server asks",
        "On HTTP 429 or 503, halve parallel transfers, then ramp back up.",
        "Retry failed transfers",
        "Waits 1, 2, 4, 8 and 10 seconds between attempts.",
        "Bandwidth limit",
        "Applies to all transfers together.",
        "Protocol",
        "Every connection is encrypted with TLS 1.2 or newer.",
        "Keep connections open between files",
        "Saves a TLS handshake per file. Strongly recommended.",
        "Request timeout",
        "Proxy",
        "Verify every upload with a checksum",
        "SHA-256 on Bunny Storage, MD5 on S3-compatible services. The server rejects damaged files.",
        "When a file already exists",
        "One choice per batch, never one dialog per file.",
    ] {
        assert!(count(&w, text) > 0, "{text}");
    }
    let value = |label: &str| {
        find(&w, AccessibleRole::Combobox, label)
            .accessible_value()
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(value("Retries"), "5 times");
    assert_eq!(value("HTTP version"), "HTTP/2 when available");
    assert_eq!(value("Request timeout"), "120 seconds");
    assert_eq!(value("Proxy"), "Use Windows settings");
    assert_eq!(value("When uploading"), "Uploads: Ask once per batch");
    assert_eq!(value("When downloading"), "Downloads: Ask once per batch");
    // The proxy address is only there for a custom proxy.
    assert_eq!(count(&w, "Proxy address"), 0);
    w.set_proxy_index(2);
    slint::platform::update_timers_and_animations();
    assert_eq!(count(&w, "Proxy address"), 1);
}

#[test]
fn restore_defaults_fires() {
    let w = window();
    let pressed = Rc::new(RefCell::new(0));
    w.on_restore_defaults({
        let pressed = pressed.clone();
        move || *pressed.borrow_mut() += 1
    });
    press(&w, AccessibleRole::Button, "Restore Defaults");
    assert_eq!(*pressed.borrow(), 1);
}

#[test]
fn wire_applies_good_edits_shows_errors_and_restores_defaults() {
    let w = window();
    let applied: Rc<RefCell<Vec<TransferSettings>>> = Rc::default();
    wire(&w, {
        let applied = applied.clone();
        move |s| applied.borrow_mut().push(s)
    });
    show(&w, &settings());

    // An edit with a good form goes to `apply`.
    press(&w, AccessibleRole::Button, "More");
    assert_eq!(w.get_simultaneous(), 33);
    press(&w, AccessibleRole::Switch, "Verify uploads");
    assert_eq!(applied.borrow().len(), 2);
    assert_eq!(applied.borrow()[0].simultaneous, 33);
    assert!(!applied.borrow()[1].verify_checksums);

    // A bad bandwidth shows its message and changes nothing.
    w.set_bandwidth_on(true);
    w.set_bandwidth_mb("0".into());
    w.invoke_changed();
    assert_eq!(w.get_bandwidth_error(), "Enter a speed above 0 MB/s.");
    assert_eq!(w.get_proxy_error(), "");
    assert_eq!(applied.borrow().len(), 2);
    // So does a custom proxy with a password, under the proxy row.
    w.set_bandwidth_mb("2.5".into());
    w.set_proxy_index(2);
    w.set_proxy_url("http://u:secret@proxy:8080".into());
    w.invoke_changed();
    assert_eq!(w.get_bandwidth_error(), "");
    assert_eq!(
        w.get_proxy_error(),
        "HTTPorter can't store a proxy password yet. Use a proxy without one, or Use Windows \
         settings."
    );
    assert_eq!(applied.borrow().len(), 2);
    // Fixed, the form applies and the messages go.
    w.set_proxy_url("http://proxy:8080".into());
    w.invoke_changed();
    assert_eq!(
        (w.get_bandwidth_error(), w.get_proxy_error()),
        ("".into(), "".into())
    );
    let last = applied.borrow().last().cloned().unwrap();
    assert_eq!(last.bandwidth_limit, Some(2_621_440));
    assert_eq!(last.proxy, ProxyMode::Custom("http://proxy:8080".into()));

    // Restore Defaults puts the defaults in the form and applies them.
    w.set_bandwidth_error("stale".into());
    press(&w, AccessibleRole::Button, "Restore Defaults");
    assert_eq!(applied.borrow().last(), Some(&settings()));
    assert_eq!(w.get_simultaneous(), 32);
    assert!(!w.get_bandwidth_on() && w.get_verify());
    assert_eq!(
        (w.get_proxy_index(), w.get_bandwidth_error()),
        (0, "".into())
    );
}

#[test]
fn the_slider_and_the_segments_follow_values_set_by_the_app() {
    let w = window();
    // The user moves them first …
    press(&w, AccessibleRole::Button, "Fewer");
    press(&w, AccessibleRole::RadioButton, "Limit to");
    assert_eq!((w.get_simultaneous(), w.get_bandwidth_on()), (31, true));
    // … then the app sets the form, and they follow.
    show(
        &w,
        &TransferSettings {
            simultaneous: 96,
            ..settings()
        },
    );
    slint::platform::update_timers_and_animations();
    let slider = find(&w, AccessibleRole::Slider, "Simultaneous transfers");
    assert_eq!(slider.accessible_value().as_deref(), Some("96"));
    assert_eq!(
        find(&w, AccessibleRole::RadioButton, "Off").accessible_checked(),
        Some(true)
    );
    assert_eq!(
        find(&w, AccessibleRole::RadioButton, "Limit to").accessible_checked(),
        Some(false)
    );
}
