//! Keyboard operation and translation guard, driven headlessly with the testing backend.

use crate::viewmodel::{FileKindData, RowData, to_slint_rows};
use crate::{AppWindow, SiteItem};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
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

pub(crate) fn site(name: &str, mono: &str) -> SiteItem {
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
    // Add Site opens a modal sheet; closing it puts the focus back on the button.
    assert!(w.get_add_site_open());
    tap(w.window(), Key::Escape);
    assert!(!w.get_add_site_open());
    slint::platform::update_timers_and_animations();
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

// ---- File panes ----

slint::slint! {
    #[style = "cupertino"]
    import { Pane } from "../ui/components/pane.slint";
    import { FileRow } from "../ui/types.slint";
    export { FileRow }

    // One pane on its own, exposing its focus state.
    export component PaneBed inherits Window {
        width: 600px;
        height: 400px;
        in property <[FileRow]> rows;
        in property <[string]> crumbs;
        out property <bool> focused: pane.has-focus;
        out property <bool> ring: pane.focus-visible;
        callback crumb-clicked(int);

        pane := Pane {
            width: parent.width;
            height: parent.height;
            title: "Bed";
            rows: root.rows;
            crumbs: root.crumbs;
            crumb-clicked(index) => { root.crumb-clicked(index); }
        }
    }
}

fn row_data(prefix: &str, count: usize) -> Vec<RowData> {
    (0..count)
        .map(|i| RowData {
            name: format!("{prefix}-{i:05}.txt"),
            kind: FileKindData::Doc,
            size: "1 B".into(),
            modified: "Today 10:02".into(),
            folder: i % 5 == 0,
        })
        .collect()
}

fn window_with_panes(local: usize, remote: usize) -> AppWindow {
    testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    w.set_local_rows(to_slint_rows(&row_data("local", local)));
    w.set_remote_rows(to_slint_rows(&row_data("remote", remote)));
    w.show().unwrap();
    w
}

// Tab order with no sites: sidebar toggle, Add Site, Settings (Back, Forward and Publish are
// disabled), then each pane: its list, then its Name, Size and Modified headers.
const LOCAL_LIST: usize = 4;
const LOCAL_NAME_HEADER: usize = 5;
const REMOTE_LIST: usize = 8;

fn list_items(w: &impl ComponentHandle) -> Vec<ElementHandle> {
    ElementQuery::from_root(w)
        .match_accessible_role(AccessibleRole::ListItem)
        .find_all()
}

pub(crate) fn row_starting(w: &impl ComponentHandle, name: &str) -> ElementHandle {
    let name = name.to_owned();
    ElementQuery::from_root(w)
        .match_accessible_role(AccessibleRole::ListItem)
        .match_predicate({
            let name = name.clone();
            move |e| e.accessible_label().is_some_and(|l| l.starts_with(&name))
        })
        .find_first()
        .unwrap_or_else(|| panic!("no row starts with {name:?}"))
}

fn pane_named(w: &AppWindow, title: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(w, title)
        .find(|e| e.accessible_role() == Some(AccessibleRole::Region))
        .unwrap_or_else(|| panic!("no pane named {title:?}"))
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

/// True when the row is fully inside the pane's list: below its two header rows (63 px) and above
/// its bottom edge.
fn fully_visible(row: &ElementHandle, pane: &ElementHandle) -> bool {
    let (r, rs) = (row.absolute_position(), row.size());
    let (p, ps) = (pane.absolute_position(), pane.size());
    r.y >= p.y + 63.0 - 0.5 && r.y + rs.height <= p.y + ps.height + 0.5
}

#[test]
fn up_and_down_move_the_selection_and_stop_at_the_ends() {
    let w = window_with_panes(3, 3);
    tab(&w, LOCAL_LIST);
    assert_eq!(w.get_local_selected(), -1);
    tap(w.window(), Key::DownArrow);
    assert_eq!(w.get_local_selected(), 0);
    tap(w.window(), Key::DownArrow);
    tap(w.window(), Key::DownArrow);
    tap(w.window(), Key::DownArrow);
    assert_eq!(w.get_local_selected(), 2);
    tap(w.window(), Key::UpArrow);
    assert_eq!(w.get_local_selected(), 1);
    tap(w.window(), Key::Home);
    assert_eq!(w.get_local_selected(), 0);
    tap(w.window(), Key::UpArrow);
    assert_eq!(w.get_local_selected(), 0);
    tap(w.window(), Key::End);
    assert_eq!(w.get_local_selected(), 2);
    // The other pane is untouched.
    assert_eq!(w.get_remote_selected(), -1);
}

#[test]
fn up_with_no_selection_starts_from_the_last_row() {
    let w = window_with_panes(4, 0);
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::UpArrow);
    assert_eq!(w.get_local_selected(), 3);
}

#[test]
fn arrow_keys_do_nothing_in_an_empty_pane() {
    let w = window_with_panes(0, 0);
    tab(&w, LOCAL_LIST);
    for key in [
        Key::DownArrow,
        Key::UpArrow,
        Key::Home,
        Key::End,
        Key::PageDown,
    ] {
        tap(w.window(), key);
    }
    tap(w.window(), Key::Return);
    assert_eq!(w.get_local_selected(), -1);
}

#[test]
fn enter_opens_the_selected_row_and_backspace_goes_up() {
    let w = window_with_panes(3, 3);
    let opened = Rc::new(Cell::new(-1));
    let ups = Rc::new(Cell::new(0));
    w.on_open_local({
        let opened = opened.clone();
        move |i| opened.set(i)
    });
    w.on_local_up({
        let ups = ups.clone();
        move || ups.set(ups.get() + 1)
    });
    tab(&w, LOCAL_LIST);
    // Nothing is selected yet, so Enter has nothing to open.
    tap(w.window(), Key::Return);
    assert_eq!(opened.get(), -1);
    tap(w.window(), Key::DownArrow);
    tap(w.window(), Key::DownArrow);
    tap(w.window(), Key::Return);
    assert_eq!(opened.get(), 1);
    tap(w.window(), Key::Backspace);
    assert_eq!(ups.get(), 1);
}

#[test]
fn the_cloud_pane_has_its_own_selection_and_callbacks() {
    let w = window_with_panes(3, 3);
    let opened = Rc::new(Cell::new(-1));
    let ups = Rc::new(Cell::new(0));
    let local_calls = Rc::new(Cell::new(0));
    w.on_open_remote({
        let opened = opened.clone();
        move |i| opened.set(i)
    });
    w.on_remote_up({
        let ups = ups.clone();
        move || ups.set(ups.get() + 1)
    });
    w.on_open_local({
        let local_calls = local_calls.clone();
        move |_| local_calls.set(local_calls.get() + 1)
    });
    w.on_local_up({
        let local_calls = local_calls.clone();
        move || local_calls.set(local_calls.get() + 1)
    });
    tab(&w, REMOTE_LIST);
    tap(w.window(), Key::End);
    tap(w.window(), Key::Return);
    tap(w.window(), Key::Backspace);
    assert_eq!(
        (w.get_remote_selected(), opened.get(), ups.get()),
        (2, 2, 1)
    );
    assert_eq!((w.get_local_selected(), local_calls.get()), (-1, 0));
}

#[test]
fn key_repeat_moves_the_selection_but_never_opens_or_goes_up() {
    let w = window_with_panes(5, 0);
    let calls = Rc::new(Cell::new(0));
    w.on_open_local({
        let calls = calls.clone();
        move |_| calls.set(calls.get() + 1)
    });
    w.on_local_up({
        let calls = calls.clone();
        move || calls.set(calls.get() + 1)
    });
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::DownArrow);
    for _ in 0..2 {
        w.window().dispatch_event(WindowEvent::KeyPressRepeated {
            text: Key::DownArrow.into(),
        });
    }
    assert_eq!(w.get_local_selected(), 2);
    for key in [Key::Return, Key::Backspace] {
        w.window()
            .dispatch_event(WindowEvent::KeyPressRepeated { text: key.into() });
    }
    assert_eq!(calls.get(), 0);
}

