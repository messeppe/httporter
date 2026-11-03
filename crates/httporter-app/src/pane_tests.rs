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
    init_headless();
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

pub(crate) fn crumb_names(names: &[&str]) -> ModelRc<SharedString> {
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

// ---- Task 17: selecting, the rubber band and the context menus ----

use crate::keyboard_tests::{chord, description};
use slint::platform::PointerEventButton;
use std::cell::RefCell;

/// Marks rows `rows` of a pane's model as selected, as the app does with the rows it selects.
fn mark_selected(model: ModelRc<crate::FileRow>, rows: &[usize]) {
    for &i in rows {
        let mut row = model.row_data(i).unwrap();
        row.selected = true;
        model.set_row_data(i, row);
    }
}

fn press_at(w: &AppWindow, at: LogicalPosition, button: PointerEventButton) {
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: at });
    w.window().dispatch_event(WindowEvent::PointerPressed {
        position: at,
        button,
    });
}

fn move_to(w: &AppWindow, at: LogicalPosition) {
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position: at });
}

fn release_at(w: &AppWindow, at: LogicalPosition, button: PointerEventButton) {
    w.window().dispatch_event(WindowEvent::PointerReleased {
        position: at,
        button,
    });
}

fn right_click(w: &AppWindow, at: LogicalPosition) {
    press_at(w, at, PointerEventButton::Right);
    release_at(w, at, PointerEventButton::Right);
}

/// Task 17: Ctrl+A in a pane reports "select all" for that pane, and for no other combination.
#[test]
fn ctrl_a_selects_all_rows() {
    let w = window_with_panes(4, 3);
    let all = Rc::new(RefCell::new(Vec::new()));
    w.on_select_all({
        let all = all.clone();
        move |pane| all.borrow_mut().push(pane)
    });
    tab(&w, LOCAL_LIST);
    chord(w.window(), &[Key::Control], "a");
    assert_eq!(*all.borrow(), [0]);
    chord(w.window(), &[Key::Control], "A");
    assert_eq!(*all.borrow(), [0, 0]);

    // Only Ctrl+A: not A, Ctrl+Shift+A, Ctrl+Alt+A.
    tap(w.window(), "a");
    chord(w.window(), &[Key::Control, Key::Shift], "a");
    chord(w.window(), &[Key::Control, Key::Alt], "a");
    assert_eq!(all.borrow().len(), 2);

    // The cloud pane's own list reports itself.
    tab(&w, REMOTE_LIST - LOCAL_LIST);
    chord(w.window(), &[Key::Control], "a");
    assert_eq!(*all.borrow(), [0, 0, 1]);
    // And a pane with no rows still says so: the app has nothing to select.
    let empty = window_with_panes(0, 0);
    let all = Rc::new(Cell::new(0));
    empty.on_select_all({
        let all = all.clone();
        move |_| all.set(all.get() + 1)
    });
    tab(&empty, LOCAL_LIST);
    chord(empty.window(), &[Key::Control], "a");
    assert_eq!(all.get(), 1);
}

/// The keyboard reports the row it lands on, and Shift says it extends the selection.
#[test]
fn the_keys_report_the_row_they_land_on_with_shift() {
    let w = window_with_panes(5, 0);
    let log = Rc::new(RefCell::new(Vec::new()));
    w.on_row_pointer({
        let log = log.clone();
        move |pane, row, ctrl, shift| log.borrow_mut().push((pane, row, ctrl, shift))
    });
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::DownArrow);
    tap(w.window(), Key::DownArrow);
    chord(w.window(), &[Key::Shift], Key::DownArrow);
    chord(w.window(), &[Key::Shift], Key::End);
    tap(w.window(), Key::Home);
    chord(w.window(), &[Key::Shift], Key::PageDown);
    assert_eq!(
        *log.borrow(),
        [
            (0, 0, false, false),
            (0, 1, false, false),
            (0, 2, false, true),
            (0, 4, false, true),
            (0, 0, false, false),
            (0, 4, false, true),
        ]
    );
}

/// A click reports the modifiers that were held, and a click on the other pane says which pane.
#[test]
fn a_click_reports_the_row_and_the_modifiers() {
    let w = window_with_panes(6, 6);
    let log = Rc::new(RefCell::new(Vec::new()));
    w.on_row_pointer({
        let log = log.clone();
        move |pane, row, ctrl, shift| log.borrow_mut().push((pane, row, ctrl, shift))
    });
    let at = |name: &str| center(&row_starting(&w, name));
    click(w.window(), at("local-00001"), 1);
    chord_click(&w, &[Key::Control], at("local-00003"));
    chord_click(&w, &[Key::Shift], at("local-00005"));
    chord_click(&w, &[Key::Control, Key::Shift], at("remote-00002"));
    click(w.window(), at("remote-00004"), 1);
    assert_eq!(
        *log.borrow(),
        [
            (0, 1, false, false),
            (0, 3, true, false),
            (0, 5, false, true),
            (1, 2, true, true),
            (1, 4, false, false),
        ]
    );
}

