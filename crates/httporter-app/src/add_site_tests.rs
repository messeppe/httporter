// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Add Site sheet and the sidebar's site list, driven headlessly with the testing backend:
//! what the sheet reports, how it behaves as a modal sheet (focus, Escape, Return, the window
//! behind) and how it presents itself to assistive technology.

use crate::add_site::{FieldError, show_field_error};
use crate::keyboard_tests::{center, click, let_motion_finish, site, tab, tap};
use crate::pane_tests::row_starting;
use crate::{AppWindow, ProviderTile};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleLiveness, AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalSize, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;

// Tab order inside the open sheet, from the Storage zone field where the focus starts. Since
// Task 21 the password field has an eye button, and the provider grid and the "Sign in with"
// switch come first in the sheet: Tab from the last button wraps to the grid.
const ZONE: usize = 0;
const REGION: usize = 1;
const PASSWORD: usize = 2;
const EYE: usize = 3;
const NAME: usize = 4;
const TEST: usize = 5;
const CANCEL: usize = 6;
const ADD: usize = 7;
const GRID: usize = 8;
// 9: the "Sign in with" switch.
const STOPS: usize = 10;

/// A window with two sites and three regions, logging what the app is told.
struct Bed {
    w: AppWindow,
    log: Rc<RefCell<Vec<String>>>,
}

fn bed() -> Bed {
    // The platform can be set once per thread; a test may need several windows.
    thread_local! {
        static PLATFORM_SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !PLATFORM_SET.replace(true) {
        testing::init_no_event_loop();
    }
    let w = AppWindow::new().unwrap();
    w.set_sites(ModelRc::new(VecModel::from(vec![
        site("example-shop.com", ProviderTile::Bunny, "B"),
        site("media-archive", ProviderTile::R2, "R2"),
    ])));
    let regions: Vec<SharedString> = ["Frankfurt", "Singapore", "London"]
        .into_iter()
        .map(Into::into)
        .collect();
    w.set_region_names(ModelRc::new(VecModel::from(regions)));
    let log = Rc::new(RefCell::new(Vec::new()));
    w.on_add_site({
        let log = log.clone();
        move || log.borrow_mut().push("add-site".into())
    });
    w.on_site_selected({
        let log = log.clone();
        move |i| log.borrow_mut().push(format!("site-selected {i}"))
    });
    w.on_open_settings({
        let log = log.clone();
        move || log.borrow_mut().push("settings".into())
    });
    w.on_add_site_changed({
        let log = log.clone();
        move |zone, region| log.borrow_mut().push(format!("changed {zone}|{region}"))
    });
    w.on_add_site_test({
        let log = log.clone();
        move |name, zone, region, pw| {
            log.borrow_mut()
                .push(format!("test {name}|{zone}|{region}|{pw}"))
        }
    });
    w.on_add_site_save({
        let log = log.clone();
        move |name, zone, region, pw| {
            log.borrow_mut()
                .push(format!("save {name}|{zone}|{region}|{pw}"))
        }
    });
    w.on_add_site_cancel({
        let log = log.clone();
        move || log.borrow_mut().push("cancel".into())
    });
    log_round_two(&w, &log);
    w.show().unwrap();
    Bed { w, log }
}

/// Logs the callbacks of the second round of Add Site (Task 21) and of the Welcome screen.
fn log_round_two(w: &AppWindow, log: &Rc<RefCell<Vec<String>>>) {
    w.on_add_site_provider({
        let log = log.clone();
        move |i| log.borrow_mut().push(format!("provider {i}"))
    });
    w.on_add_site_sign_in_mode({
        let log = log.clone();
        move |i| log.borrow_mut().push(format!("sign-in {i}"))
    });
    w.on_add_site_reveal({
        let log = log.clone();
        move || log.borrow_mut().push("reveal".into())
    });
    w.on_add_site_find_zones({
        let log = log.clone();
        move |key| log.borrow_mut().push(format!("find-zones {key}"))
    });
    w.on_add_site_zones_chosen({
        let log = log.clone();
        move |checked, keep| {
            let checked: Vec<bool> = slint::Model::iter(&checked).collect();
            log.borrow_mut().push(format!("zones {checked:?} {keep}"))
        }
    });
    w.on_add_site_save_s3({
        let log = log.clone();
        move |draft, secret| {
            log.borrow_mut().push(format!(
                "save-s3 {}|{}|{}|{}|{}|{}|{}",
                draft.provider,
                draft.region,
                draft.account_id,
                draft.endpoint,
                draft.bucket,
                draft.key_id,
                secret
            ))
        }
    });
    w.on_welcome_add({
        let log = log.clone();
        move || log.borrow_mut().push("welcome-add".into())
    });
    w.on_welcome_import({
        let log = log.clone();
        move || log.borrow_mut().push("welcome-import".into())
    });
}

impl Bed {
    /// Opens the sheet the way the app does, from code.
    fn open(&self) {
        self.w.set_add_site_open(true);
        slint::platform::update_timers_and_animations();
        // The sheet comes in (I7) before it is clicked at.
        let_motion_finish();
        // The sheet is built on the next pass over the window's items; looking for it does that.
        assert!(self.is_open());
    }

    /// Everything the app was told since the last call.
    fn told(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.borrow_mut())
    }

    fn tap(&self, key: impl Into<SharedString>) {
        tap(self.w.window(), key);
        // Escape and Cancel close the sheet after its exit (I7).
        let_motion_finish();
    }

    fn type_text(&self, text: &str) {
        for ch in text.chars() {
            self.tap(ch.to_string());
        }
    }

    fn tab(&self, times: usize) {
        tab(&self.w, times);
    }

    fn backtab(&self, times: usize) {
        for _ in 0..times {
            self.tap(Key::Backtab);
        }
    }

    /// True when the sheet is on screen.
    fn is_open(&self) -> bool {
        ElementQuery::from_root(&self.w)
            .match_accessible_role(AccessibleRole::Form)
            .find_first()
            .is_some()
    }

    fn sheet(&self) -> ElementHandle {
        ElementQuery::from_root(&self.w)
            .match_accessible_role(AccessibleRole::Form)
            .find_first()
            .expect("the sheet is not open")
    }

    /// The control with this role and accessible label on the sheet.
    fn control(&self, role: AccessibleRole, label: &str) -> ElementHandle {
        let wanted = label.to_owned();
        self.sheet()
            .query_descendants()
            .match_accessible_role(role)
            .match_predicate(move |e| e.accessible_label().is_some_and(|l| l.as_str() == wanted))
            .find_first()
            .unwrap_or_else(|| panic!("no {role:?} labelled {label:?} on the sheet"))
    }

    fn button(&self, label: &str) -> ElementHandle {
        self.control(AccessibleRole::Button, label)
    }

    /// What a text field holds, as assistive technology sees it.
    fn value(&self, label: &str) -> String {
        self.control(AccessibleRole::TextInput, label)
            .accessible_value()
            .unwrap_or_default()
            .to_string()
    }

    /// How many elements on the sheet carry exactly this accessible label.
    fn labelled(&self, label: &str) -> usize {
        let wanted = label.to_owned();
        self.sheet()
            .query_descendants()
            .match_predicate(move |e| e.accessible_label().is_some_and(|l| l.as_str() == wanted))
            .find_all()
            .len()
    }

    /// Opens the sheet, fills the four fields through the keyboard and leaves the focus on the
    /// Storage zone field again.
    fn fill(&self, name: &str, zone: &str, region_steps: usize, password: &str) {
        self.type_text(zone);
        self.tab(1);
        for _ in 0..region_steps {
            self.tap(Key::DownArrow);
        }
        self.tab(1);
        self.type_text(password);
        self.tab(NAME - PASSWORD);
        self.type_text(name);
        self.tab(STOPS - NAME);
    }
}

// ---- Opening and closing ----

#[test]
fn the_sheet_is_closed_until_asked_for() {
    let b = bed();
    assert!(!b.is_open());
    assert!(!b.w.get_add_site_open());
}