#[test]
fn shortcuts_with_a_modifier_are_left_to_the_app() {
    let w = window_with_panes(5, 0);
    let ups = Rc::new(Cell::new(0));
    w.on_local_up({
        let ups = ups.clone();
        move || ups.set(ups.get() + 1)
    });
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::DownArrow);
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    tap(w.window(), Key::DownArrow);
    tap(w.window(), Key::Backspace);
    w.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    assert_eq!((w.get_local_selected(), ups.get()), (0, 0));
    tap(w.window(), Key::DownArrow);
    assert_eq!(w.get_local_selected(), 1);
}

#[test]
fn column_headers_sort_from_the_keyboard() {
    let w = window_with_panes(3, 0);
    let sorted = Rc::new(Cell::new(-1));
    w.on_sort_local({
        let sorted = sorted.clone();
        move |c| sorted.set(c)
    });
    let state = |w: &AppWindow| (w.get_local_sort_column(), w.get_local_sort_ascending());
    assert_eq!(state(&w), (0, true));

    tab(&w, LOCAL_NAME_HEADER);
    tap(w.window(), Key::Return);
    // Pressing the sorted column again reverses it.
    assert_eq!(sorted.get(), 0);
    assert_eq!(state(&w), (0, false));

    tab(&w, 1);
    tap(w.window(), Key::Space);
    // Another column starts ascending.
    assert_eq!(sorted.get(), 1);
    assert_eq!(state(&w), (1, true));

    tab(&w, 1);
    tap(w.window(), Key::Return);
    assert_eq!(sorted.get(), 2);
    assert_eq!(state(&w), (2, true));
    tap(w.window(), Key::Return);
    assert_eq!(state(&w), (2, false));
    // The cloud pane keeps its own sort.
    assert_eq!(
        (w.get_remote_sort_column(), w.get_remote_sort_ascending()),
        (0, true)
    );
}