/// Clicks with modifiers held.
fn chord_click(w: &AppWindow, modifiers: &[Key], at: LogicalPosition) {
    for m in modifiers {
        w.window()
            .dispatch_event(WindowEvent::KeyPressed { text: (*m).into() });
    }
    click(w.window(), at, 1);
    for m in modifiers.iter().rev() {
        w.window()
            .dispatch_event(WindowEvent::KeyReleased { text: (*m).into() });
    }
}

/// Dragging in the empty space below the rows reports the rows the band crosses as it moves; with
/// Ctrl held it says the band adds to the selection; it only starts below the last row.
#[test]
fn the_rubber_band_reports_the_rows_it_crosses() {
    let w = window_with_panes(3, 3);
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    let bands = Rc::new(RefCell::new(Vec::new()));
    w.on_band_select({
        let bands = bands.clone();
        move |pane, from, to, additive| bands.borrow_mut().push((pane, from, to, additive))
    });
    let log = Rc::new(RefCell::new(Vec::new()));
    w.on_row_pointer({
        let log = log.clone();
        move |pane, row, _, _| log.borrow_mut().push((pane, row))
    });
    let list = w.get_local_bounds();
    let x = list.x + list.width / 2.0;
    let at = |below_top: f32| LogicalPosition::new(x, list.list_top + below_top);

    // Three rows end at 84 px; a press at 150 px is in the empty space. It crosses no row yet.
    press_at(&w, at(150.0), PointerEventButton::Left);
    assert_eq!(*bands.borrow(), [(0, -1, -1, false)]);
    // Up to 30 px: it now covers rows 1 and 2 (the rows end at 3).
    move_to(&w, at(30.0));
    assert_eq!(bands.borrow().last(), Some(&(0, 1, 2, false)));
    move_to(&w, at(5.0));
    assert_eq!(bands.borrow().last(), Some(&(0, 0, 2, false)));
    // Back down to the empty space: it covers none again.
    move_to(&w, at(120.0));
    assert_eq!(bands.borrow().last(), Some(&(0, -1, -1, false)));
    release_at(&w, at(120.0), PointerEventButton::Left);
    let done = bands.borrow().len();
    move_to(&w, at(10.0));
    assert_eq!(bands.borrow().len(), done, "the band ends with the button");
    assert!(
        log.borrow().is_empty(),
        "the band selects no row by a click"
    );

    // With Ctrl held it adds to the selection.
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    press_at(&w, at(150.0), PointerEventButton::Left);
    move_to(&w, at(40.0));
    release_at(&w, at(40.0), PointerEventButton::Left);
    w.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    assert_eq!(bands.borrow().last(), Some(&(0, 1, 2, true)));

    // A full list has no empty space: the press is on a row, and there is no band.
    let full = window_with_panes(60, 3);
    full.window()
        .set_size(slint::LogicalSize::new(1280.0, 800.0));
    let n = Rc::new(Cell::new(0));
    full.on_band_select({
        let n = n.clone();
        move |_, _, _, _| n.set(n.get() + 1)
    });
    let bounds = full.get_local_bounds();
    let last = LogicalPosition::new(
        bounds.x + bounds.width / 2.0,
        bounds.list_top + bounds.height - 63.0 - 5.0,
    );
    press_at(&full, last, PointerEventButton::Left);
    move_to(&full, LogicalPosition::new(last.x, last.y - 60.0));
    release_at(&full, last, PointerEventButton::Left);
    assert_eq!(n.get(), 0);
}

/// The titles of the entries of the context menu that is open, in order, with whether each can be
/// chosen (a turned-off entry is drawn at half strength). Separators have no title.
fn open_menu(w: &AppWindow) -> Vec<(String, bool)> {
    let frame = ElementQuery::from_root(w)
        .match_type_name("MenuFrame")
        .find_first()
        .expect("a context menu is open");
    frame
        .query_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .find_all()
        .into_iter()
        .filter_map(|e| {
            let title = e.accessible_label()?.to_string();
            (!title.is_empty()).then(|| (title, e.computed_opacity() > 0.9))
        })
        .collect()
}

fn menu_is_open(w: &AppWindow) -> bool {
    ElementQuery::from_root(w)
        .match_type_name("MenuFrame")
        .find_first()
        .is_some()
}

