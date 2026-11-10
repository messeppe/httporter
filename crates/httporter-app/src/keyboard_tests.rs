// SPDX-License-Identifier: MIT OR Apache-2.0

//! Keyboard operation of the sidebar, the shared buttons and the window's shortcuts (Back and
//! Forward, Alt+arrows, F5), the Publish sheet's plan list and Purge, and Quick Look's Source /
//! Changes switch, driven headlessly with the testing backend. The panes' own keys are in
//! `pane_tests.rs`; the `@tr()` and copy scans are in `lint_tests.rs`.

use crate::pane_tests::{LOCAL_LIST, REMOTE_LIST, crumb_names, row_starting, window_with_panes};
use crate::{
    AppWindow, FileKind, PlanGroup, PlanRow, ProviderTile, PublishSheetData, QuickLookData,
    SiteItem,
};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, SharedString, VecModel};
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
    // Tab order: sidebar toggle, the toolbar's Two panes, Single pane and Synchronized browsing,
    // then the site list, one stop (ledger 155: Down moves along it). Back, Forward, Compare and
    // Publish are disabled.
    tab(&w, 5);
    tap(w.window(), Key::DownArrow);
    // The row the keyboard moves to takes the focus in a change handler, which the event loop
    // runs between events.
    slint::platform::update_timers_and_animations();
    tap(w.window(), Key::Return);
    assert_eq!(picked.get(), 1);
    assert_eq!(w.get_selected_site(), 1);
}