#[test]
fn clicking_a_row_selects_it_and_double_clicking_opens_it() {
    let w = window_with_panes(6, 6);
    let opened = Rc::new(Cell::new(-1));
    w.on_open_local({
        let opened = opened.clone();
        move |i| opened.set(i)
    });
    click(w.window(), center(&row_starting(&w, "local-00003")), 1);
    assert_eq!((w.get_local_selected(), opened.get()), (3, -1));
    // The click also gives the pane the keyboard focus.
    tap(w.window(), Key::DownArrow);
    assert_eq!(w.get_local_selected(), 4);
    click(w.window(), center(&row_starting(&w, "local-00002")), 2);
    assert_eq!((w.get_local_selected(), opened.get()), (2, 2));
    assert_eq!(w.get_remote_selected(), -1);
}

#[test]
fn clicking_a_column_header_sorts_by_it() {
    let w = window_with_panes(3, 0);
    let sorted = Rc::new(Cell::new(-1));
    w.on_sort_local({
        let sorted = sorted.clone();
        move |c| sorted.set(c)
    });
    let size = ElementHandle::find_by_accessible_label(&w, "Sort by Size")
        .next()
        .unwrap();
    click(w.window(), center(&size), 1);
    assert_eq!(sorted.get(), 1);
    assert_eq!(
        (w.get_local_sort_column(), w.get_local_sort_ascending()),
        (1, true)
    );
    // The Size header is now the sorted one, and Name no longer is.
    let count = |label| ElementHandle::find_by_accessible_label(&w, label).count();
    assert_eq!(count("Size, sorted ascending"), 1);
    assert_eq!(count("Name, sorted ascending"), 1, "only the cloud pane's");
}