/// The entry of the open menu with this title.
fn menu_entry(w: &AppWindow, title: &str) -> ElementHandle {
    let frame = ElementQuery::from_root(w)
        .match_type_name("MenuFrame")
        .find_first()
        .expect("a context menu is open");
    frame
        .query_descendants()
        .match_accessible_role(AccessibleRole::Text)
        .match_predicate({
            let title = title.to_owned();
            move |e| e.accessible_label().is_some_and(|l| l == title)
        })
        .find_first()
        .unwrap_or_else(|| panic!("no menu entry {title:?}"))
}

/// Records what the menus and the shortcuts call.
fn record_commands(w: &AppWindow) -> Rc<RefCell<Vec<String>>> {
    let log = Rc::new(RefCell::new(Vec::new()));
    macro_rules! log {
        ($on:ident, $name:expr) => {{
            let (log, weak) = (log.clone(), w.as_weak());
            w.$on(move || {
                let local = weak.upgrade().unwrap().get_local_pane_active();
                log.borrow_mut().push(format!(
                    "{} ({})",
                    $name,
                    if local { "pc" } else { "cloud" }
                ));
            });
        }};
    }
    log!(on_upload, "upload");
    log!(on_download, "download");
    log!(on_download_to, "download-to");
    log!(on_new_folder, "new-folder");
    log!(on_delete, "delete");
    log!(on_refresh, "refresh");
    w.on_show_in_explorer({
        let log = log.clone();
        move |row| log.borrow_mut().push(format!("explorer {row}"))
    });
    w.on_row_pointer({
        let log = log.clone();
        move |pane, row, _, _| log.borrow_mut().push(format!("row {pane}:{row}"))
    });
    log
}

/// Task 17: a right click on a cloud row opens Download, Download to…, New Folder…, Delete… and
/// Refresh; on a This PC row Upload, New Folder…, Show in Explorer and Refresh. Each is on or off
/// as the toolbar says, and a chosen one reaches the app.
#[test]
fn context_menu_items_present_with_tr() {
    let w = window_with_panes(4, 4);
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    let log = record_commands(&w);
    let entries = |items: &[(&str, bool)]| -> Vec<(String, bool)> {
        items.iter().map(|(t, on)| ((*t).to_owned(), *on)).collect()
    };

    // The cloud menu, with Delete off (nothing is selected that could go).
    w.set_can_download(true);
    w.set_can_new_folder(true);
    right_click(&w, center(&row_starting(&w, "remote-00001")));
    assert_eq!(
        open_menu(&w),
        entries(&[
            ("Download", true),
            ("Download to…", true),
            ("New Folder…", true),
            ("Delete…", false),
            ("Refresh", true),
        ])
    );
    // The row under the click was picked first, as an ordinary click on it would.
    assert_eq!(*log.borrow(), ["row 1:1"]);
    // A turned-off entry cannot be chosen; a turned-on one can, and closes the menu.
    menu_entry(&w, "Delete…").mock_single_click(PointerEventButton::Left);
    assert_eq!(log.borrow().len(), 1, "a turned-off entry cannot be chosen");
    // (Whether Slint dismisses the menu after a click on a turned-off entry is up to it.)
    mark_selected(w.get_remote_rows(), &[1]); // the app selected the row that was picked
    reopen_menu(&w, center(&row_starting(&w, "remote-00001")));
    menu_entry(&w, "Download to…").mock_single_click(PointerEventButton::Left);
    assert!(!menu_is_open(&w));
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("download-to (cloud)")
    );

    // With a selection that can go, Delete… is on; and it reaches the app.
    w.set_can_delete(true);
    mark_selected(w.get_remote_rows(), &[2]);
    right_click(&w, center(&row_starting(&w, "remote-00002")));
    assert_eq!(
        log.borrow().len(),
        1 + 1,
        "no new pick: the row was selected already"
    );
    assert!(open_menu(&w).contains(&("Delete…".to_owned(), true)));
    menu_entry(&w, "Delete…").mock_single_click(PointerEventButton::Left);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("delete (cloud)")
    );
    right_click(&w, center(&row_starting(&w, "remote-00002")));
    menu_entry(&w, "Download").mock_single_click(PointerEventButton::Left);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("download (cloud)")
    );
    right_click(&w, center(&row_starting(&w, "remote-00002")));
    menu_entry(&w, "New Folder…").mock_single_click(PointerEventButton::Left);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("new-folder (cloud)")
    );
    right_click(&w, center(&row_starting(&w, "remote-00002")));
    menu_entry(&w, "Refresh").mock_single_click(PointerEventButton::Left);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("refresh (cloud)")
    );

    // The This PC menu.
    log.borrow_mut().clear();
    w.set_can_upload(true);
    w.set_can_new_folder(false);
    right_click(&w, center(&row_starting(&w, "local-00002")));
    assert_eq!(
        open_menu(&w),
        entries(&[
            ("Upload", true),
            ("New Folder…", false),
            ("Show in Explorer", true),
            ("Refresh", true),
        ])
    );
    assert_eq!(*log.borrow(), ["row 0:2"]);
    menu_entry(&w, "New Folder…").mock_single_click(PointerEventButton::Left);
    mark_selected(w.get_local_rows(), &[2]);
    reopen_menu(&w, center(&row_starting(&w, "local-00002")));
    assert_eq!(log.borrow().len(), 1, "turned off");
    menu_entry(&w, "Show in Explorer").mock_single_click(PointerEventButton::Left);
    assert_eq!(log.borrow().last().map(String::as_str), Some("explorer 2"));
    right_click(&w, center(&row_starting(&w, "local-00003")));
    menu_entry(&w, "Upload").mock_single_click(PointerEventButton::Left);
    assert_eq!(log.borrow().last().map(String::as_str), Some("upload (pc)"));
    right_click(&w, center(&row_starting(&w, "local-00003")));
    menu_entry(&w, "Refresh").mock_single_click(PointerEventButton::Left);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("refresh (pc)"),
        "the pane's own Refresh lists the pane it was chosen in"
    );
}