#[test]
fn the_sidebar_button_opens_the_sheet_and_still_tells_the_app() {
    let b = bed();
    // Toggle, the toolbar's View group (Two panes, Single pane, Synchronized browsing), the site
    // list (one stop, ledger 155), then Add Site.
    b.tab(6);
    b.tap(Key::Return);
    assert!(b.w.get_add_site_open() && b.is_open());
    assert_eq!(b.told()[0], "add-site");
}

#[test]
fn the_app_can_open_and_close_the_sheet() {
    let b = bed();
    b.open();
    assert!(b.is_open());
    b.w.set_add_site_open(false);
    slint::platform::update_timers_and_animations();
    assert!(!b.is_open());
}

#[test]
fn opening_puts_the_focus_in_the_storage_zone_field() {
    let b = bed();
    b.open();
    b.type_text("zone-a");
    assert_eq!(b.value("Storage zone"), "zone-a");
    assert_eq!(b.value("Site name"), "");
}

#[test]
fn opening_from_the_keyboard_puts_the_focus_in_the_storage_zone_field_too() {
    let b = bed();
    // Toolbar (4 stops), the site list (one stop, ledger 155), then Add Site.
    b.tab(6);
    b.tap(Key::Return);
    b.type_text("zone-b");
    assert_eq!(b.value("Storage zone"), "zone-b");
}

#[test]
fn opening_reports_the_starting_endpoint_inputs() {
    let b = bed();
    b.open();
    assert_eq!(b.told(), vec!["changed |0"]);
}

#[test]
fn the_sheet_starts_empty_every_time() {
    let b = bed();
    b.open();
    b.type_text("zone-a");
    b.tab(PASSWORD);
    b.type_text("pw");
    b.tap(Key::Escape);
    assert!(!b.is_open());
    b.open();
    assert_eq!(b.value("Storage zone"), "");
    assert_eq!(b.value("Site name"), "");
    b.told();
    b.tab(PASSWORD);
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|"]);
}

#[test]
fn messages_do_not_outlive_the_sheet() {
    let b = bed();
    b.open();
    b.w.set_add_site_zone_error("zone problem".into());
    b.w.set_add_site_region_error("region problem".into());
    b.w.set_add_site_password_error("password problem".into());
    b.w.set_add_site_result("a result".into());
    b.tap(Key::Escape);
    b.open();
    assert_eq!(
        (
            b.w.get_add_site_zone_error(),
            b.w.get_add_site_region_error(),
            b.w.get_add_site_password_error(),
            b.w.get_add_site_result(),
        ),
        ("".into(), "".into(), "".into(), "".into())
    );
    for text in [
        "zone problem",
        "region problem",
        "password problem",
        "a result",
    ] {
        assert_eq!(b.labelled(text), 0, "{text}");
    }
}

#[test]
fn escape_cancels_from_every_control() {
    for stop in 0..STOPS {
        let b = bed();
        b.open();
        b.tab(stop);
        b.told();
        b.tap(Key::Escape);
        assert_eq!(b.told(), vec!["cancel"], "from stop {stop}");
        assert!(!b.w.get_add_site_open(), "from stop {stop}");
        assert!(!b.is_open(), "from stop {stop}");
    }
}

#[test]
fn the_cancel_button_cancels_and_closes() {
    let b = bed();
    b.open();
    b.tab(CANCEL);
    b.told();
    b.tap(Key::Space);
    assert_eq!(b.told(), vec!["cancel"]);
    assert!(!b.is_open());
}

#[test]
fn the_focus_goes_back_to_add_site_when_the_sheet_closes() {
    let b = bed();
    b.tab(6);
    b.tap(Key::Return);
    b.tap(Key::Escape);
    slint::platform::update_timers_and_animations();
    assert!(!b.is_open());
    b.told();
    // The Add Site button has the focus again, so Return opens the sheet once more.
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["add-site", "changed |0"]);
    assert!(b.is_open());
}

#[test]
fn the_focus_goes_back_when_the_app_closes_the_sheet() {
    let b = bed();
    b.open();
    b.w.set_add_site_open(false);
    slint::platform::update_timers_and_animations();
    b.told();
    b.tap(Key::Return);
    assert_eq!(b.told()[0], "add-site");
    assert!(b.is_open());
}

// ---- Keyboard focus stays inside the sheet ----

#[test]
fn tab_visits_the_controls_in_reading_order() {
    let b = bed();
    b.open();
    b.type_text("a"); // Storage zone
    b.tab(1);
    b.tap(Key::DownArrow); // Region
    b.tab(1);
    b.type_text("b"); // Password
    b.tab(EYE - PASSWORD);
    assert_eq!(b.value("Password"), "", "masked");
    b.tap(Key::Space); // the eye button shows the password
    assert_eq!(b.value("Password"), "b", "shown");
    b.tab(1);
    b.type_text("c"); // Site name
    b.told();
    b.tab(1);
    b.tap(Key::Space); // Test Connection
    b.tab(2); // past Cancel
    b.tap(Key::Return); // Add Site
    assert_eq!(b.told(), vec!["test c|a|1|b", "save c|a|1|b"]);
    // After the last button come the provider grid and the sign-in switch, then the zone field
    // again (with its text selected, so typing replaces it).
    b.tab(STOPS - ADD);
    b.type_text("d");
    assert_eq!(b.value("Storage zone"), "d");
}

#[test]
fn tab_wraps_from_the_last_control_to_the_first_and_back() {
    let b = bed();
    b.open();
    b.tab(STOPS * 3);
    b.type_text("z");
    assert_eq!(b.value("Storage zone"), "z");
}

#[test]
fn shift_tab_from_the_first_control_goes_to_the_last() {
    let b = bed();
    b.open();
    b.told();
    // Back from the zone field past the sign-in switch to the provider grid, the first stop, and
    // from there round to Add Site.
    b.backtab(STOPS - ADD);
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|"]);
}

#[test]
fn shift_held_with_tab_goes_backwards_too() {
    // Real keyboards send Tab with the Shift modifier rather than a separate Backtab key.
    let b = bed();
    b.open();
    // To the provider grid, the first stop.
    b.backtab(STOPS - GRID);
    b.told();
    b.w.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    b.tap(Key::Tab);
    b.w.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|"]);
}

#[test]
fn the_focus_never_leaves_the_sheet() {
    // Escape is only heard while the focus is inside the sheet, so it tells where the focus is.
    for presses in 0..=2 * STOPS + 1 {
        for forward in [true, false] {
            let b = bed();
            b.open();
            if forward {
                b.tab(presses);
            } else {
                b.backtab(presses);
            }
            b.told();
            b.tap(Key::Escape);
            let direction = if forward { "Tab" } else { "Shift+Tab" };
            assert_eq!(b.told(), vec!["cancel"], "{presses} x {direction}");
        }
    }
}

#[test]
fn the_wrap_still_works_while_the_buttons_are_disabled() {
    let b = bed();
    b.open();
    b.w.set_add_site_busy(true);
    // Test Connection and Add Site are disabled, so Cancel is the last stop.
    b.tab(CANCEL - 1);
    b.told();
    b.tap(Key::Space);
    assert_eq!(
        b.told(),
        vec!["cancel"],
        "Cancel is the last stop while busy"
    );

    let b = bed();
    b.open();
    b.w.set_add_site_busy(true);
    b.tab(NAME + 1);
    // Tab from Cancel wraps to the provider grid; the sign-in switch, then the zone field follow.
    b.tab(1 + (STOPS - GRID));
    b.type_text("z");
    assert_eq!(
        b.value("Storage zone"),
        "z",
        "Tab from Cancel wraps to the first stop"
    );
}

#[test]
fn the_wrap_works_from_the_first_field_backwards_while_busy() {
    let b = bed();
    b.open();
    b.w.set_add_site_busy(true);
    // Back past the sign-in switch to the provider grid, then round to Cancel.
    b.backtab(STOPS - GRID + 1);
    b.told();
    b.tap(Key::Space);
    assert_eq!(b.told(), vec!["cancel"]);
}

// ---- What the sheet reports ----

