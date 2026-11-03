//! Keyboard operation of the sidebar, the shared buttons and the window's shortcuts (Back and
//! Forward, Alt+arrows, F5), driven headlessly with the testing backend. The panes' own keys are
//! in `pane_tests.rs`; the `@tr()` and copy scans are in `lint_tests.rs`.

use crate::pane_tests::{LOCAL_LIST, REMOTE_LIST, row_starting, window_with_panes};
use crate::{AppWindow, ProviderTile, SiteItem};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use std::cell::Cell;
use std::rc::Rc;

slint::slint! {
    #[style = "cupertino"]
    import { CapsuleButton, ToolButton } from "../ui/components/buttons.slint";

    // Two shared buttons side by side, counting their `clicked` callbacks.
    export component Testbed inherits Window {
        width: 300px;
        height: 100px;
        in property <bool> buttons-enabled: true;
        out property <int> tool-clicks;
        out property <int> capsule-clicks;
        out property <bool> tool-focused: tool.has-focus;
        out property <bool> capsule-focused: capsule.has-focus;

        HorizontalLayout {
            alignment: start;
            spacing: 10px;
            tool := ToolButton {
                label: "Tool";
                enabled: root.buttons-enabled;
                clicked => { root.tool-clicks += 1; }
            }
            capsule := CapsuleButton {
                text: "Capsule";
                enabled: root.buttons-enabled;
                clicked => { root.capsule-clicks += 1; }
            }
        }
    }
}

fn testbed() -> Testbed {
    testing::init_no_event_loop();
    let t = Testbed::new().unwrap();
    t.show().unwrap();
    t
}