/// The Menu key and Shift+F10 open the menu of the row the keyboard is on.
#[test]
fn the_menu_key_and_shift_f10_open_the_context_menu() {
    let w = window_with_panes(4, 4);
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    let log = record_commands(&w);
    tab(&w, LOCAL_LIST);
    tap(w.window(), Key::DownArrow);
    tap(w.window(), Key::DownArrow);
    assert!(!menu_is_open(&w));
    tap(w.window(), Key::Menu);
    assert_eq!(
        open_menu(&w)
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["Upload", "New Folder…", "Show in Explorer", "Refresh"]
    );
    menu_entry(&w, "Show in Explorer").mock_single_click(PointerEventButton::Left);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("explorer 1"),
        "the row the keyboard is on"
    );

    // Shift+F10 in the cloud pane's list.
    click(w.window(), row_center_of(&w, "remote-00001"), 1);
    tap(w.window(), Key::DownArrow);
    chord(w.window(), &[Key::Shift], Key::F10);
    assert_eq!(
        open_menu(&w)
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        [
            "Download",
            "Download to…",
            "New Folder…",
            "Delete…",
            "Refresh"
        ]
    );
    // A click elsewhere closes it without choosing anything.
    let before = log.borrow().len();
    click(w.window(), center(&pane_named(&w, "This PC")), 1);
    assert!(!menu_is_open(&w));
    assert!(
        log.borrow()
            .iter()
            .skip(before)
            .all(|l| l.starts_with("row"))
    );
}

// ---- Task 22: dragging between the panes, the chips and paste ----

/// A window wide enough for the panes to sit side by side, with rows 0 and 5 of each pane
/// folders, a site chosen and the toolbar's rules for uploading and downloading on.
fn window_for_drags() -> AppWindow {
    let w = window_with_panes(6, 6);
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    w.set_selected_site(0);
    w.set_can_upload(true);
    w.set_can_download(true);
    w
}

struct Drops {
    to_remote: Rc<RefCell<Vec<i32>>>,
    to_local: Rc<RefCell<Vec<i32>>>,
    picks: Rc<RefCell<Vec<(i32, i32, bool, bool)>>>,
}

fn watch_drops(w: &AppWindow) -> Drops {
    let drops = Drops {
        to_remote: Rc::default(),
        to_local: Rc::default(),
        picks: Rc::default(),
    };
    w.on_drag_to_remote({
        let log = drops.to_remote.clone();
        move |row| log.borrow_mut().push(row)
    });
    w.on_drag_to_local({
        let log = drops.to_local.clone();
        move |row| log.borrow_mut().push(row)
    });
    w.on_row_pointer({
        let log = drops.picks.clone();
        move |pane, row, ctrl, shift| log.borrow_mut().push((pane, row, ctrl, shift))
    });
    drops
}

/// Presses at `from`, moves 10 px right and then to `to`, and lets go there.
fn drag(w: &AppWindow, from: LogicalPosition, to: LogicalPosition) {
    press_at(w, from, PointerEventButton::Left);
    move_to(w, LogicalPosition::new(from.x + 10.0, from.y));
    move_to(w, to);
    release_at(w, to, PointerEventButton::Left);
}

fn row_center(w: &AppWindow, name: &str) -> LogicalPosition {
    center(&row_starting(w, name))
}

fn chips(w: &AppWindow, text: &str) -> usize {
    ElementHandle::find_by_accessible_label(w, text).count()
}