#[test]
fn typing_in_the_zone_and_choosing_a_region_report_the_endpoint_inputs() {
    let b = bed();
    b.open();
    b.told();
    b.type_text("ab");
    assert_eq!(b.told(), vec!["changed a|0", "changed ab|0"]);
    b.tab(REGION);
    b.tap(Key::DownArrow);
    assert_eq!(b.told(), vec!["changed ab|1"]);
    b.tap(Key::DownArrow);
    b.tap(Key::DownArrow);
    // The list ends at the last region.
    assert_eq!(b.told().last().map(String::as_str), Some("changed ab|2"));
}

#[test]
fn the_other_fields_do_not_report_endpoint_changes() {
    let b = bed();
    b.open();
    b.told();
    b.tab(PASSWORD);
    b.type_text("pw");
    b.tab(NAME - PASSWORD);
    b.type_text("My site");
    assert_eq!(b.told(), Vec::<String>::new());
}

#[test]
fn return_in_a_field_adds_the_site_with_every_value() {
    let b = bed();
    b.open();
    b.fill("My site", "zone-a", 1, "pw");
    b.told();
    // The focus is back on the Storage zone field.
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save My site|zone-a|1|pw"]);
}

#[test]
fn return_in_any_text_field_triggers_the_default_button() {
    for field in [ZONE, PASSWORD, NAME] {
        let b = bed();
        b.open();
        b.tab(field);
        b.told();
        b.tap(Key::Return);
        assert_eq!(b.told(), vec!["save ||0|"], "from stop {field}");
    }
}

#[test]
fn return_does_nothing_while_busy() {
    let b = bed();
    b.open();
    b.w.set_add_site_busy(true);
    b.told();
    b.tap(Key::Return);
    assert_eq!(b.told(), Vec::<String>::new());
    b.w.set_add_site_busy(false);
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|"]);
}

#[test]
fn a_held_return_does_not_submit() {
    // Opening the sheet with a held Return key must not send the empty form straight away.
    let b = bed();
    b.open();
    b.told();
    for _ in 0..3 {
        b.w.window().dispatch_event(WindowEvent::KeyPressRepeated {
            text: Key::Return.into(),
        });
    }
    assert_eq!(b.told(), Vec::<String>::new());
    assert!(b.is_open());
}

#[test]
fn the_buttons_report_the_form() {
    let b = bed();
    b.open();
    b.fill("My site", "zone-a", 2, "pw");
    b.told();

    b.tab(TEST);
    b.tap(Key::Space);
    assert_eq!(b.told(), vec!["test My site|zone-a|2|pw"]);

    b.tab(ADD - TEST);
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save My site|zone-a|2|pw"]);
}

#[test]
fn the_buttons_can_be_pressed_by_assistive_technology() {
    let b = bed();
    b.open();
    b.told();
    b.button("Test Connection")
        .invoke_accessible_default_action();
    b.button("Add Site").invoke_accessible_default_action();
    b.button("Cancel").invoke_accessible_default_action();
    let_motion_finish();
    assert_eq!(b.told(), vec!["test ||0|", "save ||0|", "cancel"]);
}

#[test]
fn a_busy_sheet_disables_testing_and_adding_but_not_cancelling() {
    let b = bed();
    b.open();
    b.w.set_add_site_busy(true);
    assert_eq!(
        b.button("Test Connection").accessible_enabled(),
        Some(false)
    );
    assert_eq!(b.button("Add Site").accessible_enabled(), Some(false));
    assert_eq!(b.button("Cancel").accessible_enabled(), Some(true));
    b.told();
    b.button("Test Connection")
        .invoke_accessible_default_action();
    b.button("Add Site").invoke_accessible_default_action();
    assert_eq!(b.told(), Vec::<String>::new());

    b.w.set_add_site_busy(false);
    assert_eq!(b.button("Test Connection").accessible_enabled(), Some(true));
    assert_eq!(b.button("Add Site").accessible_enabled(), Some(true));
}

#[test]
fn the_password_is_masked_for_assistive_technology_and_still_reported() {
    let b = bed();
    b.open();
    b.tab(PASSWORD);
    b.type_text("pw-1");
    // Assistive technology sees a password input with no readable value.
    assert_eq!(b.value("Password"), "");
    b.told();
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|pw-1"]);
}

#[test]
fn what_is_typed_into_the_password_field_cannot_be_copied_or_cut() {
    let b = bed();
    b.open();
    let hold_ctrl = |held: bool| {
        let text: SharedString = Key::Control.into();
        b.w.window().dispatch_event(if held {
            WindowEvent::KeyPressed { text }
        } else {
            WindowEvent::KeyReleased { text }
        });
    };
    let ctrl = |key: &str| {
        hold_ctrl(true);
        b.tap(key);
        hold_ctrl(false);
    };

    // A normal field copies: this is what the clipboard holds afterwards.
    b.tab(NAME);
    b.type_text("ab");
    ctrl("a");
    ctrl("c");

    // The password field neither copies nor cuts, and still holds what was typed.
    b.backtab(NAME - PASSWORD);
    b.type_text("pw");
    ctrl("a");
    ctrl("c");
    ctrl("x");
    b.tab(NAME - PASSWORD);
    ctrl("a");
    ctrl("v");
    assert_eq!(
        b.value("Site name"),
        "ab",
        "the clipboard still has the name"
    );
    b.told();
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ab||0|pw"]);
}

#[test]
fn the_password_field_is_declared_as_a_password_input() {
    // The headless backend cannot see the input type, so look at the source. Since Task 21 every
    // secret is a `SecretField` (the password, the account key and the secret access key), which
    // is a password input until its eye button is pressed; no plain field sets a password type.
    let read = |name: &str| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("ui/components")
            .join(name);
        std::fs::read_to_string(path).unwrap()
    };
    let code = |source: &str| -> Vec<String> {
        source
            .lines()
            .map(|l| l.split("//").next().unwrap().trim().to_owned())
            .collect()
    };
    let sheet = code(&read("add_site.slint"));
    let secrets: Vec<_> = (0..sheet.len())
        .filter(|&i| sheet[i].ends_with(":= SecretField {"))
        .collect();
    assert_eq!(secrets.len(), 3, "password, account key, secret access key");
    // The first is the field labelled Password, on the next line.
    assert!(sheet[secrets[0]].starts_with("password-field := SecretField {"));
    assert_eq!(sheet[secrets[0] + 1], "label: @tr(\"Password\");");
    assert!(!sheet.iter().any(|l| l.starts_with("input-type:")));

    // The secret field hands the password type to its text field unless revealed, and the text
    // field hands it to its input, so nothing is shown in clear.
    let controls = code(&read("form_controls.slint"));
    assert!(
        controls
            .iter()
            .any(|l| l == "input-type: root.revealed ? InputType.text : InputType.password;")
    );
    assert!(controls.iter().any(|l| l == "input-type: root.input-type;"));
}

// ---- Errors and the connection result ----

#[test]
fn field_errors_show_under_their_own_field_in_red() {
    let b = bed();
    b.open();
    b.w.set_add_site_zone_error("Enter the storage zone name.".into());
    b.w.set_add_site_region_error("Choose a region.".into());
    b.w.set_add_site_password_error("Enter the zone's password.".into());

    let below = |field: &str, error: &str| {
        let (f, e) = (
            b.control(AccessibleRole::TextInput, field),
            b.labelled(error),
        );
        assert_eq!(e, 1, "{error}");
        (f, b.control(AccessibleRole::Text, error))
    };
    let (zone, zone_error) = below("Storage zone", "Enter the storage zone name.");
    assert!(zone_error.absolute_position().y >= zone.absolute_position().y + zone.size().height);
    let (password, password_error) = below("Password", "Enter the zone's password.");
    assert!(
        password_error.absolute_position().y
            >= password.absolute_position().y + password.size().height
    );
    // The region error sits under the region list.
    let region = b.control(AccessibleRole::Combobox, "Region");
    let region_error = b.control(AccessibleRole::Text, "Choose a region.");
    assert!(
        region_error.absolute_position().y >= region.absolute_position().y + region.size().height
    );
    // And each field says what is wrong with it.
    assert_eq!(
        b.control(AccessibleRole::TextInput, "Storage zone")
            .accessible_description()
            .as_deref(),
        Some("Enter the storage zone name.")
    );
    assert_eq!(
        b.control(AccessibleRole::TextInput, "Site name")
            .accessible_description()
            .as_deref(),
        Some("")
    );
}