#[test]
fn space_on_a_site_row_selects_it() {
    let w = window_with_two_sites();
    tab(&w, 5);
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
    // Past the site list, one Tab stop however many sites (ledger 155).
    tab(&w, 6);
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
    tab(&w, 7);
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
    // It slides out first (I7), then goes.
    testing::mock_elapsed_time(std::time::Duration::from_millis(400));
    assert!(!w.get_sidebar_visible());
    assert_eq!(add_site_buttons(&w), 0, "the sidebar is gone");

    // The toolbar button still toggles from where the app left it.
    tab(&w, 1);
    tap(w.window(), Key::Return);
    assert!(w.get_sidebar_visible());
    // It slides in (I7).
    testing::mock_elapsed_time(std::time::Duration::from_millis(400));
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

pub(crate) fn toolbar_button(w: &AppWindow, label: &str) -> ElementHandle {
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

/// Holds `modifiers`, taps `key`, lets the modifiers go.
pub(crate) fn chord(window: &slint::Window, modifiers: &[Key], key: impl Into<SharedString>) {
    for modifier in modifiers {
        window.dispatch_event(WindowEvent::KeyPressed {
            text: (*modifier).into(),
        });
    }
    tap(window, key);
    for modifier in modifiers.iter().rev() {
        window.dispatch_event(WindowEvent::KeyReleased {
            text: (*modifier).into(),
        });
    }
}

/// What a control tells assistive technology after its label ("" for nothing).
pub(crate) fn description(e: &ElementHandle) -> String {
    e.accessible_description()
        .map(|d| d.to_string())
        .unwrap_or_default()
}

// ---- The toolbar's file actions and their shortcuts (Task 17) ----

const ACTIONS: [&str; 4] = ["Upload", "Download", "New folder", "Delete"];

/// How often each of the four commands was called, in the order of [`ACTIONS`].
fn count_actions(w: &AppWindow) -> Rc<[Cell<usize>; 4]> {
    let counts: Rc<[Cell<usize>; 4]> = Rc::new(Default::default());
    macro_rules! count {
        ($on:ident, $i:expr) => {{
            let counts = counts.clone();
            w.$on(move || counts[$i].set(counts[$i].get() + 1));
        }};
    }
    count!(on_upload, 0);
    count!(on_download, 1);
    count!(on_new_folder, 2);
    count!(on_delete, 3);
    counts
}

fn counted(counts: &[Cell<usize>; 4]) -> [usize; 4] {
    [
        counts[0].get(),
        counts[1].get(),
        counts[2].get(),
        counts[3].get(),
    ]
}

/// Task 17: each of Upload, Download, New folder and Delete is pressable only while the app says
/// so (`can-*`); the others stay off and say why; a pressed button reaches the app.
#[test]
fn toolbar_buttons_follow_rules() {
    let w = window_with_panes(3, 3);
    let counts = count_actions(&w);
    let enabled = |label: &str| toolbar_button(&w, label).accessible_enabled();

    // Until the app says otherwise they are all off, and pressing one does nothing.
    w.set_actions_note("Transfers arrive in a later version".into());
    for label in ACTIONS {
        assert_eq!(enabled(label), Some(false), "{label}");
        assert_eq!(
            description(&toolbar_button(&w, label)),
            "Transfers arrive in a later version",
            "{label} says why"
        );
        click(w.window(), center(&toolbar_button(&w, label)), 1);
        toolbar_button(&w, label).invoke_accessible_default_action();
    }
    assert_eq!(counted(&counts), [0, 0, 0, 0]);

    // Each one on its own.
    type Flag = (&'static str, fn(&AppWindow, bool));
    let flags: [Flag; 4] = [
        ("Upload", AppWindow::set_can_upload),
        ("Download", AppWindow::set_can_download),
        ("New folder", AppWindow::set_can_new_folder),
        ("Delete", AppWindow::set_can_delete),
    ];
    for (i, (label, set)) in flags.iter().enumerate() {
        set(&w, true);
        for other in ACTIONS {
            assert_eq!(enabled(other), Some(other == *label), "{label} on: {other}");
        }
        assert_eq!(
            description(&toolbar_button(&w, label)),
            "",
            "an enabled {label} has nothing to explain"
        );
        click(w.window(), center(&toolbar_button(&w, label)), 1);
        assert_eq!(counted(&counts)[i], 1, "{label}");
        assert_eq!(counted(&counts).iter().sum::<usize>(), i + 1);
        set(&w, false);
    }

    // They are Tab stops while they are on, and Return presses the one with the focus.
    w.set_can_upload(true);
    tab(&w, 5); // the sidebar toggle, the View group's three, then Upload
    tap(w.window(), Key::Return);
    assert_eq!(counted(&counts)[0], 2);
}

/// Task 17: with This PC in use Delete is off and the toolbar says to use Explorer; with a cloud
/// selection it is on and says nothing. Its shortcut follows the same flag.
#[test]
fn delete_disabled_on_this_pc_with_note() {
    let w = window_with_panes(3, 3);
    let counts = count_actions(&w);
    let note = "Delete works in the cloud pane. Use Explorer for files on this PC.";
    let delete = || toolbar_button(&w, "Delete");

    // This PC in use: the app turns Delete off and sets the note, over the "arrives later" one.
    w.set_actions_note("Transfers arrive in a later version".into());
    w.set_delete_note(note.into());
    assert_eq!(delete().accessible_enabled(), Some(false));
    assert_eq!(description(&delete()), note);
    // The other three keep the general note.
    assert_eq!(
        description(&toolbar_button(&w, "Upload")),
        "Transfers arrive in a later version"
    );
    w.set_actions_note("".into());
    assert_eq!(description(&delete()), note);
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::Delete);
    click(w.window(), center(&delete()), 1);
    assert_eq!(counted(&counts)[3], 0);

    // The cloud pane in use with rows selected: Delete is on and the note is gone.
    w.set_delete_note("".into());
    w.set_can_delete(true);
    assert_eq!(delete().accessible_enabled(), Some(true));
    assert_eq!(description(&delete()), "");
    click(w.window(), center(&delete()), 1);
    assert_eq!(counted(&counts)[3], 1);

    // Cloud pane in use, nothing selected: off, and nothing to say either.
    w.set_can_delete(false);
    assert_eq!(delete().accessible_enabled(), Some(false));
    assert_eq!(description(&delete()), "");
}

/// Ctrl+U, Ctrl+Shift+N and Del do what the buttons do, while the buttons can be pressed.
#[test]
fn ctrl_u_ctrl_shift_n_and_del_follow_the_can_flags() {
    let w = window_with_panes(3, 3);
    let counts = count_actions(&w);
    tab(&w, LOCAL_LIST);

    // Everything is off: the keys do nothing.
    chord(w.window(), &[Key::Control], "u");
    chord(w.window(), &[Key::Control, Key::Shift], "n");
    tap(w.window(), Key::Delete);
    assert_eq!(counted(&counts), [0, 0, 0, 0]);

    w.set_can_upload(true);
    w.set_can_new_folder(true);
    w.set_can_delete(true);
    chord(w.window(), &[Key::Control], "u");
    chord(w.window(), &[Key::Control], "U");
    chord(w.window(), &[Key::Control, Key::Shift], "n");
    chord(w.window(), &[Key::Control, Key::Shift], "N");
    tap(w.window(), Key::Delete);
    assert_eq!(counted(&counts), [2, 0, 2, 1]);

    // Not with the wrong modifiers, and not while a key is held down.
    chord(w.window(), &[], "u");
    chord(w.window(), &[Key::Control, Key::Alt], "u");
    chord(w.window(), &[Key::Control, Key::Shift], "u");
    chord(w.window(), &[Key::Control], "n");
    chord(w.window(), &[Key::Shift], "n");
    chord(w.window(), &[Key::Control], Key::Delete);
    chord(w.window(), &[Key::Shift], Key::Delete);
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    w.window()
        .dispatch_event(WindowEvent::KeyPressRepeated { text: "u".into() });
    w.window().dispatch_event(WindowEvent::KeyPressRepeated {
        text: Key::Delete.into(),
    });
    w.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    assert_eq!(counted(&counts), [2, 0, 2, 1]);

    // Turned off again, they stop at once.
    w.set_can_upload(false);
    w.set_can_delete(false);
    chord(w.window(), &[Key::Control], "u");
    tap(w.window(), Key::Delete);
    assert_eq!(counted(&counts), [2, 0, 2, 1]);
    // Not behind the Add Site sheet.
    w.set_can_upload(true);
    w.set_add_site_open(true);
    slint::platform::update_timers_and_animations();
    chord(w.window(), &[Key::Control], "u");
    assert_eq!(counted(&counts)[0], 2);
}

// ---- Task 25, items 3 and 4: the Alt key ----

/// Records Back and Forward as (which, whether This PC was the pane in use at that moment).
fn record_travel(w: &AppWindow) -> Rc<std::cell::RefCell<Vec<(&'static str, bool)>>> {
    let went = Rc::new(std::cell::RefCell::new(Vec::new()));
    w.on_go_back({
        let (went, weak) = (went.clone(), w.as_weak());
        move || {
            let local = weak.upgrade().unwrap().get_local_pane_active();
            went.borrow_mut().push(("back", local));
        }
    });
    w.on_go_forward({
        let (went, weak) = (went.clone(), w.as_weak());
        move || {
            let local = weak.upgrade().unwrap().get_local_pane_active();
            went.borrow_mut().push(("forward", local));
        }
    });
    went
}

/// Polish 3 (ledger 252): Alt+Left and Alt+Right act on the pane that has the focus, also when
/// the focus is on one of its breadcrumbs or column headers, not only its list.
#[test]
fn polish_3_alt_arrows_act_on_the_pane_with_focus_including_crumbs_and_headers() {
    let w = window_with_panes(3, 3);
    w.set_local_crumbs(crumb_names(&["D:", "sites", "uploads"]));
    w.set_remote_crumbs(crumb_names(&["zone", "wp-content", "uploads"]));
    // Both panes can go both ways, so the answer is in which pane is in use when it is asked.
    let setters: [fn(&AppWindow, bool); 4] = [
        AppWindow::set_local_can_go_back,
        AppWindow::set_local_can_go_forward,
        AppWindow::set_remote_can_go_back,
        AppWindow::set_remote_can_go_forward,
    ];
    for set in setters {
        set(&w, true);
    }
    let went = record_travel(&w);
    let alt = |key: Key| chord(w.window(), &[Key::Alt], key);

    // Tab stops: 1 the sidebar toggle, 2 Back and 3 Forward (both can be pressed here), 4 to 6
    // the View group (Two panes, Single pane, Synchronized browsing), 7 the site's row, 8 Add
    // Site, 9 This PC's list, 10 and 11 its two crumbs that are buttons, 12 its Refresh, 13 to 15
    // its column headers, 16 the cloud pane's list, 17 and 18 its crumbs, then (Refresh is off
    // with no site) 19 to 21 its headers.
    tab(&w, 10);
    alt(Key::LeftArrow);
    assert_eq!(*went.borrow(), [("back", true)], "from a crumb of This PC");
    tab(&w, 3);
    alt(Key::RightArrow);
    assert_eq!(
        went.borrow().last(),
        Some(&("forward", true)),
        "from the Name header of This PC"
    );

    tab(&w, 6);
    alt(Key::LeftArrow);
    assert_eq!(
        went.borrow().last(),
        Some(&("back", false)),
        "from the Name header of the cloud pane"
    );
    alt(Key::RightArrow);
    assert_eq!(went.borrow().last(), Some(&("forward", false)));
    // One stop back, a crumb of the cloud pane, is still the cloud pane.
    tap(w.window(), Key::Backtab);
    alt(Key::LeftArrow);
    assert_eq!(went.borrow().last(), Some(&("back", false)));

    // And back in This PC, at a header, with Shift+Tab: 18 down to 13.
    for _ in 0..5 {
        tap(w.window(), Key::Backtab);
    }
    alt(Key::LeftArrow);
    assert_eq!(went.borrow().last(), Some(&("back", true)));
    assert_eq!(went.borrow().len(), 6);
}

/// Polish 3 (ledger 252): the keys work as soon as the window opens, with nothing clicked or
/// tabbed to; the cloud pane is the one in use at first.
#[test]
fn polish_3_alt_arrows_work_at_startup_without_a_click() {
    let w = window_with_panes(3, 3);
    w.set_remote_can_go_back(true);
    w.set_remote_can_go_forward(true);
    let went = record_travel(&w);
    chord(w.window(), &[Key::Alt], Key::LeftArrow);
    chord(w.window(), &[Key::Alt], Key::RightArrow);
    assert_eq!(*went.borrow(), [("back", false), ("forward", false)]);

    // The same for F5.
    let refreshes = Rc::new(Cell::new(0));
    w.on_refresh({
        let refreshes = refreshes.clone();
        move || refreshes.set(refreshes.get() + 1)
    });
    tap(w.window(), Key::F5);
    assert_eq!(refreshes.get(), 1);
}

/// Polish 4 (ledger 252): Alt+Space (the window's system menu) and Alt+Return press nothing; the
/// same keys without Alt still do.
#[test]
fn polish_4_alt_space_and_alt_return_do_not_press_the_shared_buttons() {
    let t = testbed();
    tap(t.window(), Key::Tab);
    assert!(t.get_tool_focused());
    chord(t.window(), &[Key::Alt], Key::Space);
    chord(t.window(), &[Key::Alt], Key::Return);
    assert_eq!(t.get_tool_clicks(), 0);
    tap(t.window(), Key::Space);
    tap(t.window(), Key::Return);
    assert_eq!(t.get_tool_clicks(), 2);

    tap(t.window(), Key::Tab);
    assert!(t.get_capsule_focused());
    chord(t.window(), &[Key::Alt], Key::Space);
    chord(t.window(), &[Key::Alt], Key::Return);
    assert_eq!(t.get_capsule_clicks(), 0);
    tap(t.window(), Key::Return);
    assert_eq!(t.get_capsule_clicks(), 1);
}

/// Polish 4: also for the controls the window builds from them: a breadcrumb, a column header, a
/// site's row in the sidebar and the Add Site button.
#[test]
fn polish_4_alt_space_and_alt_return_do_not_press_crumbs_headers_rows_or_add_site() {
    let w = window_with_panes(3, 3);
    w.set_local_crumbs(crumb_names(&["D:", "sites", "uploads"]));
    let pressed = Rc::new(std::cell::RefCell::new(Vec::new()));
    w.on_local_crumb_clicked({
        let pressed = pressed.clone();
        move |i| pressed.borrow_mut().push(format!("crumb {i}"))
    });
    w.on_sort_local({
        let pressed = pressed.clone();
        move |c| pressed.borrow_mut().push(format!("sort {c}"))
    });
    w.on_site_selected({
        let pressed = pressed.clone();
        move |i| pressed.borrow_mut().push(format!("site {i}"))
    });
    w.on_add_site({
        let pressed = pressed.clone();
        move || pressed.borrow_mut().push("add site".to_owned())
    });
    let with_alt = |w: &AppWindow| {
        chord(w.window(), &[Key::Alt], Key::Space);
        chord(w.window(), &[Key::Alt], Key::Return);
    };

    // The sidebar's row (Tab 5, past the View group) and the Add Site button (Tab 6).
    tab(&w, 5);
    with_alt(&w);
    tab(&w, 1);
    with_alt(&w);
    // The first crumb (the list is between), then the Name header (past the other crumb and
    // Refresh).
    tab(&w, 2);
    with_alt(&w);
    tab(&w, 3);
    with_alt(&w);
    assert!(pressed.borrow().is_empty(), "{:?}", pressed.borrow());
    assert!(!w.get_add_site_open(), "Add Site did not open its sheet");

    // Without Alt, Return presses the one with the focus: the Name header sorts …
    tap(w.window(), Key::Return);
    assert_eq!(*pressed.borrow(), ["sort 0"]);
    // … the crumb goes to its folder …
    for _ in 0..3 {
        tap(w.window(), Key::Backtab);
    }
    tap(w.window(), Key::Return);
    assert_eq!(pressed.borrow().last().map(String::as_str), Some("crumb 0"));
    // … the sidebar's row picks its site …
    for _ in 0..2 {
        tap(w.window(), Key::Backtab);
    }
    tap(w.window(), Key::Return);
    assert_eq!(pressed.borrow().last().map(String::as_str), Some("site 0"));
    // … and Add Site is pressed (the sheet it opens comes last).
    tap(w.window(), Key::Tab);
    tap(w.window(), Key::Return);
    assert_eq!(
        pressed.borrow().last().map(String::as_str),
        Some("add site")
    );
}

// ---- The Publish sheet's plan list and Purge, and Quick Look's Source / Changes switch ----

/// Starts the testing backend once on this thread.
fn init_once() {
    thread_local! {
        static STARTED: Cell<bool> = const { Cell::new(false) };
    }
    if !STARTED.with(|started| started.replace(true)) {
        testing::init_no_event_loop();
    }
}

type Said = Rc<std::cell::RefCell<Vec<String>>>;

fn plan_row(path: &str, label: &str, checked: bool, header: bool) -> PlanRow {
    PlanRow {
        path: path.into(),
        kind: if header {
            FileKind::Folder
        } else {
            FileKind::Code
        },
        group: if label == "New" || path == "New" {
            PlanGroup::New
        } else {
            PlanGroup::Changed
        },
        label: label.into(),
        size: if header { "24 KB" } else { "12 KB" }.into(),
        checked,
        header,
        depth: 0,
        folder: false,
        expanded: false,
    }
}

/// The Publish sheet open on a plan of two groups (five rows), with nothing only on the site (so
/// Remove extras is off) and the purge on or off; what the sheet asks for is logged.
fn publish_window(purge_enabled: bool) -> (AppWindow, Said) {
    init_once();
    let w = AppWindow::new().unwrap();
    w.set_publish_sheet(PublishSheetData {
        title: "Publish to example-shop.com".into(),
        changed: 2,
        new: 1,
        purge_enabled,
        button_label: "Publish 3 Files".into(),
        can_publish: true,
        ..Default::default()
    });
    w.set_plan_rows(ModelRc::new(VecModel::from(vec![
        plan_row("Changed", "2 files", true, true),
        plan_row("index.html", "Changed", true, false),
        plan_row("about.html", "Changed", false, false),
        plan_row("New", "1 file", true, true),
        plan_row("site.css", "New", true, false),
    ])));
    let said: Said = Rc::default();
    w.on_plan_toggle({
        let said = said.clone();
        move |row| said.borrow_mut().push(format!("toggle {row}"))
    });
    w.on_plan_expand({
        let said = said.clone();
        move |row| said.borrow_mut().push(format!("expand {row}"))
    });
    w.on_publish_purge({
        let said = said.clone();
        move |mode| said.borrow_mut().push(format!("purge {mode}"))
    });
    w.on_publish_confirm({
        let said = said.clone();
        move || said.borrow_mut().push("confirm".to_owned())
    });
    w.set_publish_open(true);
    w.show().unwrap();
    slint::platform::update_timers_and_animations();
    (w, said)
}

fn keys(w: &AppWindow, keys: &[Key]) {
    for &key in keys {
        tap(w.window(), key);
    }
}

fn said(log: &Said) -> Vec<String> {
    std::mem::take(&mut *log.borrow_mut())
}

fn element(w: &AppWindow, role: AccessibleRole, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(w, label)
        .find(|e| e.accessible_role() == Some(role))
        .unwrap_or_else(|| panic!("no {role:?} {label:?}"))
}

/// Accessibility sweep: the plan list is one Tab stop (Shift+Tab from Cancel, the default focus,
/// past Export Plan…; Remove extras and Purge are off). Up, Down, Home, End, PageUp and PageDown
/// move the keyboard's row, Space ticks it through `plan-toggle` as a click does (a header's for
/// its group), and Return is still the sheet's Publish.
#[test]
fn the_plan_list_is_one_tab_stop_moved_by_keys_and_ticked_with_space() {
    let (w, log) = publish_window(false);
    keys(&w, &[Key::Backtab, Key::Backtab, Key::Space]);
    assert_eq!(said(&log), ["toggle 0"], "the list starts on its first row");

    keys(&w, &[Key::DownArrow, Key::DownArrow, Key::Space]);
    keys(&w, &[Key::UpArrow, Key::Space]);
    keys(&w, &[Key::End, Key::Space, Key::DownArrow, Key::Space]);
    keys(&w, &[Key::Home, Key::Space, Key::UpArrow, Key::Space]);
    assert_eq!(
        said(&log),
        [
            "toggle 2", "toggle 1", "toggle 4", "toggle 4", "toggle 0", "toggle 0"
        ]
    );

    // A page is the rows in view less one: three or four of the four or five shown.
    keys(&w, &[Key::PageDown, Key::Space]);
    let down = said(&log);
    assert!(
        down == ["toggle 3"] || down == ["toggle 4"],
        "PageDown went to {down:?}"
    );
    keys(&w, &[Key::PageUp, Key::Space]);
    assert_eq!(said(&log), ["toggle 0"]);

    // A held Space ticks once; Left and Right do nothing on a file (only a tree folder opens).
    w.window().dispatch_event(WindowEvent::KeyPressRepeated {
        text: Key::Space.into(),
    });
    keys(&w, &[Key::RightArrow, Key::LeftArrow]);
    assert!(said(&log).is_empty());

    tap(w.window(), Key::Return);
    assert_eq!(said(&log), ["confirm"]);
}

/// In a long plan the keyboard's row is scrolled into view: End shows the last row, Home the
/// first, and PageDown goes on by about a view at a time.
#[test]
fn the_plan_list_scrolls_the_keyboards_row_into_view() {
    let (w, log) = publish_window(false);
    let mut rows = vec![plan_row("Changed", "60 files", true, true)];
    rows.extend((1..=60).map(|i| plan_row(&format!("page-{i}.html"), "Changed", true, false)));
    w.set_plan_rows(ModelRc::new(VecModel::from(rows)));
    slint::platform::update_timers_and_animations();
    let list = element(&w, AccessibleRole::List, "Files to publish");
    let in_view = |label: &str| {
        let Some(row) = ElementHandle::find_by_accessible_label(&w, label)
            .find(|e| e.accessible_role() == Some(AccessibleRole::Checkbox))
        else {
            return false;
        };
        let (top, bottom) = (
            list.absolute_position().y,
            list.absolute_position().y + list.size().height,
        );
        let y = row.absolute_position().y;
        y >= top - 0.5 && y + row.size().height <= bottom + 0.5
    };
    assert!(!in_view("page-60.html, Changed"));

    keys(&w, &[Key::Backtab, Key::Backtab, Key::End, Key::Space]);
    assert_eq!(said(&log), ["toggle 60"]);
    assert!(in_view("page-60.html, Changed"), "End shows the last row");
    assert!(!in_view("page-1.html, Changed"));

    keys(&w, &[Key::Home, Key::PageDown, Key::PageDown, Key::Space]);
    let toggled = said(&log);
    let row: usize = toggled[0].trim_start_matches("toggle ").parse().unwrap();
    assert!(row >= 6, "two pages down from the top went to row {row}");
    assert!(in_view(&format!("page-{row}.html, Changed")));
    assert!(!in_view("page-1.html, Changed"));

    keys(&w, &[Key::Home]);
    assert!(in_view("Changed, 60 files"), "Home shows the first row");
}

/// Narrator reads a plan row as its path and what happens to it, then its state ("index.html,
/// Changed, checked"), and a header as its group and count. The list says how many rows it has.
#[test]
fn plan_rows_tell_assistive_technology_what_they_are() {
    let (w, _log) = publish_window(false);
    let row = element(&w, AccessibleRole::Checkbox, "index.html, Changed");
    assert_eq!(row.accessible_checkable(), Some(true));
    assert_eq!(row.accessible_checked(), Some(true));
    assert_eq!(row.accessible_item_index(), Some(1));
    assert_eq!(row.accessible_item_count(), Some(5));
    assert_eq!(description(&row), "12 KB");
    let unticked = element(&w, AccessibleRole::Checkbox, "about.html, Changed");
    assert_eq!(unticked.accessible_checked(), Some(false));
    let header = element(&w, AccessibleRole::Checkbox, "Changed, 2 files");
    assert_eq!(header.accessible_checked(), Some(true));
    let list = element(&w, AccessibleRole::List, "Files to publish");
    assert_eq!(list.accessible_item_count(), Some(5));
}

/// A tree folder opens with Right and closes with Left, through `plan-expand` as its triangle.
#[test]
fn a_tree_folder_opens_with_right_and_closes_with_left() {
    let (w, log) = publish_window(false);
    let folder = |expanded: bool| PlanRow {
        kind: FileKind::Folder,
        depth: 1,
        folder: true,
        expanded,
        ..plan_row("images", "2 files", true, false)
    };
    let rows = w.get_plan_rows();
    rows.set_row_data(1, folder(false));
    keys(&w, &[Key::Backtab, Key::Backtab, Key::DownArrow]);
    keys(&w, &[Key::LeftArrow, Key::RightArrow]);
    assert_eq!(
        said(&log),
        ["expand 1"],
        "Left does nothing on a closed folder"
    );
    rows.set_row_data(1, folder(true));
    keys(&w, &[Key::RightArrow, Key::LeftArrow]);
    assert_eq!(
        said(&log),
        ["expand 1"],
        "Right does nothing on an open folder"
    );
}

/// Accessibility sweep: the Purge control turned off (an S3 site) says it is off and takes no
/// focus, as a turned-off button: Shift+Tab goes from Export Plan… straight to the list.
#[test]
fn the_purge_control_turned_off_takes_no_focus() {
    let (w, log) = publish_window(false);
    let purge = element(&w, AccessibleRole::RadioGroup, "Purge CDN after publishing");
    assert_eq!(purge.accessible_enabled(), Some(false));
    keys(
        &w,
        &[Key::Backtab, Key::Backtab, Key::RightArrow, Key::Space],
    );
    assert_eq!(said(&log), ["toggle 0"]);
}

/// Turned on, the Purge control is the Tab stop before Export Plan… and the arrows choose.
#[test]
fn the_purge_control_turned_on_is_reached_and_chosen_with_the_arrows() {
    let (w, log) = publish_window(true);
    let purge = element(&w, AccessibleRole::RadioGroup, "Purge CDN after publishing");
    assert_eq!(purge.accessible_enabled(), Some(true));
    keys(
        &w,
        &[Key::Backtab, Key::Backtab, Key::RightArrow, Key::RightArrow],
    );
    assert_eq!(said(&log), ["purge 1", "purge 2"]);
}

/// Quick Look open on a text file that can (or can't) be compared; what it asks for is logged.
fn quick_look_window(can_diff: bool) -> (AppWindow, Said) {
    init_once();
    let w = AppWindow::new().unwrap();
    w.set_ql(QuickLookData {
        title: "index.html".into(),
        kind: 0,
        can_diff,
        diff_label: "Changes vs Bunny".into(),
        diff_note: "Nothing to compare it with.".into(),
        ..Default::default()
    });
    let said: Said = Rc::default();
    w.on_ql_set_mode({
        let said = said.clone();
        move |mode| said.borrow_mut().push(format!("mode {mode}"))
    });
    w.on_ql_step({
        let said = said.clone();
        move |by| said.borrow_mut().push(format!("step {by}"))
    });
    w.on_ql_open_with({
        let said = said.clone();
        move || said.borrow_mut().push("open with".to_owned())
    });
    w.on_ql_close({
        let said = said.clone();
        move || said.borrow_mut().push("close".to_owned())
    });
    w.set_ql_open(true);
    w.show().unwrap();
    slint::platform::update_timers_and_animations();
    (w, said)
}

/// Accessibility sweep: Quick Look's Source / Changes switch is the Tab stop after the close
/// button. Space or Return flips the view; ← and → stay Quick Look's previous and next file there
/// too, and Esc still closes.
#[test]
fn the_quick_look_switch_flips_with_space_and_keeps_the_arrows_for_files() {
    let (w, log) = quick_look_window(true);
    // The panel has the keys at first: → is the next file.
    tap(w.window(), Key::RightArrow);
    assert_eq!(said(&log), ["step 1"]);

    keys(&w, &[Key::Tab, Key::Tab, Key::Space]);
    assert_eq!(said(&log), ["mode 1"]);
    assert_eq!(w.get_ql_mode(), 1);
    let switch = element(&w, AccessibleRole::Switch, "Changes vs Bunny");
    assert_eq!(switch.accessible_checked(), Some(true));

    // The switch keeps the focus: the arrows step through the files, Return flips back.
    keys(&w, &[Key::LeftArrow, Key::RightArrow, Key::Return]);
    assert_eq!(said(&log), ["step -1", "step 1", "mode 0"]);
    assert_eq!(switch.accessible_checked(), Some(false));

    tap(w.window(), Key::Escape);
    assert_eq!(said(&log), ["close"]);
}

/// With nothing to compare the switch can't switch: it takes no focus (Tab goes from the close
/// button to Open With…, Open in Browser being off) and says why.
#[test]
fn the_quick_look_switch_takes_no_focus_with_nothing_to_compare() {
    let (w, log) = quick_look_window(false);
    let switch = element(&w, AccessibleRole::Switch, "Changes vs Bunny");
    assert_eq!(switch.accessible_enabled(), Some(false));
    assert_eq!(description(&switch), "Nothing to compare it with.");
    keys(&w, &[Key::Tab, Key::Tab, Key::Return]);
    assert_eq!(said(&log), ["open with"]);
    assert_eq!(w.get_ql_mode(), 0);
}