/// Press and release one key in the window.
pub(crate) fn tap(window: &slint::Window, key: impl Into<SharedString>) {
    let text: SharedString = key.into();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

pub(crate) fn site(name: &str, provider: ProviderTile, mono: &str) -> SiteItem {
    SiteItem {
        id: name.into(),
        name: name.into(),
        subtitle: "Bunny".into(),
        provider,
        mono: mono.into(),
        healthy: true,
    }
}

fn window_with_two_sites() -> AppWindow {
    testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    w.set_sites(ModelRc::new(VecModel::from(vec![
        site("example-shop.com", ProviderTile::Bunny, "B"),
        site("media-archive", ProviderTile::R2, "R2"),
    ])));
    w.show().unwrap();
    w
}

pub(crate) fn tab(w: &AppWindow, times: usize) {
    for _ in 0..times {
        tap(w.window(), Key::Tab);
    }
}

#[test]
fn enter_on_a_site_row_selects_it() {
    let w = window_with_two_sites();
    let picked = Rc::new(Cell::new(-1));
    w.on_site_selected({
        let picked = picked.clone();
        move |i| picked.set(i)
    });
    // Tab order: sidebar toggle, row 0, row 1 (Back, Forward and Publish are disabled).
    tab(&w, 3);
    tap(w.window(), Key::Return);
    assert_eq!(picked.get(), 1);
    assert_eq!(w.get_selected_site(), 1);
}

#[test]
fn space_on_a_site_row_selects_it() {
    let w = window_with_two_sites();
    tab(&w, 2);
    tap(w.window(), Key::Space);
    assert_eq!(w.get_selected_site(), 0);
}

#[test]
fn add_site_works_from_the_keyboard() {
    let w = window_with_two_sites();
    let added = Rc::new(Cell::new(0));
    w.on_add_site({
        let added = added.clone();
        move || added.set(added.get() + 1)
    });
    tab(&w, 4);
    tap(w.window(), Key::Return);
    assert_eq!(added.get(), 1);
    // Add Site opens a modal sheet; closing it puts the focus back on the button.
    assert!(w.get_add_site_open());
    tap(w.window(), Key::Escape);
    assert!(!w.get_add_site_open());
    slint::platform::update_timers_and_animations();
    tap(w.window(), Key::Space);
    assert_eq!(added.get(), 2);
}

/// Ruling P22: there is no Settings window yet, so the gear is shown turned off and says why,
/// instead of doing nothing when pressed.
#[test]
fn settings_is_turned_off_until_it_exists_and_says_so() {
    let w = window_with_two_sites();
    let opened = Rc::new(Cell::new(0));
    w.on_open_settings({
        let opened = opened.clone();
        move || opened.set(opened.get() + 1)
    });
    let gear = ElementHandle::find_by_accessible_label(&w, "Settings")
        .next()
        .expect("the Settings button is shown");
    assert_eq!(gear.accessible_role(), Some(AccessibleRole::Button));
    assert_eq!(gear.accessible_enabled(), Some(false));
    assert_eq!(
        gear.accessible_description().as_deref(),
        Some("Settings arrive in a later version")
    );
    // Not a Tab stop: after Add Site comes the This PC list.
    assert!(!w.get_local_pane_active());
    tab(&w, 5);
    assert!(
        w.get_local_pane_active(),
        "the focus is in the This PC list"
    );
    // Neither the keyboard, the mouse nor assistive technology can press it.
    tap(w.window(), Key::Return);
    click(w.window(), center(&gear), 1);
    gear.invoke_accessible_default_action();
    assert_eq!(opened.get(), 0);
}

#[test]
fn sidebar_toggle_works_from_the_keyboard() {
    let w = window_with_two_sites();
    assert!(w.get_sidebar_visible());
    tab(&w, 1);
    tap(w.window(), Key::Return);
    assert!(!w.get_sidebar_visible());
    tap(w.window(), Key::Space);
    assert!(w.get_sidebar_visible());
}

/// Ledger 152: `sidebar-visible` is `in-out`, so the app can restore a hidden sidebar (and hide
/// it) as well as the toolbar button.
#[test]
fn the_app_can_hide_and_show_the_sidebar() {
    let w = window_with_two_sites();
    let add_site_buttons = |w: &AppWindow| {
        ElementHandle::find_by_accessible_label(w, "Add Site")
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .count()
    };
    assert!(w.get_sidebar_visible());
    assert_eq!(add_site_buttons(&w), 1);

    w.set_sidebar_visible(false);
    slint::platform::update_timers_and_animations();
    assert!(!w.get_sidebar_visible());
    assert_eq!(add_site_buttons(&w), 0, "the sidebar is gone");

    // The toolbar button still toggles from where the app left it.
    tab(&w, 1);
    tap(w.window(), Key::Return);
    assert!(w.get_sidebar_visible());
    assert_eq!(add_site_buttons(&w), 1);
}

#[test]
fn buttons_take_focus_in_order_and_activate_on_enter_and_space() {
    let t = testbed();
    assert!(!t.get_tool_focused() && !t.get_capsule_focused());

    tap(t.window(), Key::Tab);
    assert!(t.get_tool_focused() && !t.get_capsule_focused());
    tap(t.window(), Key::Return);
    tap(t.window(), Key::Space);
    assert_eq!((t.get_tool_clicks(), t.get_capsule_clicks()), (2, 0));

    tap(t.window(), Key::Tab);
    assert!(!t.get_tool_focused() && t.get_capsule_focused());
    tap(t.window(), Key::Return);
    tap(t.window(), Key::Space);
    assert_eq!((t.get_tool_clicks(), t.get_capsule_clicks()), (2, 2));

    // Shift+Tab goes back.
    tap(t.window(), Key::Backtab);
    assert!(t.get_tool_focused() && !t.get_capsule_focused());
}

#[test]
fn other_keys_and_key_repeat_do_not_activate() {
    let t = testbed();
    tap(t.window(), Key::Tab);
    tap(t.window(), "a");
    tap(t.window(), Key::Escape);
    t.window().dispatch_event(WindowEvent::KeyPressRepeated {
        text: Key::Return.into(),
    });
    t.window().dispatch_event(WindowEvent::KeyPressRepeated {
        text: Key::Space.into(),
    });
    assert_eq!(t.get_tool_clicks(), 0);
    tap(t.window(), Key::Return);
    assert_eq!(t.get_tool_clicks(), 1);
}

#[test]
fn disabled_buttons_take_no_focus_and_do_not_activate() {
    let t = testbed();
    t.set_buttons_enabled(false);
    tab_twice_then_activate(&t);
    assert!(!t.get_tool_focused() && !t.get_capsule_focused());
    assert_eq!((t.get_tool_clicks(), t.get_capsule_clicks()), (0, 0));
}

#[test]
fn a_button_disabled_while_focused_stops_activating() {
    let t = testbed();
    tap(t.window(), Key::Tab);
    t.set_buttons_enabled(false);
    tap(t.window(), Key::Return);
    tap(t.window(), Key::Space);
    assert_eq!(t.get_tool_clicks(), 0);
}

fn tab_twice_then_activate(t: &Testbed) {
    tap(t.window(), Key::Tab);
    tap(t.window(), Key::Tab);
    tap(t.window(), Key::Return);
    tap(t.window(), Key::Space);
}

pub(crate) fn center(e: &ElementHandle) -> LogicalPosition {
    let (p, s) = (e.absolute_position(), e.size());
    LogicalPosition::new(p.x + s.width / 2.0, p.y + s.height / 2.0)
}

pub(crate) fn click(window: &slint::Window, at: LogicalPosition, times: usize) {
    window.dispatch_event(WindowEvent::PointerMoved { position: at });
    for _ in 0..times {
        window.dispatch_event(WindowEvent::PointerPressed {
            position: at,
            button: PointerEventButton::Left,
        });
        window.dispatch_event(WindowEvent::PointerReleased {
            position: at,
            button: PointerEventButton::Left,
        });
    }
}

fn toolbar_button(w: &AppWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .unwrap_or_else(|| panic!("no {label} button"))
}

/// Ruling P22: Back and Forward follow the pane last used or focused, the cloud pane at first.
#[test]
fn back_and_forward_are_enabled_for_the_pane_last_used() {
    let w = window_with_panes(3, 3);
    w.set_remote_can_go_back(true);
    w.set_local_can_go_forward(true);
    let enabled = |label| toolbar_button(&w, label).accessible_enabled();
    assert_eq!(
        (enabled("Back"), enabled("Forward")),
        (Some(true), Some(false))
    );

    // A click in This PC makes it the pane they act on.
    click(w.window(), center(&row_starting(&w, "local-00001")), 1);
    assert!(w.get_local_pane_active());
    assert_eq!(
        (enabled("Back"), enabled("Forward")),
        (Some(false), Some(true))
    );

    // So does the keyboard: Tab into the cloud pane's list (past This PC's Refresh button and
    // its three column headers).
    tab(&w, 5);
    assert!(!w.get_local_pane_active());
    assert_eq!(
        (enabled("Back"), enabled("Forward")),
        (Some(true), Some(false))
    );

    let went = Rc::new(std::cell::RefCell::new(Vec::new()));
    for (name, back) in [("back", true), ("forward", false)] {
        let went = went.clone();
        let record = move || went.borrow_mut().push(name);
        if back {
            w.on_go_back(record);
        } else {
            w.on_go_forward(record);
        }
    }
    click(w.window(), center(&toolbar_button(&w, "Back")), 1);
    click(w.window(), center(&toolbar_button(&w, "Forward")), 1);
    assert_eq!(*went.borrow(), ["back"]);
}

/// Alt+Left and Alt+Right press Back and Forward, the Windows way, while they can be pressed.
#[test]
fn alt_left_and_alt_right_go_back_and_forward() {
    let w = window_with_panes(3, 3);
    let went = Rc::new(std::cell::RefCell::new(Vec::new()));
    w.on_go_back({
        let went = went.clone();
        move || went.borrow_mut().push("back")
    });
    w.on_go_forward({
        let went = went.clone();
        move || went.borrow_mut().push("forward")
    });
    let with_alt = |key: Key| {
        w.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Alt.into(),
        });
        tap(w.window(), key);
        w.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Alt.into(),
        });
    };
    tab(&w, LOCAL_LIST);
    with_alt(Key::LeftArrow);
    assert!(went.borrow().is_empty(), "nowhere to go back to");

    w.set_local_can_go_back(true);
    w.set_local_can_go_forward(true);
    with_alt(Key::LeftArrow);
    with_alt(Key::RightArrow);
    assert_eq!(*went.borrow(), ["back", "forward"]);
    // Without Alt, the arrows still move the selection and go nowhere.
    tap(w.window(), Key::LeftArrow);
    assert_eq!(went.borrow().len(), 2);

    // Not behind the Add Site sheet.
    w.set_add_site_open(true);
    slint::platform::update_timers_and_animations();
    with_alt(Key::LeftArrow);
    assert_eq!(went.borrow().len(), 2);
}