#[test]
fn editing_a_field_clears_its_own_error_only() {
    let b = bed();
    b.open();
    b.w.set_add_site_zone_error("zone problem".into());
    b.w.set_add_site_region_error("region problem".into());
    b.w.set_add_site_password_error("password problem".into());

    b.type_text("z");
    assert_eq!(b.w.get_add_site_zone_error(), "");
    assert_eq!(b.w.get_add_site_password_error(), "password problem");
    assert_eq!(b.labelled("zone problem"), 0);

    b.tab(REGION);
    b.tap(Key::DownArrow);
    assert_eq!(b.w.get_add_site_region_error(), "");
    assert_eq!(b.w.get_add_site_password_error(), "password problem");

    b.tab(1);
    b.type_text("p");
    assert_eq!(b.w.get_add_site_password_error(), "");
}

#[test]
fn showing_a_field_error_sets_one_and_clears_the_others() {
    let w = AppWindow::new().unwrap();
    show_field_error(
        &w,
        Some(&FieldError::Zone("Enter the storage zone name.".into())),
    );
    assert_eq!(w.get_add_site_zone_error(), "Enter the storage zone name.");
    assert_eq!(w.get_add_site_region_error(), "");
    assert_eq!(w.get_add_site_password_error(), "");

    show_field_error(
        &w,
        Some(&FieldError::Password("Enter the zone's password.".into())),
    );
    assert_eq!(w.get_add_site_zone_error(), "");
    assert_eq!(
        w.get_add_site_password_error(),
        "Enter the zone's password."
    );

    show_field_error(&w, Some(&FieldError::Region("Choose a region.".into())));
    assert_eq!(w.get_add_site_region_error(), "Choose a region.");
    assert_eq!(w.get_add_site_password_error(), "");

    show_field_error(&w, None);
    assert_eq!(
        (
            w.get_add_site_zone_error(),
            w.get_add_site_region_error(),
            w.get_add_site_password_error()
        ),
        ("".into(), "".into(), "".into())
    );
}

#[test]
fn the_test_result_is_announced_and_goes_stale_when_the_form_changes() {
    let b = bed();
    b.open();
    let text = "Connected over HTTPS · HTTP/2 · 21 ms · 282 items in /";
    assert_eq!(b.labelled(text), 0);
    b.w.set_add_site_result(text.into());
    b.w.set_add_site_result_ok(true);
    assert_eq!(b.labelled(text), 1);
    let box_ = b.control(AccessibleRole::Text, text);
    assert_eq!(
        box_.accessible_live_region(),
        Some(AccessibleLiveness::Polite)
    );

    // A different zone or region makes the result about something else: it goes away.
    b.type_text("z");
    assert_eq!(b.w.get_add_site_result(), "");
    assert_eq!(b.labelled(text), 0);

    b.w.set_add_site_result(
        "Couldn't sign in to z. Check the password, zone name and region.".into(),
    );
    b.w.set_add_site_result_ok(false);
    b.tab(REGION);
    b.tap(Key::DownArrow);
    assert_eq!(b.w.get_add_site_result(), "");

    b.w.set_add_site_result("old".into());
    b.tab(1);
    b.type_text("p");
    assert_eq!(b.w.get_add_site_result(), "");
}

#[test]
fn a_busy_sheet_says_it_is_connecting() {
    let b = bed();
    b.open();
    assert_eq!(b.labelled("Connecting…"), 0);
    b.w.set_add_site_busy(true);
    assert_eq!(b.labelled("Connecting…"), 1);
    b.w.set_add_site_busy(false);
    assert_eq!(b.labelled("Connecting…"), 0);
}

#[test]
fn the_endpoint_is_shown_read_only() {
    let b = bed();
    b.open();
    b.w.set_add_site_endpoint("https://sg.storage.bunnycdn.com/zone-a/".into());
    assert_eq!(b.labelled("https://sg.storage.bunnycdn.com/zone-a/"), 1);
    // Not an input: nothing to type into besides the three fields.
    let inputs = b
        .sheet()
        .query_descendants()
        .match_accessible_role(AccessibleRole::TextInput)
        .find_all();
    assert_eq!(inputs.len(), 3);
}

// ---- What the sheet shows ----

#[test]
fn the_sheet_is_a_labelled_form_and_names_its_regions() {
    let b = bed();
    b.open();
    assert_eq!(b.sheet().accessible_label().as_deref(), Some("Add Site"));
    let region = b.control(AccessibleRole::Combobox, "Region");
    assert_eq!(region.accessible_value().as_deref(), Some("Frankfurt"));
}

#[test]
fn only_bunny_can_be_picked_in_this_build() {
    let b = bed();
    b.open();
    let radios = b
        .sheet()
        .query_descendants()
        .match_accessible_role(AccessibleRole::RadioButton)
        .find_all();
    let describe = |e: &ElementHandle| {
        (
            e.accessible_label().unwrap().to_string(),
            e.accessible_enabled(),
            e.accessible_checked(),
        )
    };
    let tiles: Vec<_> = radios.iter().map(describe).collect();
    // Task 21: a tile is called "{name}, {kind}"; a tile that is off says why in its description.
    let bunny = (
        "Bunny Storage, HTTP API".to_string(),
        Some(true),
        Some(true),
    );
    assert!(tiles.contains(&bunny), "{tiles:?}");
    for provider in [
        "Amazon S3, S3",
        "Cloudflare R2, S3-compatible",
        "Backblaze B2, S3-compatible",
        "Wasabi, S3-compatible",
        "DigitalOcean Spaces, S3-compatible",
        "MinIO, S3-compatible",
        "Other S3, Custom endpoint",
    ] {
        let wanted = (provider.to_owned(), Some(false), Some(false));
        assert!(tiles.contains(&wanted), "{provider}: {tiles:?}");
    }
    let off: Vec<_> = radios
        .iter()
        .filter(|e| e.accessible_enabled() == Some(false))
        .map(|e| e.accessible_description().unwrap_or_default().to_string())
        .collect();
    assert_eq!(off, vec!["Arrives later in this build."; 8]);
    // Sign-in method: the zone password is the only one that works.
    assert!(
        tiles.contains(&("Storage zone password".to_string(), Some(true), Some(true))),
        "{tiles:?}"
    );
    assert!(
        tiles.contains(&("Bunny account key".to_string(), Some(false), Some(false))),
        "{tiles:?}"
    );
    assert_eq!(tiles.len(), 10);
}

#[test]
fn the_unavailable_tiles_are_labelled_coming_soon_on_screen() {
    // Task 21: one note under the grid and one beside the sign-in switch, while the second round
    // of Add Site is off (Ruling Q10); none once it is on.
    let notes = |b: &Bed| {
        b.sheet()
            .query_descendants()
            .match_accessible_role(AccessibleRole::Text)
            .match_predicate(|e| {
                e.accessible_label()
                    .is_some_and(|l| l.as_str() == "Arrives later in this build.")
            })
            .find_all()
            .len()
    };
    let b = bed();
    b.open();
    assert_eq!(notes(&b), 2);

    let b = bed();
    b.w.set_sites_v2_ready(true);
    b.open();
    assert_eq!(notes(&b), 0);
}

/// Puts one message on the open sheet.
type ShowMessage = fn(&AppWindow);