#[test]
fn rows_and_headers_are_described_for_screen_readers() {
    let w = window_with_panes(3, 2);
    w.set_local_selected(1);

    assert!(pane_named(&w, "This PC").is_valid());
    assert!(pane_named(&w, "No site selected").is_valid());
    let lists: Vec<_> = ElementHandle::find_by_accessible_label(&w, "Files in This PC").collect();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].accessible_role(), Some(AccessibleRole::List));
    assert_eq!(lists[0].accessible_item_count(), Some(3));

    let folder = row_starting(&w, "local-00000.txt");
    assert_eq!(
        folder.accessible_label().as_deref(),
        Some("local-00000.txt, folder, modified Today 10:02")
    );
    let file = row_starting(&w, "local-00001.txt");
    assert_eq!(
        file.accessible_label().as_deref(),
        Some("local-00001.txt, document, 1 B, modified Today 10:02")
    );
    assert_eq!(file.accessible_item_selected(), Some(true));
    assert_eq!(folder.accessible_item_selected(), Some(false));
    assert_eq!(file.accessible_item_index(), Some(1));
    assert_eq!(file.accessible_item_count(), Some(3));

    for label in ["Name, sorted ascending", "Sort by Size", "Sort by Modified"] {
        let found: Vec<_> = ElementHandle::find_by_accessible_label(&w, label).collect();
        assert_eq!(found.len(), 2, "{label}: one per pane");
        assert_eq!(found[0].accessible_role(), Some(AccessibleRole::Button));
    }
}

#[test]
fn assistive_technology_can_open_a_row_and_press_a_header() {
    let w = window_with_panes(3, 0);
    let opened = Rc::new(Cell::new(-1));
    let sorted = Rc::new(Cell::new(-1));
    w.on_open_local({
        let opened = opened.clone();
        move |i| opened.set(i)
    });
    w.on_sort_local({
        let sorted = sorted.clone();
        move |c| sorted.set(c)
    });
    row_starting(&w, "local-00002").invoke_accessible_default_action();
    assert_eq!((opened.get(), w.get_local_selected()), (2, 2));
    ElementHandle::find_by_accessible_label(&w, "Sort by Modified")
        .next()
        .unwrap()
        .invoke_accessible_default_action();
    assert_eq!(sorted.get(), 2);
}

#[test]
fn only_the_visible_rows_are_created() {
    let w = window_with_panes(50_000, 0);
    let created = list_items(&w).len();
    assert!(
        (10..150).contains(&created),
        "{created} rows exist for 50,000"
    );
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::End);
    let created = list_items(&w).len();
    assert!((10..150).contains(&created), "{created} rows after End");
    assert_eq!(w.get_local_selected(), 49_999);
}

#[test]
fn the_list_scrolls_to_keep_the_selection_in_view() {
    let w = window_with_panes(300, 0);
    let pane = pane_named(&w, "This PC");
    tab(&w, LOCAL_LIST);

    // One step at a time, past the bottom of the first screen and on.
    for i in 0..60 {
        tap(w.window(), Key::DownArrow);
        let row = row_starting(&w, &format!("local-{i:05}"));
        assert!(fully_visible(&row, &pane), "row {i} is cut off");
    }
    // A long jump down and back up, then a page at a time.
    tap(w.window(), Key::End);
    assert!(fully_visible(&row_starting(&w, "local-00299"), &pane));
    tap(w.window(), Key::Home);
    assert!(fully_visible(&row_starting(&w, "local-00000"), &pane));
    let mut last = w.get_local_selected();
    for _ in 0..5 {
        tap(w.window(), Key::PageDown);
        let now = w.get_local_selected();
        assert!(now > last + 3, "a page moved from {last} to {now}");
        let row = row_starting(&w, &format!("local-{now:05}"));
        assert!(fully_visible(&row, &pane), "row {now} is cut off");
        last = now;
    }
    for _ in 0..5 {
        tap(w.window(), Key::PageUp);
        let now = w.get_local_selected();
        assert!(now < last, "page up from {last} to {now}");
        let row = row_starting(&w, &format!("local-{now:05}"));
        assert!(fully_visible(&row, &pane), "row {now} is cut off");
        last = now;
    }
}