/// Task 22: a drag from This PC dropped on a folder row of the cloud pane names that row; on a
/// file, the empty space or the header of the pane, the folder it shows (-1); on the pane the drag
/// began in, or outside both panes, it does nothing.
#[test]
fn in_app_drag_fires_with_target_row() {
    let w = window_for_drags();
    let drops = watch_drops(&w);
    let remote = pane_named(&w, "example-shop.com");

    drag(
        &w,
        row_center(&w, "local-00001"),
        row_center(&w, "remote-00005"),
    );
    assert_eq!(*drops.to_remote.borrow(), [5], "a folder row");
    drag(
        &w,
        row_center(&w, "local-00001"),
        row_center(&w, "remote-00000"),
    );
    assert_eq!(
        *drops.to_remote.borrow(),
        [5, 0],
        "the first row is a folder too"
    );
    drag(
        &w,
        row_center(&w, "local-00001"),
        row_center(&w, "remote-00002"),
    );
    assert_eq!(*drops.to_remote.borrow(), [5, 0, -1], "a file row");
    // Below the rows, and on the pane's header.
    let (p, s) = (remote.absolute_position(), remote.size());
    drag(
        &w,
        row_center(&w, "local-00001"),
        LogicalPosition::new(p.x + s.width / 2.0, p.y + s.height - 40.0),
    );
    drag(
        &w,
        row_center(&w, "local-00001"),
        LogicalPosition::new(p.x + 60.0, p.y + 18.0),
    );
    assert_eq!(*drops.to_remote.borrow(), [5, 0, -1, -1, -1]);

    // Nothing for: the pane it began in, the toolbar, the status bar.
    let before = drops.to_remote.borrow().len();
    drag(
        &w,
        row_center(&w, "local-00001"),
        row_center(&w, "local-00003"),
    );
    drag(
        &w,
        row_center(&w, "local-00001"),
        LogicalPosition::new(p.x + 100.0, 10.0),
    );
    drag(
        &w,
        row_center(&w, "local-00001"),
        LogicalPosition::new(p.x + 100.0, 795.0),
    );
    assert_eq!(drops.to_remote.borrow().len(), before);
    assert!(drops.to_local.borrow().is_empty());

    // The same from the cloud pane to This PC, with `can-download`.
    drag(
        &w,
        row_center(&w, "remote-00002"),
        row_center(&w, "local-00005"),
    );
    drag(
        &w,
        row_center(&w, "remote-00002"),
        row_center(&w, "local-00004"),
    );
    drag(
        &w,
        row_center(&w, "remote-00002"),
        row_center(&w, "remote-00001"),
    );
    assert_eq!(*drops.to_local.borrow(), [5, -1]);
    assert_eq!(drops.to_remote.borrow().len(), before);
}

/// The same when the window is narrow and the panes are one above the other.
#[test]
fn in_app_drag_works_between_stacked_panes() {
    let w = window_with_panes(6, 6);
    w.window().set_size(slint::LogicalSize::new(960.0, 900.0));
    w.set_selected_site(0);
    w.set_can_upload(true);
    let drops = watch_drops(&w);
    assert!(
        pane_named(&w, "example-shop.com").absolute_position().y
            > pane_named(&w, "This PC").absolute_position().y
    );
    drag(
        &w,
        row_center(&w, "local-00001"),
        row_center(&w, "remote-00005"),
    );
    drag(
        &w,
        row_center(&w, "local-00001"),
        row_center(&w, "remote-00003"),
    );
    assert_eq!(*drops.to_remote.borrow(), [5, -1]);
}

/// A drag starts after 4 px of movement: until then it is a click, and the ghost shows the
/// number of rows being carried.
#[test]
fn a_drag_starts_after_four_pixels_and_shows_a_ghost_with_the_count() {
    let w = window_for_drags();
    let drops = watch_drops(&w);
    w.set_local_selected_count(3);
    let from = row_center(&w, "local-00001");
    let ghost = |text: &str| chips(&w, text);

    press_at(&w, from, PointerEventButton::Left);
    move_to(&w, LogicalPosition::new(from.x + 3.0, from.y));
    assert_eq!(ghost("3 items"), 0, "3 px is not a drag yet");
    move_to(&w, LogicalPosition::new(from.x + 3.0, from.y + 2.0));
    assert_eq!(ghost("3 items"), 0, "nor is 3.6 px");
    move_to(&w, LogicalPosition::new(from.x + 4.0, from.y));
    assert_eq!(ghost("3 items"), 1, "4 px is");
    move_to(&w, row_center(&w, "remote-00005"));
    assert_eq!(ghost("3 items"), 1);
    release_at(&w, row_center(&w, "remote-00005"), PointerEventButton::Left);
    assert_eq!(ghost("3 items"), 0, "the ghost goes with the button");
    assert_eq!(*drops.to_remote.borrow(), [5]);

    // One row reads "1 item".
    w.set_local_selected_count(1);
    press_at(&w, from, PointerEventButton::Left);
    move_to(&w, LogicalPosition::new(from.x + 20.0, from.y));
    assert_eq!(ghost("1 item"), 1);
    assert_eq!(ghost("1 items"), 0);
    release_at(&w, from, PointerEventButton::Left);

    // A press and release without a move is a click, and no drop.
    press_at(&w, from, PointerEventButton::Left);
    release_at(&w, from, PointerEventButton::Left);
    assert_eq!(drops.to_remote.borrow().len(), 1);
}