#[test]
fn the_sheet_fits_the_smallest_window_with_any_one_message_showing() {
    // The app shows a field error or a test result, not both: an edit clears the result, and only
    // a submit that passes validation gets as far as a test.
    let messages: [(&str, ShowMessage); 5] = [
        ("no message", |_| {}),
        ("zone error", |w| {
            w.set_add_site_zone_error("Zone names can't contain spaces.".into())
        }),
        ("region error", |w| {
            w.set_add_site_region_error("Choose a region.".into())
        }),
        ("password error", |w| {
            w.set_add_site_password_error("Enter the zone's password.".into())
        }),
        ("test result", |w| {
            w.set_add_site_result(
                "Couldn't sign in to z. Check the password, zone name and region.".into(),
            )
        }),
    ];
    for (what, show) in messages {
        let b = bed();
        b.open();
        b.w.window().set_size(LogicalSize::new(960.0, 600.0));
        b.w.set_add_site_endpoint("https://sg.storage.bunnycdn.com/zone-a/".into());
        show(&b.w);
        slint::platform::update_timers_and_animations();
        let sheet = b.sheet();
        let (p, s) = (sheet.absolute_position(), sheet.size());
        assert!(
            p.y >= 0.0 && p.y + s.height <= 600.0,
            "{what}: sheet {p:?} {s:?}"
        );
        assert!(
            p.x >= 0.0 && p.x + s.width <= 960.0,
            "{what}: sheet {p:?} {s:?}"
        );
        for label in ["Test Connection", "Cancel", "Add Site"] {
            let button = b.button(label);
            let (p, s) = (button.absolute_position(), button.size());
            assert!(
                p.y + s.height <= 600.0,
                "{what}: {label} is cut off: {p:?} {s:?}"
            );
        }
    }
}

// ---- The window behind ----

#[test]
fn the_window_behind_ignores_the_pointer_while_the_sheet_is_open() {
    let b = bed();
    let row = row_starting(&b.w, "example-shop.com");
    click(b.w.window(), center(&row), 1);
    assert_eq!(b.told(), vec!["site-selected 0"], "control: the row works");

    b.open();
    b.told();
    click(
        b.w.window(),
        center(&row_starting(&b.w, "media-archive")),
        1,
    );
    assert_eq!(b.told(), Vec::<String>::new());
    assert_eq!(b.w.get_selected_site(), 0);
}

#[test]
fn the_window_behind_stays_inert_when_the_sheet_itself_holds_the_focus() {
    // A click on blank sheet space gives the focus to the sheet's own scope, which then lets
    // later clicks go by: the scrim has to stop them.
    let b = bed();
    b.open();
    let sheet = b.sheet();
    let blank = slint::LogicalPosition::new(
        sheet.absolute_position().x + sheet.size().width - 6.0,
        sheet.absolute_position().y + 40.0,
    );
    click(b.w.window(), blank, 1);
    b.told();
    click(
        b.w.window(),
        center(&row_starting(&b.w, "media-archive")),
        1,
    );
    click(
        b.w.window(),
        center(&row_starting(&b.w, "example-shop.com")),
        2,
    );
    assert_eq!(b.told(), Vec::<String>::new());
    assert_eq!(b.w.get_selected_site(), -1);
}

#[test]
fn clicking_the_dimmed_window_does_not_dismiss_the_sheet() {
    let b = bed();
    b.open();
    b.told();
    let size = b.w.window().size();
    click(
        b.w.window(),
        slint::LogicalPosition::new(size.width as f32 - 4.0, size.height as f32 - 4.0),
        1,
    );
    assert!(b.is_open() && b.w.get_add_site_open());
    assert_eq!(b.told(), Vec::<String>::new());
}

#[test]
fn clicking_a_field_focuses_it_and_clicking_blank_sheet_space_keeps_the_focus_inside() {
    let b = bed();
    b.open();
    let name = b.control(AccessibleRole::TextInput, "Site name");
    click(b.w.window(), center(&name), 1);
    b.type_text("n");
    assert_eq!(b.value("Site name"), "n");

    // Blank space on the sheet (its subtitle area) takes the focus without leaving the sheet.
    let sheet = b.sheet();
    let blank = slint::LogicalPosition::new(
        sheet.absolute_position().x + sheet.size().width - 6.0,
        sheet.absolute_position().y + 40.0,
    );
    click(b.w.window(), blank, 1);
    b.told();
    b.tap(Key::Escape);
    assert_eq!(b.told(), vec!["cancel"]);
}

#[test]
fn tab_and_shift_tab_still_wrap_after_a_click_on_blank_sheet_space() {
    let blank_click = |b: &Bed| {
        let sheet = b.sheet();
        let blank = slint::LogicalPosition::new(
            sheet.absolute_position().x + sheet.size().width - 6.0,
            sheet.absolute_position().y + 40.0,
        );
        click(b.w.window(), blank, 1);
    };

    // Forward from the blank space goes to the first stop (the provider grid); the sign-in switch
    // and the zone field follow.
    let b = bed();
    b.open();
    blank_click(&b);
    b.tab(1 + (STOPS - GRID));
    b.type_text("z");
    assert_eq!(b.value("Storage zone"), "z");

    // Backward from the blank space goes to the last button, not out to the window behind.
    let b = bed();
    b.open();
    blank_click(&b);
    b.told();
    b.backtab(1);
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|"]);
}

// ---- The region list ----

#[test]
fn the_region_list_opens_with_return_and_escape_closes_only_the_list() {
    let b = bed();
    b.open();
    b.tab(REGION);
    let region = || b.control(AccessibleRole::Combobox, "Region");
    assert_eq!(region().accessible_expanded(), Some(false));
    b.tap(Key::Return);
    assert_eq!(region().accessible_expanded(), Some(true));

    // Inside the list Down changes the region.
    b.told();
    b.tap(Key::DownArrow);
    assert_eq!(b.told(), vec!["changed |1"]);

    // Escape closes the list and leaves the sheet where it was.
    b.tap(Key::Escape);
    assert!(b.is_open());
    assert_eq!(region().accessible_expanded(), Some(false));
    assert_eq!(b.told(), Vec::<String>::new());
    assert_eq!(region().accessible_value().as_deref(), Some("Singapore"));

    // The focus is back on the picker; Escape there is the sheet's own Escape.
    b.tap(Key::DownArrow);
    assert_eq!(b.told(), vec!["changed |2"]);
    b.tap(Key::Escape);
    assert_eq!(b.told(), vec!["cancel"]);
}

#[test]
fn the_region_list_is_chosen_from_with_return_space_home_and_end() {
    let b = bed();
    b.open();
    b.tab(REGION);
    b.tap(Key::Space);
    b.told();
    b.tap(Key::End);
    assert_eq!(b.told(), vec!["changed |2"]);
    b.tap(Key::Home);
    assert_eq!(b.told(), vec!["changed |0"]);
    b.tap(Key::DownArrow);
    b.tap(Key::Return);
    let region = b.control(AccessibleRole::Combobox, "Region");
    assert_eq!(region.accessible_expanded(), Some(false));
    assert_eq!(region.accessible_value().as_deref(), Some("Singapore"));
    // The choice is what the buttons report.
    b.told();
    b.tab(ADD - REGION);
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||1|"]);
}

#[test]
fn the_closed_region_picker_changes_with_up_and_down_and_stops_at_the_ends() {
    let b = bed();
    b.open();
    b.tab(REGION);
    b.told();
    b.tap(Key::UpArrow);
    assert_eq!(b.told(), Vec::<String>::new());
    b.tap(Key::DownArrow);
    b.tap(Key::DownArrow);
    b.tap(Key::DownArrow);
    assert_eq!(b.told(), vec!["changed |1", "changed |2"]);
    assert_eq!(
        b.control(AccessibleRole::Combobox, "Region")
            .accessible_value()
            .as_deref(),
        Some("London")
    );
}

#[test]
fn a_region_list_can_also_be_opened_by_assistive_technology() {
    let b = bed();
    b.open();
    let region = b.control(AccessibleRole::Combobox, "Region");
    region.invoke_accessible_expand_action();
    assert_eq!(region.accessible_expanded(), Some(true));
}

#[test]
fn without_regions_the_picker_is_empty_and_does_not_open() {
    let b = bed();
    b.w.set_region_names(ModelRc::new(VecModel::<SharedString>::default()));
    b.open();
    b.tab(REGION);
    b.tap(Key::Return);
    let region = b.control(AccessibleRole::Combobox, "Region");
    assert_eq!(region.accessible_expanded(), Some(false));
    assert_eq!(region.accessible_value().as_deref(), Some(""));
}

