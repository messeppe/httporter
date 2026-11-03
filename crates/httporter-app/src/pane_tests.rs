//! The two file panes, driven headlessly with the testing backend: keyboard selection and
//! scrolling, clicks, column headers and sorting, breadcrumbs, focus rings, how the panes lay
//! out, "Loading…", and the status bar following the pane last used. Keys and clicks come from
//! the helpers of `keyboard_tests.rs`.

use crate::keyboard_tests::{center, click, site, tab, tap};
use crate::viewmodel::{FileKindData, RowData, to_slint_rows};
use crate::{AppWindow, ProviderTile};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, SharedString, VecModel};
use std::cell::Cell;
use std::rc::Rc;

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
            bytes: 1,
        })
        .collect()
}

/// A window with one saved site (not chosen, so the cloud pane reads "No site selected"): with no
/// site at all the window shows the Welcome screen instead of the panes.
pub(crate) fn window_with_panes(local: usize, remote: usize) -> AppWindow {
    testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    w.set_sites(ModelRc::new(VecModel::from(vec![site(
        "example-shop.com",
        ProviderTile::Bunny,
        "B",
    )])));
    w.set_local_rows(to_slint_rows(&row_data("local", local)));
    w.set_remote_rows(to_slint_rows(&row_data("remote", remote)));
    w.show().unwrap();
    w
}

// Tab order of `window_with_panes`: sidebar toggle, the site's row, Add Site (Back, Forward,
// Publish, Settings and the four file actions are turned off), then each pane: its list, its
// crumbs, its Refresh button (the cloud pane's is turned off without a chosen site), then its
// Name, Size and Modified headers.
pub(crate) const LOCAL_LIST: usize = 4;
pub(crate) const LOCAL_NAME_HEADER: usize = 6;
pub(crate) const REMOTE_LIST: usize = 9;

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
    // The highlight comes from the row's own flag (the app sets it with the selection).
    let rows = w.get_local_rows();
    let mut picked = rows.row_data(1).unwrap();
    picked.selected = true;
    rows.set_row_data(1, picked);

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

/// The rectangle a pane reports in `local-bounds` / `remote-bounds` is the pane on screen.
fn assert_bounds_are(bounds: crate::PaneBounds, pane: &ElementHandle, label: &str) {
    let (p, s) = (pane.absolute_position(), pane.size());
    assert_eq!(
        (bounds.x, bounds.y, bounds.width, bounds.height),
        (p.x, p.y, s.width, s.height),
        "{label}"
    );
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
    assert_bounds_are(w.get_local_bounds(), &local, "wide, This PC");
    assert_bounds_are(w.get_remote_bounds(), &remote, "wide, cloud");

    // At the minimum width the sidebar leaves too little for that: one above the other.
    w.window().set_size(slint::LogicalSize::new(960.0, 700.0));
    let (l, r) = (local.absolute_position(), remote.absolute_position());
    assert_eq!(l.x, r.x);
    assert!(r.y > l.y + local.size().height);
    assert_eq!(local.size().width, remote.size().width);
    assert_bounds_are(w.get_local_bounds(), &local, "narrow, This PC");
    assert_bounds_are(w.get_remote_bounds(), &remote, "narrow, cloud");

    // Back to wide: the bounds follow.
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    assert_bounds_are(w.get_remote_bounds(), &remote, "wide again, cloud");
    assert!(w.get_remote_bounds().x > w.get_local_bounds().x);
}

/// What the hook reads each pane's list by: where the rows start, their height, the scroll and
/// the row count.
#[test]
fn the_bounds_say_where_the_rows_are_and_follow_the_scroll() {
    let w = window_with_panes(300, 2);
    let (local, remote) = (w.get_local_bounds(), w.get_remote_bounds());
    assert_eq!((local.rows, remote.rows), (300, 2));
    assert_eq!(local.row_height, 28.0);
    // Under the 37 px header and the 26 px column titles.
    assert_eq!(local.list_top, local.y + 63.0);
    assert_eq!(remote.list_top, remote.y + 63.0);
    assert_eq!(local.scroll_y, 0.0);

    // The first row is where the bounds say.
    let first = row_starting(&w, "local-00000");
    assert_eq!(first.absolute_position().y, local.list_top);

    // Scrolling the list moves the scroll offset, and `list_top` stays.
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::End);
    slint::platform::update_timers_and_animations();
    let scrolled = w.get_local_bounds();
    assert!(scrolled.scroll_y > 28.0 * 200.0, "{}", scrolled.scroll_y);
    assert_eq!(scrolled.list_top, local.list_top);
    let last = row_starting(&w, "local-00299");
    let y = last.absolute_position().y;
    assert_eq!(
        y,
        scrolled.list_top + 299.0 * 28.0 - scrolled.scroll_y,
        "the last row is where the scroll puts it"
    );
}