/// A pane that has nowhere to send its rows (the toolbar rule is off) does not start a drag.
#[test]
fn a_drag_is_inert_while_the_toolbar_rule_is_off() {
    let w = window_for_drags();
    w.set_can_upload(false);
    let drops = watch_drops(&w);
    w.set_local_selected_count(2);
    let from = row_center(&w, "local-00001");
    press_at(&w, from, PointerEventButton::Left);
    move_to(&w, LogicalPosition::new(from.x + 20.0, from.y));
    assert_eq!(chips(&w, "2 items"), 0, "no ghost");
    move_to(&w, row_center(&w, "remote-00005"));
    assert_eq!(chips(&w, "Release to upload to remote-00005.txt"), 0);
    release_at(&w, row_center(&w, "remote-00005"), PointerEventButton::Left);
    assert!(drops.to_remote.borrow().is_empty());
    // The cloud pane's rows go only while Download is on.
    w.set_can_download(false);
    let from = row_center(&w, "remote-00002");
    drag(&w, from, row_center(&w, "local-00005"));
    assert!(drops.to_local.borrow().is_empty());
}

/// Esc gives the drag up: the ghost goes, the drop does nothing, and the next drag is whole.
#[test]
fn escape_cancels_drag() {
    let w = window_for_drags();
    let drops = watch_drops(&w);
    w.set_local_selected_count(2);
    let from = row_center(&w, "local-00001");
    let target = row_center(&w, "remote-00005");

    press_at(&w, from, PointerEventButton::Left);
    move_to(&w, LogicalPosition::new(from.x + 20.0, from.y));
    move_to(&w, target);
    assert_eq!(chips(&w, "2 items"), 1);
    assert_eq!(chips(&w, "Release to upload to remote-00005.txt"), 1);
    tap(w.window(), Key::Escape);
    assert_eq!(chips(&w, "2 items"), 0, "the ghost is gone");
    assert_eq!(
        chips(&w, "Release to upload to remote-00005.txt"),
        0,
        "and so is the chip"
    );
    // Moving on does not bring it back, and letting go over the folder drops nothing.
    move_to(&w, row_center(&w, "remote-00000"));
    assert_eq!(chips(&w, "2 items"), 0);
    release_at(&w, row_center(&w, "remote-00000"), PointerEventButton::Left);
    assert!(drops.to_remote.borrow().is_empty());

    // The next drag works as if nothing had happened.
    drag(&w, from, target);
    assert_eq!(*drops.to_remote.borrow(), [5]);
    // Esc with no drag is no business of the drag.
    tap(w.window(), Key::Escape);
    drag(&w, from, row_center(&w, "remote-00000"));
    assert_eq!(*drops.to_remote.borrow(), [5, 0]);
}

/// Pressing a row that is selected waits for the button to come up before it narrows the
/// selection, so a whole selection can be dragged; a Ctrl or Shift press acts at once.
#[test]
fn dragging_a_selection_keeps_it() {
    let w = window_for_drags();
    let drops = watch_drops(&w);
    mark_selected(w.get_local_rows(), &[1, 2, 3]);
    w.set_local_selected_count(3);

    // A drag from a selected row: no pick at all, just the drop.
    drag(
        &w,
        row_center(&w, "local-00002"),
        row_center(&w, "remote-00005"),
    );
    assert_eq!(*drops.to_remote.borrow(), [5]);
    assert!(
        drops.picks.borrow().is_empty(),
        "the selection was not collapsed: {:?}",
        drops.picks.borrow()
    );

    // A click on it (no drag) narrows the selection when the button comes up.
    let at = row_center(&w, "local-00002");
    press_at(&w, at, PointerEventButton::Left);
    assert!(drops.picks.borrow().is_empty(), "not yet");
    release_at(&w, at, PointerEventButton::Left);
    assert_eq!(*drops.picks.borrow(), [(0, 2, false, false)]);

    // With Ctrl or Shift the press acts at once, selected row or not.
    drops.picks.borrow_mut().clear();
    chord_click(&w, &[Key::Control], row_center(&w, "local-00003"));
    chord_click(&w, &[Key::Shift], row_center(&w, "local-00001"));
    assert_eq!(
        *drops.picks.borrow(),
        [(0, 3, true, false), (0, 1, false, true)]
    );
    // And a row that is not selected acts at once, so it is what is dragged.
    drops.picks.borrow_mut().clear();
    press_at(&w, row_center(&w, "local-00004"), PointerEventButton::Left);
    assert_eq!(*drops.picks.borrow(), [(0, 4, false, false)]);
    release_at(&w, row_center(&w, "local-00004"), PointerEventButton::Left);
}