// ---- The sidebar's site list ----

#[test]
fn an_empty_site_list_says_so() {
    testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    w.show().unwrap();
    // Task 21: the sidebar's empty states, and the Welcome screen in place of the panes.
    let says = |w: &AppWindow, text: &str| ElementHandle::find_by_accessible_label(w, text).count();
    let empty_sites = "No sites yet. Add one to start browsing and publishing.";
    let empty_publish = "Publish profiles appear here after you add a site.";
    let tagline = "Fast, safe file transfers to cloud storage over HTTP.";
    assert_eq!(says(&w, empty_sites), 1);
    assert_eq!(says(&w, empty_publish), 1);
    assert_eq!(says(&w, tagline), 1);
    w.set_sites(ModelRc::new(VecModel::from(vec![site(
        "example-shop.com",
        ProviderTile::Bunny,
        "B",
    )])));
    assert_eq!(says(&w, empty_sites), 0);
    assert_eq!(says(&w, empty_publish), 0);
    assert_eq!(says(&w, tagline), 0);
}

#[test]
fn a_site_whose_last_request_failed_is_described_as_such() {
    testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    let mut sick = site("media-archive", ProviderTile::R2, "R2");
    sick.healthy = false;
    w.set_sites(ModelRc::new(VecModel::from(vec![
        site("example-shop.com", ProviderTile::Bunny, "B"),
        sick,
    ])));
    w.show().unwrap();
    // Ledger 130: said in the row's label, as the mark's shape and colour show it.
    let label = |name: &str| row_starting(&w, name).accessible_label().unwrap();
    assert_eq!(label("example-shop.com"), "example-shop.com, Bunny");
    assert_eq!(
        label("media-archive"),
        "media-archive, Bunny, last request failed"
    );
}

// ---- Fix round 1: error announcements, error focus, outlines, placeholder ----

#[test]
fn error_lines_are_assertive_live_regions() {
    let b = bed();
    b.open();
    let messages = [
        ("Zone names can't contain spaces.", 0),
        ("Choose a region.", 1),
        ("Enter the zone's password.", 2),
    ];
    for (message, which) in messages {
        match which {
            0 => b.w.set_add_site_zone_error(message.into()),
            1 => b.w.set_add_site_region_error(message.into()),
            _ => b.w.set_add_site_password_error(message.into()),
        }
        let line = b.control(AccessibleRole::Text, message);
        assert_eq!(
            line.accessible_live_region(),
            Some(AccessibleLiveness::Assertive),
            "{message}"
        );
    }
}

#[test]
fn showing_a_field_error_moves_the_focus_to_that_field() {
    let show = |b: &Bed, error: FieldError| {
        show_field_error(&b.w, Some(&error));
        slint::platform::update_timers_and_animations();
    };

    // Storage zone: typing lands in it.
    let b = bed();
    b.open();
    b.tab(ADD);
    show(&b, FieldError::Zone("Enter the storage zone name.".into()));
    b.type_text("z");
    assert_eq!(b.value("Storage zone"), "z");

    // Region: Down changes the region.
    let b = bed();
    b.open();
    b.tab(ADD);
    show(&b, FieldError::Region("Choose a region.".into()));
    b.told();
    b.tap(Key::DownArrow);
    assert_eq!(b.told(), vec!["changed |1"]);

    // Password: typing lands in it, and Return then sends it.
    let b = bed();
    b.open();
    b.tab(ADD);
    show(
        &b,
        FieldError::Password("Enter the zone's password.".into()),
    );
    b.type_text("p");
    b.told();
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|p"]);

    // The same error shown again (nothing was edited) still brings the focus back.
    let b = bed();
    b.open();
    let again = || FieldError::Zone("Enter the storage zone name.".into());
    b.tab(ADD);
    show(&b, again());
    b.tab(2);
    show(&b, again());
    b.type_text("z");
    assert_eq!(b.value("Storage zone"), "z");

    // Clearing the errors does not move the focus.
    let b = bed();
    b.open();
    b.tab(ADD);
    show_field_error(&b.w, None);
    slint::platform::update_timers_and_animations();
    b.told();
    b.tap(Key::Return);
    assert_eq!(b.told(), vec!["save ||0|"]);
}

#[test]
fn an_empty_form_walks_the_user_to_each_problem() {
    use crate::add_site::{AddSiteForm, ValidSite};
    // The app side of Task 15, in miniature: validate on save, show the error.
    let b = bed();
    let saved: Rc<RefCell<Option<ValidSite>>> = Rc::default();
    let weak = b.w.as_weak();
    b.w.on_add_site_save({
        let saved = saved.clone();
        move |name, zone, region, password| {
            let form = AddSiteForm {
                name: name.into(),
                zone: zone.into(),
                region_index: region,
                password: password.into(),
            };
            let ui = weak.upgrade().unwrap();
            match form.validate(&["de", "sg", "uk"]) {
                Ok(site) => *saved.borrow_mut() = Some(site),
                Err(error) => show_field_error(&ui, Some(&error)),
            }
        }
    });
    b.open();
    b.tab(ADD);
    b.tap(Key::Return); // empty form: the zone is missing
    slint::platform::update_timers_and_animations();
    assert_eq!(
        b.w.get_add_site_zone_error(),
        "Enter the storage zone name."
    );
    b.type_text("z"); // the focus is already in the zone field
    b.tap(Key::Return); // password missing
    slint::platform::update_timers_and_animations();
    assert_eq!(
        b.w.get_add_site_password_error(),
        "Enter the zone's password."
    );
    assert_eq!(b.w.get_add_site_zone_error(), "");
    b.type_text("p"); // the focus is already in the password field
    assert!(saved.borrow().is_none());
    b.tap(Key::Return);
    let site = saved.borrow().clone().expect("saved");
    assert_eq!(
        (
            site.zone.as_str(),
            site.region_code.as_str(),
            site.password.as_str()
        ),
        ("z", "de", "p")
    );
}

slint::slint! {
    #[style = "cupertino"]
    import { CapsuleButton } from "../ui/components/buttons.slint";
    import { Theme } from "../ui/theme.slint";

    // A plain and a primary button, exposing their outlines.
    export component EdgeBed inherits Window {
        out property <color> field-border: Theme.field-border;
        out property <color> plain-color: plain.border-color;
        out property <length> plain-width: plain.border-width;
        out property <color> primary-color: primary.border-color;
        out property <length> primary-width: primary.border-width;

        HorizontalLayout {
            plain := CapsuleButton { text: "Plain"; }
            primary := CapsuleButton { text: "Primary"; primary: true; }
        }
    }
}

#[test]
fn a_plain_button_is_outlined_in_the_field_border_and_the_primary_one_is_not() {
    testing::init_no_event_loop();
    let bed = EdgeBed::new().unwrap();
    assert_eq!(bed.get_plain_width(), 1.0);
    assert_eq!(bed.get_plain_color(), bed.get_field_border());
    assert_ne!(bed.get_field_border().alpha(), 0);
    assert_eq!(bed.get_primary_width(), 0.5);
    assert_eq!(bed.get_primary_color().alpha(), 0);
}

#[test]
fn the_placeholder_is_set_in_the_secondary_label_colour() {
    // `label3` is only about 3:1 on the field; the placeholder carries real information (what
    // the site name will be), so it uses `label2`.
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/components/form_controls.slint");
    let source = std::fs::read_to_string(path).unwrap();
    let lines: Vec<_> = source.lines().map(str::trim).collect();
    let at = lines
        .iter()
        .position(|l| *l == "text: root.placeholder;")
        .expect("the placeholder text");
    let after = &lines[at + 1..at + 5];
    assert!(after.contains(&"color: Theme.label2;"), "{after:?}");
    assert!(!after.iter().any(|l| l.contains("label3")), "{after:?}");
}

// ---- Add Site, round 2 and Welcome (Task 21) ----