#[test]
fn the_pane_bounds_tell_which_pane_and_row_a_point_is_over() {
    // The rule Task 22 applies to a dropped file's position.
    use crate::drop_hook::{PaneLayout, PaneSide, hit};
    let w = window_with_panes(3, 3);
    let (local, remote) = (
        pane_named(&w, "This PC"),
        pane_named(&w, "No site selected"),
    );
    for (width, height) in [(1280.0, 800.0), (960.0, 700.0)] {
        w.window().set_size(slint::LogicalSize::new(width, height));
        let layout = PaneLayout {
            local: crate::geometry(&w.get_local_bounds()),
            remote: crate::geometry(&w.get_remote_bounds()),
        };
        let point = |e: &ElementHandle| {
            let c = center(e);
            (c.x, c.y)
        };
        assert_eq!(
            hit(point(&local), &layout).map(|t| t.0),
            Some(PaneSide::Local),
            "{width}: centre of This PC"
        );
        assert_eq!(
            hit(point(&remote), &layout).map(|t| t.0),
            Some(PaneSide::Remote),
            "{width}: centre of the cloud pane"
        );
        // A row of the cloud pane (the list is short: the middle of the pane is below its rows).
        assert_eq!(
            hit(point(&row_starting(&w, "remote-00001")), &layout),
            Some((PaneSide::Remote, Some(1))),
            "{width}"
        );
        assert_eq!(
            hit(point(&row_starting(&w, "local-00002")), &layout),
            Some((PaneSide::Local, Some(2))),
            "{width}"
        );
        // The toolbar above the panes is neither.
        assert_eq!(hit((point(&local).0, 10.0), &layout), None);
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

    // After the list and the two crumbs come the Refresh button and the Name column header, not
    // the current folder.
    tab(&w, 4);
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

fn loading_marks(w: &AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_accessible_label(w, "Loading…")
        .filter(|e| e.accessible_role() == Some(AccessibleRole::Text))
        .collect()
}

fn inside(e: &ElementHandle, pane: &ElementHandle) -> bool {
    let (p, s) = (e.absolute_position(), pane.absolute_position());
    let size = pane.size();
    p.x >= s.x && p.y >= s.y && p.x <= s.x + size.width && p.y <= s.y + size.height
}

/// Lets `ms` milliseconds of the testing backend's clock go by (timers fire as they come due).
fn pass(ms: u64) {
    testing::mock_elapsed_time(std::time::Duration::from_millis(ms));
}

/// Starts the timers that depend on a property set since (a Slint timer begins at the next pass
/// of the event loop, not when its `running` is set).
fn settle() {
    slint::platform::update_timers_and_animations();
}

/// Minor 2: a pane whose listing is on its way says "Loading…", on screen and to assistive
/// technology, until the answer is in. Polish 6: it says so only once the listing has taken
/// 200 ms, so a quick one flashes nothing.
#[test]
fn a_pane_says_it_is_loading() {
    let w = window_with_panes(3, 3);
    assert!(loading_marks(&w).is_empty());
    let cloud = pane_named(&w, "No site selected");
    let this_pc = pane_named(&w, "This PC");

    w.set_remote_loading(true);
    settle();
    pass(199);
    assert!(loading_marks(&w).is_empty(), "not yet");
    pass(1);
    let marks = loading_marks(&w);
    assert_eq!(marks.len(), 1);
    assert!(inside(&marks[0], &cloud) && !inside(&marks[0], &this_pc));
    assert_eq!(
        marks[0].accessible_live_region(),
        Some(i_slint_backend_testing::AccessibleLiveness::Polite)
    );
    // The rows stay while it loads.
    assert_eq!(
        row_starting(&w, "remote-00002").accessible_role(),
        Some(AccessibleRole::ListItem)
    );

    // The other pane has its own clock.
    w.set_local_loading(true);
    settle();
    pass(199);
    assert_eq!(loading_marks(&w).len(), 1);
    pass(1);
    assert_eq!(loading_marks(&w).len(), 2);
    w.set_remote_loading(false);
    settle();
    let marks = loading_marks(&w);
    assert_eq!(marks.len(), 1);
    assert!(inside(&marks[0], &this_pc));
    w.set_local_loading(false);
    settle();
    assert!(loading_marks(&w).is_empty());
}

/// Polish 6: a listing that is in before 200 ms never shows the mark (so a screen reader says
/// nothing); one that takes longer shows one mark for as long as it takes, and a listing after it
/// starts its 200 ms afresh.
#[test]
fn polish_6_loading_flashes_nothing_for_a_quick_listing() {
    let w = window_with_panes(3, 3);
    // 150 ms: in before the mark would show.
    w.set_remote_loading(true);
    settle();
    pass(150);
    w.set_remote_loading(false);
    settle();
    pass(500);
    assert!(
        loading_marks(&w).is_empty(),
        "the timer was stopped with the listing"
    );

    // A slow one: the mark is there from 200 ms, once, however long it takes.
    w.set_remote_loading(true);
    settle();
    pass(200);
    assert_eq!(loading_marks(&w).len(), 1);
    pass(5_000);
    assert_eq!(
        loading_marks(&w).len(),
        1,
        "one live region, not one per tick"
    );
    w.set_remote_loading(false);
    settle();
    assert!(loading_marks(&w).is_empty());

    // The next listing starts from zero: nothing at 199 ms.
    w.set_remote_loading(true);
    settle();
    pass(199);
    assert!(loading_marks(&w).is_empty());
    pass(1);
    assert_eq!(loading_marks(&w).len(), 1);
}

fn release_chips(w: &AppWindow) -> usize {
    ElementHandle::find_by_accessible_label(w, "Release to upload").count()
}

/// Minor 10: with no site there is nothing to upload to, so files dragged over the cloud pane
/// do not light it up.
#[test]
fn with_no_site_dragged_files_do_not_light_up_the_cloud_pane() {
    let w = window_with_panes(3, 3);
    w.set_drop_hover_remote(true);
    assert_eq!(release_chips(&w), 0);
    w.set_drop_hover_remote(false);

    w.set_sites(ModelRc::new(VecModel::from(vec![site(
        "example-shop.com",
        ProviderTile::Bunny,
        "B",
    )])));
    w.set_selected_site(0);
    w.set_drop_hover_remote(true);
    assert_eq!(release_chips(&w), 1);
    w.set_drop_hover_remote(false);
    assert_eq!(release_chips(&w), 0);
}

/// Minor 11: the status bar counts the items of the pane last clicked or focused.
#[test]
fn the_status_bar_follows_the_pane_last_used() {
    let w = window_with_panes(3, 3);
    w.set_local_status("3 local items".into());
    w.set_remote_status("3 cloud items".into());
    assert_eq!(w.get_status_left(), "3 cloud items");

    click(w.window(), center(&row_starting(&w, "local-00001")), 1);
    assert_eq!(w.get_status_left(), "3 local items");
    tab(&w, REMOTE_LIST - LOCAL_LIST);
    assert!(!w.get_local_pane_active());
    assert_eq!(w.get_status_left(), "3 cloud items");

    // A message from a drop stays until the user turns to the other pane.
    w.set_status_message("Drop files on the example-shop.com pane to upload them.".into());
    assert_eq!(
        w.get_status_left(),
        "Drop files on the example-shop.com pane to upload them."
    );
    click(w.window(), center(&row_starting(&w, "local-00001")), 1);
    slint::platform::update_timers_and_animations();
    assert_eq!(w.get_status_left(), "3 local items");
}
