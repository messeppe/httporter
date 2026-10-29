//! Keyboard operation and translation guard, driven headlessly with the testing backend.

use crate::{AppWindow, SiteItem};
use i_slint_backend_testing as testing;
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
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
fn tap(window: &slint::Window, key: impl Into<SharedString>) {
    let text: SharedString = key.into();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

fn site(name: &str, mono: &str) -> SiteItem {
    SiteItem {
        id: name.into(),
        name: name.into(),
        subtitle: "Bunny".into(),
        mono: mono.into(),
        healthy: true,
    }
}

fn window_with_two_sites() -> AppWindow {
    testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    w.set_sites(ModelRc::new(VecModel::from(vec![
        site("example-shop.com", "B"),
        site("media-archive", "R2"),
    ])));
    w.show().unwrap();
    w
}

fn tab(w: &AppWindow, times: usize) {
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
fn add_site_and_settings_work_from_the_keyboard() {
    let w = window_with_two_sites();
    let added = Rc::new(Cell::new(0));
    let settings = Rc::new(Cell::new(0));
    w.on_add_site({
        let added = added.clone();
        move || added.set(added.get() + 1)
    });
    w.on_open_settings({
        let settings = settings.clone();
        move || settings.set(settings.get() + 1)
    });
    tab(&w, 4);
    tap(w.window(), Key::Return);
    assert_eq!((added.get(), settings.get()), (1, 0));
    tab(&w, 1);
    tap(w.window(), Key::Space);
    assert_eq!((added.get(), settings.get()), (1, 1));
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

/// Every user-facing string in the .slint sources must go through @tr() so it can be
/// translated later. This is a text scan, not a parser: it checks the properties that carry
/// text shown on screen or read by a screen reader.
#[test]
fn user_facing_strings_are_wrapped_in_tr() {
    const PROPS: [&str; 6] = [
        "text",
        "label",
        "title",
        "accessible-label",
        "accessible-description",
        "placeholder-text",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    let mut files = vec![];
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "slint") {
                files.push(path);
            }
        }
    }
    assert!(files.len() >= 8, "found only {files:?}");

    let mut offenders = vec![];
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        for (n, line) in source.lines().enumerate() {
            let code = line.split("//").next().unwrap().trim();
            for prop in PROPS {
                let Some(value) = code
                    .strip_prefix(prop)
                    .and_then(|r| r.trim_start().strip_prefix(':'))
                else {
                    continue;
                };
                if value.contains('"') && !value.contains("@tr(") {
                    offenders.push(format!("{}:{}: {}", file.display(), n + 1, code));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "strings without @tr():\n{}",
        offenders.join("\n")
    );
}