/// The provider tiles in the mockup's order, as assistive technology names them.
const TILES: [&str; 8] = [
    "Bunny Storage, HTTP API",
    "Amazon S3, S3",
    "Cloudflare R2, S3-compatible",
    "Backblaze B2, S3-compatible",
    "Wasabi, S3-compatible",
    "DigitalOcean Spaces, S3-compatible",
    "MinIO, S3-compatible",
    "Other S3, Custom endpoint",
];

/// The bed with every control of the second round turned on (Ruling Q10), as `install` leaves
/// the window since Task 24.
fn bed_v2() -> Bed {
    let b = bed();
    b.w.set_sites_v2_ready(true);
    b
}

impl Bed {
    /// The provider grid's tiles: (name, enabled, checked), in the order they are on screen.
    fn tiles(&self) -> Vec<(String, Option<bool>, Option<bool>)> {
        self.control(AccessibleRole::RadioGroup, "Storage provider")
            .query_descendants()
            .match_accessible_role(AccessibleRole::RadioButton)
            .find_all()
            .iter()
            .map(|e| {
                (
                    e.accessible_label().unwrap_or_default().to_string(),
                    e.accessible_enabled(),
                    e.accessible_checked(),
                )
            })
            .collect()
    }

    /// The labels of the text fields on show, in reading order.
    fn inputs(&self) -> Vec<String> {
        self.sheet()
            .query_descendants()
            .match_accessible_role(AccessibleRole::TextInput)
            .find_all()
            .iter()
            .map(|e| e.accessible_label().unwrap_or_default().to_string())
            .collect()
    }

    fn press(&self, role: AccessibleRole, label: &str) {
        self.control(role, label).invoke_accessible_default_action();
        let_motion_finish();
    }

    fn has(&self, role: AccessibleRole, label: &str) -> bool {
        let wanted = label.to_owned();
        self.sheet()
            .query_descendants()
            .match_accessible_role(role)
            .match_predicate(move |e| e.accessible_label().is_some_and(|l| l.as_str() == wanted))
            .find_first()
            .is_some()
    }
}

#[test]
fn provider_grid_has_eight_tiles_in_mockup_order() {
    // Off: only Bunny can be picked, and the arrow keys stay on it.
    let b = bed();
    b.open();
    let tiles = b.tiles();
    let names: Vec<&str> = tiles.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, TILES);
    assert_eq!(tiles[0].1, Some(true));
    assert!(
        tiles[1..]
            .iter()
            .all(|(_, enabled, _)| *enabled == Some(false))
    );
    b.backtab(STOPS - GRID);
    b.told();
    b.tap(Key::RightArrow);
    assert_eq!(b.told(), Vec::<String>::new());
    assert_eq!(b.tiles()[0].2, Some(true));

    // On: every tile, one Tab stop moved through with the arrow keys (four to a row).
    let b = bed_v2();
    b.open();
    assert!(
        b.tiles()
            .iter()
            .all(|(_, enabled, _)| *enabled == Some(true))
    );
    b.backtab(STOPS - GRID);
    b.told();
    for _ in 0..8 {
        b.tap(Key::RightArrow);
    }
    let picked: Vec<String> = (1..=7).map(|i| format!("provider {i}")).collect();
    assert_eq!(b.told(), picked, "the last tile is the end");
    assert_eq!(b.tiles()[7].2, Some(true));
    b.tap(Key::UpArrow);
    assert_eq!(b.told(), ["provider 3"]);
    b.tap(Key::DownArrow);
    assert_eq!(b.told(), ["provider 7"]);
    b.tap(Key::LeftArrow);
    assert_eq!(b.told(), ["provider 6"]);
    // A tile pressed by assistive technology is chosen too.
    b.press(AccessibleRole::RadioButton, "Cloudflare R2, S3-compatible");
    assert_eq!(b.told(), ["provider 2"]);
    assert_eq!(b.tiles()[2].2, Some(true));
}

#[test]
fn each_provider_shows_its_own_fields() {
    let b = bed_v2();
    b.open();
    assert_eq!(b.inputs(), ["Storage zone", "Password", "Site name"]);
    let s3 = |first: &[&str]| -> Vec<String> {
        first
            .iter()
            .chain(&["Bucket", "Access key ID", "Secret access key", "Site name"])
            .map(|s| (*s).to_owned())
            .collect()
    };
    for (tile, wanted) in [
        (1, s3(&["Region"])),
        (2, s3(&["Account ID"])),
        (3, s3(&["Region"])),
        (4, s3(&["Region"])),
        (5, s3(&["Region"])),
        (6, s3(&["Endpoint"])),
        (7, s3(&["Endpoint", "Region"])),
    ] {
        b.press(AccessibleRole::RadioButton, TILES[tile]);
        assert_eq!(b.inputs(), wanted, "{}", TILES[tile]);
        // Bunny's sign-in choice goes with Bunny.
        assert!(!b.has(AccessibleRole::RadioGroup, "Sign-in method"));
        assert!(b.has(AccessibleRole::Button, "Choose…"));
    }
    // Only Other S3 has the path-style box.
    assert!(b.has(AccessibleRole::Checkbox, "Use path-style addresses"));
    b.press(AccessibleRole::RadioButton, TILES[6]);
    assert!(!b.has(AccessibleRole::Checkbox, "Use path-style addresses"));

    // Back on Bunny, the account key has a form of its own, without a site name.
    b.press(AccessibleRole::RadioButton, TILES[0]);
    b.press(AccessibleRole::RadioButton, "Bunny account key");
    assert_eq!(b.inputs(), ["Account key"]);
    assert!(b.has(AccessibleRole::Button, "Find Zones"));
    assert!(!b.has(AccessibleRole::Button, "Test Connection"));

    // An S3 form reports what is on show, with the secret apart.
    let b = bed_v2();
    b.open();
    // Back to the grid, Right to Amazon S3, Tab to its Region field.
    b.backtab(STOPS - GRID);
    b.tap(Key::RightArrow);
    b.told();
    b.tab(1);
    for (text, tabs) in [("eu-west-1", 1), ("media", 2), ("AKID", 1), ("sk", 0)] {
        b.type_text(text);
        b.tab(tabs);
    }
    b.tap(Key::Return);
    assert_eq!(b.told(), ["save-s3 1|eu-west-1|||media|AKID|sk"]);
}

#[test]
fn reveal_toggles_and_resets_on_close() {
    let b = bed_v2();
    b.open();
    b.tab(PASSWORD);
    b.type_text("pw");
    assert_eq!(b.value("Password"), "", "hidden at first");
    b.told();
    b.press(AccessibleRole::Button, "Show password");
    assert_eq!(b.told(), ["reveal"]);
    assert_eq!(b.value("Password"), "pw");
    assert!(b.has(AccessibleRole::Button, "Hide password"));
    b.press(AccessibleRole::Button, "Hide password");
    assert_eq!(b.value("Password"), "");
    b.press(AccessibleRole::Button, "Show password");
    assert_eq!(b.value("Password"), "pw");

    // Another provider hides it again (what was typed is kept) …
    b.press(AccessibleRole::RadioButton, TILES[1]);
    b.press(AccessibleRole::RadioButton, TILES[0]);
    assert_eq!(b.value("Password"), "");
    assert!(!b.has(AccessibleRole::Button, "Hide password"));
    b.press(AccessibleRole::Button, "Show password");
    assert_eq!(b.value("Password"), "pw");
    // … and so does closing the sheet.
    b.tap(Key::Escape);
    b.open();
    assert!(b.has(AccessibleRole::Button, "Show password"));
    assert!(!b.has(AccessibleRole::Button, "Hide password"));

    // The account key and the secret access key have an eye too.
    b.told();
    b.press(AccessibleRole::RadioButton, "Bunny account key");
    b.press(AccessibleRole::Button, "Show account key");
    assert!(b.has(AccessibleRole::Button, "Hide account key"));
    b.press(AccessibleRole::RadioButton, TILES[4]);
    b.press(AccessibleRole::Button, "Show secret access key");
    assert!(b.has(AccessibleRole::Button, "Hide secret access key"));
    assert_eq!(
        b.told(),
        ["changed |0", "sign-in 1", "reveal", "provider 4", "reveal"]
    );
    b.tap(Key::Escape);
    b.open();
    b.press(AccessibleRole::RadioButton, TILES[4]);
    assert!(b.has(AccessibleRole::Button, "Show secret access key"));
}