/// Minor 3: F5 and Ctrl+R refresh (the window picks the pane last used), once per press.
#[test]
fn f5_and_ctrl_r_refresh() {
    let w = window_with_panes(3, 3);
    let refreshes = Rc::new(Cell::new(0));
    w.on_refresh({
        let refreshes = refreshes.clone();
        move || refreshes.set(refreshes.get() + 1)
    });
    let with_control = |key: &str| {
        w.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Control.into(),
        });
        tap(w.window(), key);
        w.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
    };
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::F5);
    assert_eq!(refreshes.get(), 1);
    with_control("r");
    assert_eq!(refreshes.get(), 2);
    // From the cloud pane's list too.
    tab(&w, REMOTE_LIST - LOCAL_LIST);
    assert!(!w.get_local_pane_active());
    tap(w.window(), Key::F5);
    assert_eq!(refreshes.get(), 3);

    // A held key refreshes once, and a plain R is just a letter.
    w.window().dispatch_event(WindowEvent::KeyPressRepeated {
        text: Key::F5.into(),
    });
    tap(w.window(), "r");
    assert_eq!(refreshes.get(), 3);

    // Not behind the Add Site sheet.
    w.set_add_site_open(true);
    slint::platform::update_timers_and_animations();
    tap(w.window(), Key::F5);
    with_control("r");
    assert_eq!(refreshes.get(), 3);
}