#[test]
fn a_selection_set_by_the_app_is_scrolled_into_view() {
    let w = window_with_panes(300, 0);
    let pane = pane_named(&w, "This PC");
    w.set_local_selected(150);
    // The pane reacts to the change on the next pass of the event loop.
    slint::platform::update_timers_and_animations();
    assert!(fully_visible(&row_starting(&w, "local-00150"), &pane));
    // Clearing it (as after opening a folder) goes back to the top.
    w.set_local_selected(-1);
    slint::platform::update_timers_and_animations();
    assert!(fully_visible(&row_starting(&w, "local-00000"), &pane));
}

#[test]
fn the_panes_sit_side_by_side_when_wide_and_stack_when_narrow() {
    let w = window_with_panes(3, 3);
    let (local, remote) = (
        pane_named(&w, "This PC"),
        pane_named(&w, "No site selected"),
    );

    // The default 1280 px window leaves room for two 420 px panes next to each other.
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    let (l, r) = (local.absolute_position(), remote.absolute_position());
    assert_eq!(l.y, r.y);
    assert!(r.x > l.x + 400.0);
    assert!(!w.get_panes_stacked());
    assert_eq!(w.get_pane_split(), r.x);
    assert_eq!(w.get_remote_pane_y(), r.y);

    // At the minimum width the sidebar leaves too little for that: one above the other.
    w.window().set_size(slint::LogicalSize::new(960.0, 700.0));
    let (l, r) = (local.absolute_position(), remote.absolute_position());
    assert_eq!(l.x, r.x);
    assert!(r.y > l.y + local.size().height);
    assert_eq!(local.size().width, remote.size().width);
    assert!(w.get_panes_stacked());
    assert_eq!(w.get_pane_split(), r.x);
    assert_eq!(w.get_remote_pane_y(), r.y);

    // Back to wide: the three values follow.
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    let r = remote.absolute_position();
    assert!(!w.get_panes_stacked());
    assert_eq!((w.get_pane_split(), w.get_remote_pane_y()), (r.x, r.y));
}

#[test]
fn the_pane_position_outputs_tell_which_pane_a_point_is_over() {
    // The rule Task 14 applies to a dropped file's position.
    let over_cloud = |w: &AppWindow, x: f32, y: f32| {
        if w.get_panes_stacked() {
            y >= w.get_remote_pane_y()
        } else {
            x >= w.get_pane_split()
        }
    };
    let w = window_with_panes(3, 3);
    let (local, remote) = (
        pane_named(&w, "This PC"),
        pane_named(&w, "No site selected"),
    );
    for (width, height) in [(1280.0, 800.0), (960.0, 700.0)] {
        w.window().set_size(slint::LogicalSize::new(width, height));
        let (lc, rc) = (center(&local), center(&remote));
        assert!(!over_cloud(&w, lc.x, lc.y), "{width}: centre of This PC");
        assert!(
            over_cloud(&w, rc.x, rc.y),
            "{width}: centre of the cloud pane"
        );
    }
}

fn crumb_names(names: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        names
            .iter()
            .map(|n| (*n).into())
            .collect::<Vec<SharedString>>(),
    ))
}

fn crumb_button(w: &AppWindow, name: &str) -> Option<ElementHandle> {
    ElementHandle::find_by_accessible_label(w, &format!("Go to {name}")).next()
}

/// Records the index every `local-crumb-clicked` reports.
fn record_local_crumbs(w: &AppWindow) -> Rc<std::cell::RefCell<Vec<i32>>> {
    let clicked = Rc::new(std::cell::RefCell::new(Vec::new()));
    w.on_local_crumb_clicked({
        let clicked = clicked.clone();
        move |i| clicked.borrow_mut().push(i)
    });
    clicked
}