#[test]
fn edit_site_says_save_and_keeps_an_empty_secret() {
    let b = bed_v2();
    b.w.set_add_site_editing(true);
    b.w.set_add_site_draft(crate::SiteDraft {
        provider: 0,
        name: "My site".into(),
        zone: "testzone".into(),
        region_index: 1,
        ..crate::SiteDraft::default()
    });
    b.open();
    assert_eq!(b.sheet().accessible_label().as_deref(), Some("Edit Site"));
    assert!(b.has(AccessibleRole::Button, "Save"));
    assert!(!b.has(AccessibleRole::Button, "Add Site"));
    // The grid and the sign-in choice are folded away: a saved site keeps its provider.
    assert!(!b.has(AccessibleRole::RadioGroup, "Storage provider"));
    assert!(!b.has(AccessibleRole::RadioGroup, "Sign-in method"));
    assert_eq!(b.value("Storage zone"), "testzone");
    assert_eq!(b.value("Site name"), "My site");
    assert_eq!(
        b.control(AccessibleRole::TextInput, "Password")
            .accessible_placeholder_text()
            .as_deref(),
        Some("Saved. Leave empty to keep it.")
    );
    b.told();
    b.tap(Key::Return);
    assert_eq!(b.told(), ["save My site|testzone|1|"]);
}

#[test]
fn account_key_zone_list_checkboxes() {
    use httporter_bunny::{PullZoneInfo, UnusableZone, ZoneInfo, ZoneList};

    let b = bed_v2();
    b.open();
    // From the zone field back to the sign-in switch, Right to the account key, Tab to its field.
    b.backtab(1);
    b.told();
    b.tap(Key::RightArrow);
    assert_eq!(b.told(), ["changed |0", "sign-in 1"]);
    let add = || b.control(AccessibleRole::Button, "Add Sites");
    assert_eq!(add().accessible_enabled(), Some(false), "nothing ticked");
    assert_eq!(
        b.button("Find Zones").accessible_enabled(),
        Some(false),
        "no key yet"
    );
    b.tab(1);
    b.type_text("key-1");
    b.press(AccessibleRole::Button, "Find Zones");
    assert_eq!(b.told(), ["find-zones key-1"]);
    // Return in the key field asks too, while nothing is ticked.
    b.tap(Key::Return);
    assert_eq!(b.told(), ["find-zones key-1"]);

    let zone = |name: &str, region: &str, hostnames: &[&str]| ZoneInfo {
        id: 1,
        name: name.into(),
        region: region.into(),
        host: "storage.bunnycdn.com".into(),
        password: "pw".into(),
        read_only_password: String::new(),
        pull_zones: if hostnames.is_empty() {
            Vec::new()
        } else {
            vec![PullZoneInfo {
                id: 9,
                name: "pz".into(),
                hostnames: hostnames.iter().map(|h| (*h).to_owned()).collect(),
            }]
        },
    };
    crate::add_site::show_zones(
        &b.w,
        &ZoneList {
            zones: vec![
                zone(
                    "exampleshop",
                    "sg",
                    &["exampleshop.b-cdn.net", "example-shop.com"],
                ),
                zone("archive", "de", &[]),
            ],
            unusable: vec![UnusableZone {
                name: "broken".into(),
                reason: "no password".into(),
            }],
        },
    );
    slint::platform::update_timers_and_animations();
    let first = "exampleshop · Singapore · exampleshop.b-cdn.net, example-shop.com";
    let second = "archive · Frankfurt";
    for label in [first, second] {
        let row = b.control(AccessibleRole::Checkbox, label);
        assert_eq!(row.accessible_checked(), Some(true), "{label}");
    }
    // A zone that can't be added is greyed out with its reason, and can't be ticked.
    assert_eq!(b.labelled("broken · can't be added: no password"), 1);
    assert!(!b.has(
        AccessibleRole::Checkbox,
        "broken · can't be added: no password"
    ));
    assert!(b.has(AccessibleRole::Button, "Add 2 Sites"));

    // Unticking one says so on the button.
    b.press(AccessibleRole::Checkbox, second);
    assert!(b.has(AccessibleRole::Button, "Add 1 Site"));
    assert_eq!(b.w.get_add_site_zone_count(), 1);
    b.press(AccessibleRole::Checkbox, first);
    assert!(b.has(AccessibleRole::Button, "Add Sites"));
    b.press(AccessibleRole::Checkbox, first);

    // The key is forgotten by default; "Keep it" is the other choice.
    let forget = "Use it once to fetch zone passwords, then forget it";
    let keep = "Keep it for cache purging and zone tools";
    assert_eq!(
        b.control(AccessibleRole::RadioButton, forget)
            .accessible_checked(),
        Some(true)
    );
    assert_eq!(
        b.labelled(
            "An account key can change anything in your Bunny account. HTTPorter uses it only \
             for what you choose here."
        ),
        1
    );
    b.told();
    b.press(AccessibleRole::Button, "Add 1 Site");
    assert_eq!(b.told(), ["zones [true, false, false] false"]);
    b.press(AccessibleRole::RadioButton, keep);
    b.press(AccessibleRole::Button, "Add 1 Site");
    assert_eq!(b.told(), ["zones [true, false, false] true"]);
}

#[test]
fn welcome_texts_match_mockup() {
    testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    let log = Rc::new(RefCell::new(Vec::new()));
    log_round_two(&w, &log);
    w.on_add_site({
        let log = log.clone();
        move || log.borrow_mut().push("add-site".into())
    });
    w.show().unwrap();
    let count = |label: &str| ElementHandle::find_by_accessible_label(&w, label).count();
    for text in [
        "HTTPorter",
        "Fast, safe file transfers to cloud storage over HTTP.",
        "Made for HTTP storage",
        "Talks straight to Bunny Storage and S3-compatible services over HTTPS, sending many \
         files at once instead of one by one.",
        "Publish a whole folder",
        "Uploads only what changed, checked by checksum, then clears the CDN so visitors see it \
         right away.",
        "Safe by design",
        "You see every change before it happens. Passwords stay in Windows Credential Manager, \
         never in plain files.",
        "Add Your First Site",
        "Import from .env or AWS profile…",
        "Free software · No telemetry · Your files go only to the services you add",
        "No sites yet. Add one to start browsing and publishing.",
        "Publish profiles appear here after you add a site.",
        "No site connected",
    ] {
        assert!(count(text) > 0, "{text}");
    }
    // The toolbar is titled Welcome too.
    assert!(count("Welcome") >= 2, "the screen and the toolbar");

    // Import waits for the second round (Ruling Q10), and says so.
    let import = || {
        ElementHandle::find_by_accessible_label(&w, "Import from .env or AWS profile…")
            .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .unwrap()
    };
    assert_eq!(import().accessible_enabled(), Some(false));
    assert_eq!(
        import().accessible_description().as_deref(),
        Some("Arrives later in this build.")
    );
    import().invoke_accessible_default_action();
    assert_eq!(log.borrow().len(), 0);
    w.set_sites_v2_ready(true);
    assert_eq!(import().accessible_enabled(), Some(true));
    import().invoke_accessible_default_action();
    assert_eq!(log.borrow().as_slice(), ["welcome-import"]);
    log.borrow_mut().clear();

    // Add Your First Site opens the sheet and tells the app both ways.
    ElementHandle::find_by_accessible_label(&w, "Add Your First Site")
        .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .unwrap()
        .invoke_accessible_default_action();
    assert!(w.get_add_site_open());
    assert_eq!(log.borrow().as_slice(), ["add-site", "welcome-add"]);

    // With a site, the Welcome screen goes.
    w.set_add_site_open(false);
    w.set_sites(ModelRc::new(VecModel::from(vec![site(
        "example-shop.com",
        ProviderTile::Bunny,
        "B",
    )])));
    assert_eq!(
        count("Fast, safe file transfers to cloud storage over HTTP."),
        0
    );
    assert_eq!(count("Add Your First Site"), 0);
}