/// Task 22: while files from Explorer hover over the cloud pane its chip says "Release to upload",
/// and over a folder row names it: "Release to upload to {folder}". A long name is cut short.
#[test]
fn hover_text_names_the_folder() {
    let w = window_for_drags();
    let remote = pane_named(&w, "example-shop.com");
    w.set_drop_hover_remote(true);
    assert_eq!(chips(&w, "Release to upload"), 1);
    assert_eq!(chips(&w, "Release to upload to remote-00005.txt"), 0);

    w.set_drop_hover_row(5);
    assert_eq!(chips(&w, "Release to upload to remote-00005.txt"), 1);
    assert_eq!(chips(&w, "Release to upload"), 0);
    w.set_drop_hover_row(0);
    assert_eq!(chips(&w, "Release to upload to remote-00000.txt"), 1);
    w.set_drop_hover_row(-1);
    assert_eq!(chips(&w, "Release to upload"), 1);
    w.set_drop_hover_remote(false);
    assert_eq!(chips(&w, "Release to upload"), 0);

    // The words the app builds for a drop on a folder name it too.
    let batch = crate::drop_hook::DropBatch {
        side: crate::drop_hook::PaneSide::Remote,
        row: Some(5),
        paths: vec![r"C:\a".into(), r"C:\b".into()],
    };
    assert_eq!(
        crate::copy::drop_status(&batch, Some("example-shop.com"), Some("uploads")),
        "Uploading arrives in the next build (I2): 2 items dropped on the uploads folder in example-shop.com."
    );

    // A very long folder name stays inside the pane.
    let long = "a-folder-with-a-very-long-name-that-would-not-fit-in-the-pane-at-all-".repeat(3);
    let mut rows: Vec<crate::viewmodel::RowData> = row_data("remote", 3);
    rows[0].name = long.clone();
    w.set_remote_rows(to_slint_rows(&rows));
    w.set_drop_hover_remote(true);
    w.set_drop_hover_row(0);
    let chip = ElementHandle::find_by_accessible_label(&w, &format!("Release to upload to {long}"))
        .next()
        .expect("the chip names the folder");
    let (c, cs) = (chip.absolute_position(), chip.size());
    let (p, ps) = (remote.absolute_position(), remote.size());
    assert!(
        c.x >= p.x && c.x + cs.width <= p.x + ps.width + 0.5,
        "the chip ({}..{}) is wider than the pane ({}..{})",
        c.x,
        c.x + cs.width,
        p.x,
        p.x + ps.width
    );
}

/// During a drag of the cloud selection the This PC pane says "Release to download", naming the
/// folder row it is over.
#[test]
fn the_local_pane_offers_to_download_and_names_the_folder() {
    let w = window_for_drags();
    let drops = watch_drops(&w);
    w.set_remote_selected_count(1);
    let from = row_center(&w, "remote-00002");
    press_at(&w, from, PointerEventButton::Left);
    move_to(&w, LogicalPosition::new(from.x + 20.0, from.y));
    move_to(&w, row_center(&w, "local-00005"));
    assert_eq!(chips(&w, "Release to download to local-00005.txt"), 1);
    move_to(&w, row_center(&w, "local-00003"));
    assert_eq!(chips(&w, "Release to download"), 1);
    assert_eq!(chips(&w, "Release to download to local-00003.txt"), 0);
    move_to(&w, row_center(&w, "remote-00001"));
    assert_eq!(
        chips(&w, "Release to download"),
        0,
        "not over This PC any more"
    );
    assert_eq!(
        chips(&w, "Release to upload"),
        0,
        "the cloud pane is where it began"
    );
    release_at(&w, row_center(&w, "remote-00001"), PointerEventButton::Left);
    assert!(drops.to_local.borrow().is_empty());
}