#[test]
fn every_breadcrumb_but_the_last_is_a_button_that_reports_its_index() {
    let w = window_with_panes(3, 3);
    w.set_local_crumbs(crumb_names(&["D:", "sites", "exampleshop", "uploads"]));
    let clicked = record_local_crumbs(&w);

    for name in ["D:", "sites", "exampleshop"] {
        let button = crumb_button(&w, name).unwrap_or_else(|| panic!("no button for {name}"));
        assert_eq!(button.accessible_role(), Some(AccessibleRole::Button));
    }
    // The current folder is plain text.
    assert!(crumb_button(&w, "uploads").is_none());

    click(w.window(), center(&crumb_button(&w, "exampleshop").unwrap()), 1);
    click(w.window(), center(&crumb_button(&w, "D:").unwrap()), 1);
    click(w.window(), center(&crumb_button(&w, "sites").unwrap()), 1);
    assert_eq!(*clicked.borrow(), [2, 0, 1]);
    // Clicking does not select a row or sort.
    assert_eq!(w.get_local_selected(), -1);
}

#[test]
fn clicking_the_current_folder_does_nothing() {
    let w = window_with_panes(3, 3);
    w.set_local_crumbs(crumb_names(&["D:", "sites", "uploads"]));
    let clicked = record_local_crumbs(&w);
    let last = ElementHandle::find_by_accessible_label(&w, "uploads")
        .next()
        .expect("the current folder is shown");
    assert_ne!(last.accessible_role(), Some(AccessibleRole::Button));
    click(w.window(), center(&last), 2);
    assert!(clicked.borrow().is_empty());
}

#[test]
fn breadcrumbs_work_from_the_keyboard_and_the_last_is_not_a_tab_stop() {
    let w = window_with_panes(3, 3);
    w.set_local_crumbs(crumb_names(&["D:", "sites", "uploads"]));
    let clicked = record_local_crumbs(&w);
    let sorted = Rc::new(Cell::new(-1));
    w.on_sort_local({
        let sorted = sorted.clone();
        move |c| sorted.set(c)
    });

    // Tab order in the pane: list, then the crumbs that are buttons, then the column headers.
    tab(&w, LOCAL_LIST + 1);
    tap(w.window(), Key::Return);
    assert_eq!(*clicked.borrow(), [0]);
    // A pressed crumb hands the focus to the list (see
    // `a_pressed_breadcrumb_hands_the_focus_to_the_list`), so the second crumb is two stops on.
    tab(&w, 2);
    // Key repeat does not press it.
    w.window().dispatch_event(WindowEvent::KeyPressRepeated {
        text: Key::Return.into(),
    });
    assert_eq!(*clicked.borrow(), [0]);
    tap(w.window(), Key::Space);
    assert_eq!(*clicked.borrow(), [0, 1]);

    // After the list and the two crumbs comes the Name column header, not the current folder.
    tab(&w, 3);
    tap(w.window(), Key::Return);
    assert_eq!(*clicked.borrow(), [0, 1]);
    assert_eq!(sorted.get(), 0);
}

#[test]
fn assistive_technology_can_press_a_breadcrumb() {
    let w = window_with_panes(3, 3);
    w.set_local_crumbs(crumb_names(&["D:", "sites", "uploads"]));
    let clicked = record_local_crumbs(&w);
    crumb_button(&w, "sites")
        .unwrap()
        .invoke_accessible_default_action();
    assert_eq!(*clicked.borrow(), [1]);
}

#[test]
fn the_cloud_pane_reports_its_own_breadcrumbs() {
    let w = window_with_panes(3, 3);
    w.set_remote_crumbs(crumb_names(&["exampleshop", "wp-content", "uploads"]));
    let remote = Rc::new(std::cell::RefCell::new(Vec::new()));
    w.on_remote_crumb_clicked({
        let remote = remote.clone();
        move |i| remote.borrow_mut().push(i)
    });
    let local = record_local_crumbs(&w);
    click(
        w.window(),
        center(&crumb_button(&w, "wp-content").unwrap()),
        1,
    );
    click(
        w.window(),
        center(&crumb_button(&w, "exampleshop").unwrap()),
        1,
    );
    assert_eq!(*remote.borrow(), [1, 0]);
    assert!(local.borrow().is_empty());
}

