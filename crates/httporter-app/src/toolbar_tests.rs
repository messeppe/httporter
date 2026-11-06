// SPDX-License-Identifier: MIT OR Apache-2.0

//! The toolbar row, laid out headlessly at the window's usual width (1280 px) and at its narrowest
//! (960 px): nothing in it reaches under the caption buttons, as the row gives way (the title
//! elides, the search field narrows, the connection pill goes, then the field folds into a
//! magnifier button), and the search field is the mockup's capsule with the magnifier inside it.
//! Keys and clicks come from the helpers of `keyboard_tests.rs`.

use crate::keyboard_tests::{center, chord, click, site, tap, toolbar_button};
use crate::{AppWindow, ProviderTile, Theme};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
use slint::platform::Key;
use slint::{ComponentHandle, LogicalSize, ModelRc, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const PLACEHOLDER: &str = "Search example-shop.com";
const CONNECTION: &str = "HTTPS · 21 ms";
const CAPTIONS: [&str; 4] = ["Minimize", "Maximize", "Restore", "Close"];

fn init_headless() {
    thread_local! {
        static STARTED: Cell<bool> = const { Cell::new(false) };
    }
    if !STARTED.with(|started| started.replace(true)) {
        testing::init_no_event_loop();
    }
}

/// The main window `width` wide, with a site open and the toolbar filled as the app fills it:
/// title and subtitle, the connection pill, Publish on, and the search field.
fn window(width: f32, view_mode: i32, text_scale: f32) -> AppWindow {
    init_headless();
    let w = AppWindow::new().unwrap();
    w.global::<Theme>().set_text_scale(text_scale);
    w.set_sites(ModelRc::new(VecModel::from(vec![site(
        "example-shop.com",
        ProviderTile::Bunny,
        "B",
    )])));
    w.set_selected_site(0);
    w.set_endpoint_text("Bunny Storage · exampleshop".into());
    w.set_connection_text(CONNECTION.into());
    w.set_connection_ok(true);
    w.set_can_publish(true);
    w.set_search_available(true);
    w.set_search_placeholder(PLACEHOLDER.into());
    w.set_view_mode(view_mode);
    w.show().unwrap();
    w.window().set_size(LogicalSize::new(width, 800.0));
    settle();
    w
}

/// Runs the change handlers and lets the layout follow.
fn settle() {
    slint::platform::update_timers_and_animations();
}

/// Types `text` into whatever has the focus, one key at a time.
fn type_text(w: &AppWindow, text: &str) {
    for ch in text.chars() {
        tap(w.window(), ch.to_string());
    }
}

/// Left and right edge, and top and bottom.
fn bounds(e: &ElementHandle) -> (f32, f32, f32, f32) {
    let (p, s) = (e.absolute_position(), e.size());
    (p.x, p.x + s.width, p.y, p.y + s.height)
}

fn minimize(w: &AppWindow) -> ElementHandle {
    toolbar_button(w, "Minimize")
}

/// The search field's text input, while the field shows.
fn search_input(w: &AppWindow) -> Option<ElementHandle> {
    ElementHandle::find_by_accessible_label(w, PLACEHOLDER)
        .find(|e| e.accessible_role() == Some(AccessibleRole::TextInput))
}

/// The search field's capsule, while it shows.
fn search_pill(w: &AppWindow) -> Option<ElementHandle> {
    ElementQuery::from_root(w)
        .match_type_name("SearchField")
        .find_first()
}

/// The magnifier drawn inside the capsule, while it shows.
fn magnifier(w: &AppWindow) -> Option<ElementHandle> {
    ElementHandle::find_by_element_id(w, "SearchField::magnifier").next()
}

/// The magnifier button the field folds into, while it shows.
fn search_button(w: &AppWindow) -> Option<ElementHandle> {
    ElementHandle::find_by_accessible_label(w, "Search")
        .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
}

fn connection_shown(w: &AppWindow) -> bool {
    ElementHandle::find_by_accessible_label(w, CONNECTION)
        .next()
        .is_some()
}

/// Every control of the toolbar row (its buttons, the search field) ends left of Minimize, and
/// Publish does in particular.
fn assert_clear_of_captions(w: &AppWindow, what: &str) {
    let min = minimize(w);
    let (min_left, _, top, bottom) = bounds(&min);
    let publish = bounds(&toolbar_button(w, "Publish"));
    assert!(
        publish.1 <= min_left + 0.01,
        "{what}: Publish ends at {} under Minimize at {min_left}",
        publish.1
    );
    let in_row = |e: &ElementHandle| {
        let (_, _, y0, y1) = bounds(e);
        y0 >= top - 0.01 && y1 <= bottom + 0.01
    };
    let mut controls: Vec<ElementHandle> = ElementQuery::from_root(w)
        .match_predicate(|e| {
            matches!(
                e.accessible_role(),
                Some(AccessibleRole::Button | AccessibleRole::TextInput)
            )
        })
        .find_all()
        .into_iter()
        .filter(|e| {
            !e.accessible_label()
                .is_some_and(|label| CAPTIONS.contains(&label.as_str()))
        })
        .filter(in_row)
        .collect();
    controls.extend(search_pill(w));
    assert!(
        controls.len() >= 5,
        "{what}: only {} controls",
        controls.len()
    );
    for e in &controls {
        let (_, right, _, _) = bounds(e);
        assert!(
            right <= min_left + 0.01,
            "{what}: {:?} ({:?}) ends at {right}, under Minimize at {min_left}",
            e.accessible_label(),
            e.type_name()
        );
    }
}

/// The capsule shows, with the magnifier inside it at its left and the input to its right.
fn assert_pill(w: &AppWindow, what: &str) -> (f32, f32) {
    let pill = search_pill(w).unwrap_or_else(|| panic!("{what}: no search field"));
    let (x0, x1, y0, y1) = bounds(&pill);
    let glass = magnifier(w).unwrap_or_else(|| panic!("{what}: no magnifier in the field"));
    let (g0, g1, gy0, gy1) = bounds(&glass);
    assert!(
        g0 > x0 && g1 < x1 && gy0 > y0 && gy1 < y1,
        "{what}: the magnifier {:?} is not inside the field {:?}",
        (g0, g1),
        (x0, x1)
    );
    let input = search_input(w).unwrap_or_else(|| panic!("{what}: no search input"));
    let (i0, i1, _, _) = bounds(&input);
    assert!(
        i0 >= g1 && i1 <= x1,
        "{what}: the input is not right of the magnifier"
    );
    assert_eq!(
        input.accessible_placeholder_text().as_deref(),
        Some(PLACEHOLDER),
        "{what}"
    );
    (x0, x1)
}

#[test]
fn nothing_reaches_under_the_caption_buttons() {
    for text_scale in [1.0, 1.5] {
        for view_mode in [0, 1] {
            for width in [1280.0, 960.0] {
                let what = format!("{width} px, view {view_mode}, text {text_scale}");
                let w = window(width, view_mode, text_scale);
                let (min_left, min_right, _, _) = bounds(&minimize(&w));
                assert!(
                    (min_right - (width - 92.0)).abs() < 0.5,
                    "{what}: Minimize at {min_left}..{min_right}"
                );
                assert_clear_of_captions(&w, &what);
                // The search is there, as the capsule or as its button.
                assert!(
                    search_pill(&w).is_some() || search_button(&w).is_some(),
                    "{what}: no search"
                );
            }
        }
    }
}

#[test]
fn at_1280_the_search_field_is_the_full_capsule_beside_everything() {
    let w = window(1280.0, 0, 1.0);
    let (x0, x1) = assert_pill(&w, "1280");
    assert!(
        (x1 - x0 - 220.0).abs() < 0.5,
        "the field is {} wide",
        x1 - x0
    );
    // In the mockup's place: after Publish, before the caption buttons.
    let publish = bounds(&toolbar_button(&w, "Publish"));
    assert!(x0 > publish.1, "the field is not after Publish");
    assert!(connection_shown(&w), "no connection pill at 1280");
    assert!(search_button(&w).is_none());
}

/// The title's width, while it shows.
fn title_width(w: &AppWindow) -> Option<f32> {
    let top = bounds(&minimize(w)).3;
    ElementHandle::find_by_accessible_label(w, "example-shop.com")
        .filter(|e| e.accessible_role() == Some(AccessibleRole::Text))
        .find(|e| bounds(e).3 <= top)
        .map(|e| e.size().width)
}

#[test]
fn the_row_gives_way_in_order_as_the_window_narrows() {
    // The inspector view: its toolbar is the wider one.
    let w = window(1280.0, 1, 1.0);
    const STEP: f32 = 8.0;
    // The narrowest the title elides to (4 × the 15 px title font), and the search field's
    // full and least widths.
    let (title_min, full, least) = (60.0, 220.0, 140.0);
    let mut last_field = f32::MAX;
    let mut had_connection = true;
    let mut narrowed_with_connection = false;
    let mut connection_went = false;
    let mut folded = false;
    let mut width = 1280.0;
    while width >= 960.0 {
        w.window().set_size(LogicalSize::new(width, 800.0));
        settle();
        let what = format!("{width} px");
        assert_clear_of_captions(&w, &what);
        let connection = connection_shown(&w);
        assert!(
            connection || !had_connection || last_field <= least + STEP + 0.01,
            "{what}: the connection pill went with the field still {last_field} wide"
        );
        if had_connection && !connection {
            connection_went = true;
            // The room the pill leaves goes to the field first.
            last_field = f32::MAX;
        }
        assert!(!connection || had_connection, "{what}: the pill came back");
        match search_pill(&w) {
            Some(_) => {
                assert!(!folded, "{what}: the field unfolded");
                let (x0, x1) = assert_pill(&w, &what);
                let field = x1 - x0;
                assert!(field <= last_field + 0.01, "{what}: the field grew");
                assert!(
                    (least - 0.01..=full + 0.01).contains(&field),
                    "{what}: the field is {field} wide"
                );
                // The title elides before the field narrows.
                if field < full - 0.01 {
                    let title = title_width(&w).unwrap_or_else(|| panic!("{what}: no title"));
                    assert!(
                        title <= title_min + 0.5,
                        "{what}: the field narrowed to {field} with the title {title} wide"
                    );
                    narrowed_with_connection |= connection;
                }
                last_field = field;
            }
            None => {
                assert!(search_button(&w).is_some(), "{what}: no search button");
                assert!(!connection, "{what}: the pill outlived the field");
                assert!(
                    last_field <= least + STEP + 0.01,
                    "{what}: the field folded at {last_field} wide"
                );
                folded = true;
            }
        }
        had_connection = connection;
        width -= STEP;
    }
    assert!(
        narrowed_with_connection,
        "the field never narrowed beside the pill"
    );
    assert!(connection_went, "the connection pill never went");
    assert!(folded, "the field never folded");
}

#[test]
fn the_folded_search_opens_from_its_button_and_ctrl_f_and_folds_on_escape() {
    let w = window(960.0, 1, 1.0);
    assert!(
        search_pill(&w).is_none(),
        "the field is not folded at 960 px"
    );
    let button = search_button(&w).expect("no search button");

    // A click opens the field in the title's place, focused.
    click(w.window(), center(&button), 1);
    settle();
    assert_pill(&w, "opened");
    assert_clear_of_captions(&w, "opened");
    assert!(search_button(&w).is_none());
    type_text(&w, "abc");
    settle();
    assert_eq!(w.get_search_text(), "abc");

    // Escape empties it, ends the search and folds it back, the focus on its button.
    let cancels = Rc::new(Cell::new(0));
    w.on_search_cancel({
        let cancels = cancels.clone();
        move || cancels.set(cancels.get() + 1)
    });
    tap(w.window(), Key::Escape);
    settle();
    assert_eq!(w.get_search_text(), "");
    assert!(cancels.get() >= 1);
    assert!(search_pill(&w).is_none(), "Escape left the field open");
    assert!(search_button(&w).is_some());

    // Ctrl+F opens it too, and Return searches.
    let submitted = Rc::new(RefCell::new(String::new()));
    w.on_search_submitted({
        let submitted = submitted.clone();
        move |text| *submitted.borrow_mut() = text.to_string()
    });
    chord(w.window(), &[Key::Control], "f");
    settle();
    assert_pill(&w, "Ctrl+F");
    type_text(&w, "logo");
    tap(w.window(), Key::Return);
    assert_eq!(submitted.borrow().as_str(), "logo");
}

#[test]
fn the_search_field_clears_from_its_button_and_from_escape() {
    let w = window(1280.0, 0, 1.0);
    let cancels = Rc::new(Cell::new(0));
    w.on_search_cancel({
        let cancels = cancels.clone();
        move || cancels.set(cancels.get() + 1)
    });
    // Ctrl+F needs a focus inside the window: the sidebar toggle.
    tap(w.window(), Key::Tab);
    chord(w.window(), &[Key::Control], "f");
    settle();
    type_text(&w, "abc");
    settle();
    assert_eq!(w.get_search_text(), "abc");
    let clear = toolbar_button(&w, "Clear search");
    let (c0, c1, _, _) = bounds(&clear);
    let (_, x1) = assert_pill(&w, "typed");
    assert!(
        c0 > x1 - 40.0 && c1 <= x1,
        "the clear button is not at the field's right end"
    );
    click(w.window(), center(&clear), 1);
    settle();
    assert_eq!(w.get_search_text(), "");
    assert_eq!(cancels.get(), 1);
    assert!(
        ElementHandle::find_by_accessible_label(&w, "Clear search")
            .next()
            .is_none(),
        "the clear button outlived the text"
    );
    // The field stays, focused, at its full width.
    type_text(&w, "x");
    settle();
    assert_eq!(w.get_search_text(), "x");
    tap(w.window(), Key::Escape);
    settle();
    assert_eq!(w.get_search_text(), "");
    assert_eq!(cancels.get(), 2);
    assert_pill(&w, "after Escape");
}

#[test]
fn minimize_draws_its_dash_and_settings_keeps_only_close() {
    // The main window: Minimize's glyph is a 10 x 1 px bar in the middle of the button.
    let w = window(1280.0, 0, 1.0);
    let (x0, x1, y0, y1) = bounds(&minimize(&w));
    let dash = ElementQuery::from_root(&w)
        .match_type_name("Rectangle")
        .find_all()
        .into_iter()
        .find(|e| {
            let (d0, d1, dy0, dy1) = bounds(e);
            d0 > x0 && d1 < x1 && dy0 > y0 && dy1 < y1 && (dy1 - dy0 - 1.0).abs() < 0.01
        })
        .expect("Minimize draws no dash");
    assert_eq!(dash.size().width, 10.0);
    let (d0, d1, _, _) = bounds(&dash);
    assert!(
        ((d0 + d1) / 2.0 - (x0 + x1) / 2.0).abs() < 0.5,
        "the dash is off centre"
    );

    // The Settings window's title row: Close alone, at the right edge.
    let s = crate::SettingsWindow::new().unwrap();
    s.show().unwrap();
    s.window().set_size(LogicalSize::new(720.0, 600.0));
    settle();
    for label in ["Minimize", "Maximize", "Restore"] {
        assert!(
            ElementHandle::find_by_accessible_label(&s, label)
                .next()
                .is_none(),
            "Settings has {label}"
        );
    }
    let close = ElementHandle::find_by_accessible_label(&s, "Close")
        .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .expect("Settings has no Close");
    let (c0, c1, _, _) = bounds(&close);
    let width = s.window().size().width as f32 / s.window().scale_factor();
    assert!(
        (c1 - width).abs() < 0.5 && (c1 - c0 - 46.0).abs() < 0.5,
        "Close at {c0}..{c1} in a window {width} wide"
    );
}