/// Ctrl+V in the cloud pane asks to paste, while Upload can be pressed; This PC's list does not.
#[test]
fn ctrl_v_pastes_in_the_cloud_pane_only_while_upload_is_on() {
    let w = window_with_panes(3, 3);
    let pastes = Rc::new(Cell::new(0));
    w.on_paste({
        let pastes = pastes.clone();
        move || pastes.set(pastes.get() + 1)
    });
    tab(&w, REMOTE_LIST);
    chord(w.window(), &[Key::Control], "v");
    assert_eq!(pastes.get(), 0, "Upload is off");
    w.set_can_upload(true);
    chord(w.window(), &[Key::Control], "v");
    chord(w.window(), &[Key::Control], "V");
    assert_eq!(pastes.get(), 2);

    // Not with other modifiers, not a plain V, not while the key is held.
    tap(w.window(), "v");
    chord(w.window(), &[Key::Control, Key::Shift], "v");
    chord(w.window(), &[Key::Control, Key::Alt], "v");
    w.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    w.window()
        .dispatch_event(WindowEvent::KeyPressRepeated { text: "v".into() });
    w.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    assert_eq!(pastes.get(), 2);

    // This PC's list has nothing to paste into.
    let pc = window_with_panes(3, 3);
    pc.set_can_upload(true);
    let pastes = Rc::new(Cell::new(0));
    pc.on_paste({
        let pastes = pastes.clone();
        move || pastes.set(pastes.get() + 1)
    });
    tab(&pc, LOCAL_LIST);
    chord(pc.window(), &[Key::Control], "v");
    assert_eq!(pastes.get(), 0);
}

// ---- Task 25, item 8: a Refresh button in each pane's header ----

/// Polish 8 (spec 8.2.1): each pane's header has a Refresh button. It lists its own pane, whichever
/// was used last; the cloud pane's is off while no site is chosen, and says so.
#[test]
fn polish_8_each_pane_has_a_refresh_button_for_its_own_pane() {
    let w = window_with_panes(3, 3);
    w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
    let buttons = || {
        let mut found: Vec<ElementHandle> = ElementHandle::find_by_accessible_label(&w, "Refresh")
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Button))
            .collect();
        found.sort_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x));
        found
    };
    assert_eq!(buttons().len(), 2, "one in each header");
    let (local, remote) = (
        pane_named(&w, "This PC"),
        pane_named(&w, "No site selected"),
    );
    for (button, pane) in buttons().iter().zip([&local, &remote]) {
        let (b, p) = (button.absolute_position(), pane.absolute_position());
        assert!(
            b.x > p.x && b.y >= p.y && b.y < p.y + 37.0,
            "in the pane's header"
        );
    }

    // With no site chosen the cloud pane's is off, and says why.
    assert_eq!(buttons()[0].accessible_enabled(), Some(true));
    assert_eq!(description(&buttons()[0]), "");
    assert_eq!(buttons()[1].accessible_enabled(), Some(false));
    assert_eq!(description(&buttons()[1]), "No site selected");
    w.set_selected_site(0);
    assert_eq!(buttons()[1].accessible_enabled(), Some(true));
    assert_eq!(description(&buttons()[1]), "");

    // Each lists its own pane: it makes it the pane in use, then asks for a refresh.
    let log = Rc::new(RefCell::new(Vec::new()));
    w.on_refresh({
        let (log, weak) = (log.clone(), w.as_weak());
        move || {
            log.borrow_mut()
                .push(weak.upgrade().unwrap().get_local_pane_active())
        }
    });
    click(w.window(), center(&buttons()[0]), 1);
    click(w.window(), center(&buttons()[1]), 1);
    click(w.window(), center(&buttons()[0]), 1);
    assert_eq!(*log.borrow(), [true, false, true]);
    // The keyboard reaches them, in order: after the list and the crumbs.
    w.set_local_crumbs(crumb_names(&["D:", "sites"]));
    log.borrow_mut().clear();
    tab(&w, LOCAL_LIST + 1 + 1);
    tap(w.window(), Key::Return);
    assert_eq!(*log.borrow(), [true]);
    // Assistive technology can press it.
    buttons()[1].invoke_accessible_default_action();
    assert_eq!(*log.borrow(), [true, false]);
}

fn row_center_of(w: &AppWindow, name: &str) -> LogicalPosition {
    center(&row_starting(w, name))
}

/// Opens the menu of the row at `at`, unless a menu is open already.
fn reopen_menu(w: &AppWindow, at: LogicalPosition) {
    if !menu_is_open(w) {
        right_click(w, at);
    }
}

/// Starts the testing backend on this thread, once: a test that opens a second window must not
/// start it again.
fn init_headless() {
    thread_local! {
        static STARTED: Cell<bool> = const { Cell::new(false) };
    }
    if !STARTED.with(|started| started.replace(true)) {
        testing::init_no_event_loop();
    }
}