#[test]
fn a_long_path_stays_inside_the_pane_header() {
    let w = window_with_panes(3, 3);
    let names: Vec<String> = (0..9)
        .map(|i| format!("a-rather-long-folder-name-{i}"))
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    w.set_local_crumbs(crumb_names(&names));
    let pane = pane_named(&w, "This PC");
    let (p, ps) = (pane.absolute_position(), pane.size());
    for name in &names[..names.len() - 1] {
        let button = crumb_button(&w, name).unwrap();
        let (b, bs) = (button.absolute_position(), button.size());
        assert!(
            b.x >= p.x && b.x + bs.width <= p.x + ps.width,
            "{name} spans {}..{} in a pane {}..{}",
            b.x,
            b.x + bs.width,
            p.x,
            p.x + ps.width
        );
    }
}

#[test]
fn the_strong_focus_ring_follows_the_keyboard_not_the_mouse() {
    testing::init_no_event_loop();
    let bed = PaneBed::new().unwrap();
    let rows: Vec<FileRow> = row_data("bed", 4)
        .iter()
        .map(|r| FileRow {
            name: r.name.as_str().into(),
            size: r.size.as_str().into(),
            modified: r.modified.as_str().into(),
            folder: r.folder,
            ..Default::default()
        })
        .collect();
    bed.set_rows(ModelRc::new(VecModel::from(rows)));
    bed.show().unwrap();
    assert!(!bed.get_focused() && !bed.get_ring());

    // Tab in: the keyboard owns the focus, so the strong ring shows.
    tap(bed.window(), Key::Tab);
    assert!(bed.get_focused() && bed.get_ring());

    // A click on a row keeps the focus but hides the strong ring (the soft glow remains).
    // The pane sits at the window's origin: 37 px header, 26 px column headers, 28 px rows.
    click(
        bed.window(),
        LogicalPosition::new(100.0, 63.0 + 28.0 + 14.0),
        1,
    );
    assert!(bed.get_focused() && !bed.get_ring());

    // Using the keyboard again brings it back.
    tap(bed.window(), Key::DownArrow);
    assert!(bed.get_focused() && bed.get_ring());

    // Leaving the pane clears both.
    tap(bed.window(), Key::Backtab);
    assert!(!bed.get_focused() && !bed.get_ring());
}

/// Note for Task 15: the app answers a pressed breadcrumb with the new path, which rebuilds the
/// crumbs and would take the keyboard focus away with the pressed one. The pane's list takes the
/// focus when a crumb is pressed, so it survives the rebuild.
#[test]
fn a_pressed_breadcrumb_hands_the_focus_to_the_list() {
    testing::init_no_event_loop();
    let bed = PaneBed::new().unwrap();
    let path = ["D:", "sites", "uploads"];
    bed.set_crumbs(crumb_names(&path));
    let pressed = Rc::new(std::cell::RefCell::new(Vec::new()));
    bed.on_crumb_clicked({
        let pressed = pressed.clone();
        move |i| pressed.borrow_mut().push(i)
    });
    bed.show().unwrap();
    assert!(!bed.get_focused());

    // From the keyboard: Tab to the list, then past "D:" to "sites", and press it. The strong
    // ring shows.
    for _ in 0..3 {
        tap(bed.window(), Key::Tab);
    }
    tap(bed.window(), Key::Return);
    assert_eq!(*pressed.borrow(), [1]);
    assert!(bed.get_focused() && bed.get_ring());
    bed.set_crumbs(crumb_names(&path[..2]));
    assert!(bed.get_focused() && bed.get_ring());
}

/// The same with the mouse: after the crumbs are rebuilt, the arrow keys work in the list.
#[test]
fn a_clicked_breadcrumb_hands_the_focus_to_the_list() {
    let w = window_with_panes(3, 3);
    let path = ["D:", "sites", "uploads"];
    w.set_local_crumbs(crumb_names(&path));
    let clicked = record_local_crumbs(&w);
    click(w.window(), center(&crumb_button(&w, "sites").unwrap()), 1);
    assert_eq!(*clicked.borrow(), [1]);
    w.set_local_crumbs(crumb_names(&path[..2]));
    assert_eq!(w.get_local_selected(), -1);
    tap(w.window(), Key::DownArrow);
    assert_eq!(w.get_local_selected(), 0);
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
