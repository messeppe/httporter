//! Files dragged in from Windows Explorer.
//!
//! Slint 1.18's `DropArea` does not receive files dragged in from other applications on Windows,
//! so this listens to the winit window's own events instead: `HoveredFile` when a drag with files
//! enters the window, `HoveredFileCancelled` when it leaves, `DroppedFile` when it is released.
//! winit sends one event per file and no marker for the end of the list, so the files of a drop
//! are collected by [`DropTracker`] and handed over from a zero-length timer, which runs once the
//! last `DroppedFile` of the drop has been delivered.
//!
//! The events carry no position, and none arrive while the files move over the window. The cursor
//! is read from the system instead (`GetCursorPos`, in screen pixels), moved into the window's
//! logical pixels and compared with where the panes are ([`PaneLayout`]). While files hover, a
//! timer does this every [`HOVER_POLL`] to tell the UI whether they are over the cloud pane.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use slint::{Timer, TimerMode};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// How often the pane under the dragged files is looked up while they hover over the window.
const HOVER_POLL: Duration = Duration::from_millis(50);

/// The two panes a drop can land on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneSide {
    /// "This PC".
    Local,
    /// The cloud site.
    Remote,
}

/// Where the cloud pane is, in the window's logical pixels: the `pane-split`, `panes-stacked` and
/// `remote-pane-y` outputs of `AppWindow`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneLayout {
    /// Absolute x of the cloud pane's left edge (side by side).
    pub split_x: f32,
    /// The panes are one above the other: `split_x` says nothing then.
    pub stacked: bool,
    /// Absolute y of the cloud pane's top edge (stacked).
    pub remote_y: f32,
}

impl Default for PaneLayout {
    /// A window with no cloud pane anywhere, where every point is over This PC: what to use when
    /// the window is gone and nobody can be asked.
    fn default() -> Self {
        PaneLayout {
            split_x: f32::INFINITY,
            stacked: false,
            remote_y: f32::INFINITY,
        }
    }
}

/// Which pane the point (`x`, `y`) is over: the cloud pane when it is right of `split_x` (side by
/// side) or below `remote_y` (stacked), This PC otherwise. The gap between the panes counts as
/// This PC.
pub fn pane_at(x: f32, y: f32, layout: PaneLayout) -> PaneSide {
    let over_remote = if layout.stacked {
        y >= layout.remote_y
    } else {
        x >= layout.split_x
    };
    if over_remote {
        PaneSide::Remote
    } else {
        PaneSide::Local
    }
}

/// The files of one drop and the pane they landed on.
#[derive(Debug, PartialEq, Eq)]
pub struct DropBatch {
    pub side: PaneSide,
    pub paths: Vec<PathBuf>,
}

/// Collects the files of a drag: whether files are hovering and the files dropped so far.
#[derive(Default)]
pub struct DropTracker {
    hovering: bool,
    paths: Vec<PathBuf>,
    /// Cursor position, in logical pixels, when the first file of `paths` was dropped.
    at: (f32, f32),
}

impl DropTracker {
    /// Files are being dragged over the window.
    pub fn hovered(&mut self) {
        self.hovering = true;
    }

    /// The drag left the window without a drop. Files dropped but not yet taken are forgotten
    /// (Windows reports no cancel after a drop, so this only ever clears leftovers).
    pub fn cancelled(&mut self) {
        self.hovering = false;
        self.paths.clear();
    }

    /// Whether files are hovering over the window (from `hovered` until a drop or a cancel).
    pub fn is_hovering(&self) -> bool {
        self.hovering
    }

    /// One file of a drop landed with the cursor at (`x`, `y`) in the window's logical pixels.
    /// Returns true for the first file of a batch, the one that should schedule the `take`.
    pub fn dropped(&mut self, path: PathBuf, x: f32, y: f32) -> bool {
        self.hovering = false;
        let first = self.paths.is_empty();
        if first {
            self.at = (x, y);
        }
        self.paths.push(path);
        first
    }

    /// The files dropped since the last `take`, with the pane the first of them landed on, or
    /// `None` when there are none.
    pub fn take(&mut self, layout: PaneLayout) -> Option<DropBatch> {
        if self.paths.is_empty() {
            return None;
        }
        Some(DropBatch {
            side: pane_at(self.at.0, self.at.1, layout),
            paths: std::mem::take(&mut self.paths),
        })
    }
}

/// Where the window's content is on the screen, as of the last drag event.
#[derive(Clone, Copy)]
struct Geometry {
    /// Screen position of the content's top left corner, in physical pixels.
    origin: (i32, i32),
    /// Physical pixels per logical pixel.
    scale: f32,
}

/// A cursor position that is over no pane (Local, whatever the layout), for when the system will
/// not say where the cursor is.
const NOWHERE: (f32, f32) = (-1.0, -1.0);

/// A screen position in the window's logical pixels.
fn to_logical(cursor: (i32, i32), geometry: Geometry) -> (f32, f32) {
    (
        (cursor.0 - geometry.origin.0) as f32 / geometry.scale,
        (cursor.1 - geometry.origin.1) as f32 / geometry.scale,
    )
}

/// The cursor in screen pixels.
fn cursor_on_screen() -> Option<(i32, i32)> {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `GetCursorPos` writes only to the `POINT` it is given.
    (unsafe { GetCursorPos(&mut point) } != 0).then_some((point.x, point.y))
}

/// What the winit callback and the timers share.
struct Hook {
    tracker: RefCell<DropTracker>,
    geometry: Cell<Geometry>,
    /// What `on_hover` was last told.
    over_remote: Cell<bool>,
    /// Looks up the pane under the cursor while files hover.
    poll: Timer,
    on_hover: Box<dyn Fn(bool)>,
    on_drop: Box<dyn Fn(DropBatch)>,
    layout: Box<dyn Fn() -> PaneLayout>,
    /// Where the cursor is on the screen, in physical pixels. The system's answer
    /// ([`cursor_on_screen`]); a test says where it likes.
    cursor_on_screen: Box<dyn Fn() -> Option<(i32, i32)>>,
}

impl Hook {
    fn new(
        on_hover: Box<dyn Fn(bool)>,
        on_drop: Box<dyn Fn(DropBatch)>,
        layout: Box<dyn Fn() -> PaneLayout>,
        cursor_on_screen: Box<dyn Fn() -> Option<(i32, i32)>>,
    ) -> Rc<Self> {
        Rc::new(Hook {
            tracker: RefCell::new(DropTracker::default()),
            geometry: Cell::new(Geometry {
                origin: (0, 0),
                scale: 1.0,
            }),
            over_remote: Cell::new(false),
            poll: Timer::default(),
            on_hover,
            on_drop,
            layout,
            cursor_on_screen,
        })
    }

    /// Remember where the window is: the drag events do not say, and the window cannot move
    /// while the user is dragging files over it.
    fn capture_geometry(&self, window: &slint::Window) {
        use slint::winit_030::WinitWindowAccessor;
        if let Some(Some(inner)) = window.with_winit_window(|w| w.inner_position().ok()) {
            self.geometry.set(Geometry {
                origin: (inner.x, inner.y),
                scale: window.scale_factor(),
            });
        }
    }

    /// The cursor in the window's logical pixels.
    fn cursor(&self) -> (f32, f32) {
        (self.cursor_on_screen)().map_or(NOWHERE, |c| to_logical(c, self.geometry.get()))
    }

    fn set_over_remote(&self, over_remote: bool) {
        if self.over_remote.replace(over_remote) != over_remote {
            (self.on_hover)(over_remote);
        }
    }

    /// Tell the UI whether the hovering files are over the cloud pane.
    fn refresh_hover(&self) {
        let (x, y) = self.cursor();
        self.set_over_remote(pane_at(x, y, (self.layout)()) == PaneSide::Remote);
    }

    /// Files started hovering over the window.
    fn start_hover(self: &Rc<Self>, window: &slint::Window) {
        self.tracker.borrow_mut().hovered();
        self.capture_geometry(window);
        self.refresh_hover();
        let hook: Weak<Hook> = Rc::downgrade(self);
        self.poll.start(TimerMode::Repeated, HOVER_POLL, move || {
            if let Some(hook) = hook.upgrade() {
                hook.refresh_hover();
            }
        });
    }

    /// The files stopped hovering, dropped or not.
    fn end_hover(&self) {
        self.poll.stop();
        self.set_over_remote(false);
    }

    /// One dropped file. The first of a drop schedules the hand-over of all of them.
    fn drop_file(self: &Rc<Self>, window: &slint::Window, path: &Path) {
        self.capture_geometry(window);
        self.end_hover();
        let (x, y) = self.cursor();
        let first = self.tracker.borrow_mut().dropped(path.to_path_buf(), x, y);
        if first {
            let hook = Rc::clone(self);
            // The timer fires once the event loop is done with the events of this drop, so all of
            // its files are in the tracker by then.
            Timer::single_shot(Duration::ZERO, move || {
                let layout = (hook.layout)();
                let batch = hook.tracker.borrow_mut().take(layout);
                if let Some(batch) = batch {
                    (hook.on_drop)(batch);
                }
            });
        }
    }
}

/// Listen for files dragged from Explorer onto `window`.
///
/// - `on_hover(true)` when files hover over the cloud pane, `on_hover(false)` when they leave it,
///   move to This PC, are dropped, or the drag is cancelled. Called only when the answer changes.
/// - `on_drop` gets the files of one drop and the pane they were dropped on.
/// - `layout` says where the cloud pane is at the moment it is asked, in logical pixels.
///
/// Call once per window, from the thread that runs the event loop.
pub fn install_drop_hook(
    window: &slint::Window,
    on_hover: impl Fn(bool) + 'static,
    on_drop: impl Fn(DropBatch) + 'static,
    layout: impl Fn() -> PaneLayout + 'static,
) {
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

    let hook = Hook::new(
        Box::new(on_hover),
        Box::new(on_drop),
        Box::new(layout),
        Box::new(cursor_on_screen),
    );

    window.on_winit_window_event(move |window, event| {
        match event {
            WindowEvent::HoveredFile(_) => {
                // One event per file: only the first starts the hover.
                if !hook.tracker.borrow().is_hovering() {
                    hook.start_hover(window);
                }
            }
            WindowEvent::HoveredFileCancelled => {
                hook.tracker.borrow_mut().cancelled();
                hook.end_hover();
            }
            WindowEvent::DroppedFile(path) => hook.drop_file(window, path),
            _ => {}
        }
        EventResult::Propagate
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panes side by side, the cloud pane starting at x = 640.
    const SIDE_BY_SIDE: PaneLayout = PaneLayout {
        split_x: 640.0,
        stacked: false,
        remote_y: 0.0,
    };

    /// Panes one above the other, the cloud pane starting at y = 400. `split_x` is deliberately
    /// stale (right of some of the points below): stacked, only `remote_y` counts.
    const STACKED: PaneLayout = PaneLayout {
        split_x: 640.0,
        stacked: true,
        remote_y: 400.0,
    };

    #[test]
    fn hover_then_drop_collects_one_batch() {
        let mut t = DropTracker::default();
        t.hovered();
        assert!(t.dropped("C:\\a.txt".into(), 900.0, 300.0));
        assert!(!t.dropped("C:\\b".into(), 900.0, 300.0));
        let b = t.take(SIDE_BY_SIDE).unwrap();
        assert!(matches!(b.side, PaneSide::Remote));
        assert_eq!(
            b.paths,
            vec![PathBuf::from("C:\\a.txt"), PathBuf::from("C:\\b")]
        );
        assert!(t.take(SIDE_BY_SIDE).is_none());
    }

    #[test]
    fn cancel_clears_and_next_drop_starts_fresh() {
        let mut t = DropTracker::default();
        t.hovered();
        t.cancelled();
        assert!(t.dropped("C:\\a.txt".into(), 100.0, 300.0));
        let b = t.take(SIDE_BY_SIDE).unwrap();
        assert!(matches!(b.side, PaneSide::Local));
        assert_eq!(b.paths, vec![PathBuf::from("C:\\a.txt")]);
    }

    #[test]
    fn cancel_discards_files_not_yet_taken() {
        let mut t = DropTracker::default();
        assert!(t.dropped("C:\\old".into(), 900.0, 300.0));
        t.cancelled();
        assert!(t.dropped("C:\\new".into(), 100.0, 300.0));
        let b = t.take(SIDE_BY_SIDE).unwrap();
        assert_eq!(b.side, PaneSide::Local);
        assert_eq!(b.paths, vec![PathBuf::from("C:\\new")]);
    }

    #[test]
    fn a_batch_lands_where_its_first_file_was_dropped() {
        let mut t = DropTracker::default();
        assert!(t.dropped("C:\\a".into(), 100.0, 300.0));
        assert!(!t.dropped("C:\\b".into(), 900.0, 300.0));
        assert_eq!(t.take(SIDE_BY_SIDE).unwrap().side, PaneSide::Local);
    }

    #[test]
    fn the_second_drop_after_a_take_is_a_new_batch() {
        let mut t = DropTracker::default();
        assert!(t.dropped("C:\\a".into(), 900.0, 300.0));
        t.take(SIDE_BY_SIDE).unwrap();
        assert!(t.dropped("C:\\b".into(), 900.0, 300.0));
        assert_eq!(
            t.take(SIDE_BY_SIDE).unwrap().paths,
            vec![PathBuf::from("C:\\b")]
        );
    }

    #[test]
    fn nothing_dropped_takes_nothing() {
        let mut t = DropTracker::default();
        t.hovered();
        assert!(t.take(SIDE_BY_SIDE).is_none());
    }

    #[test]
    fn hovering_lasts_until_a_drop_or_a_cancel() {
        let mut t = DropTracker::default();
        assert!(!t.is_hovering());
        t.hovered();
        t.hovered();
        assert!(t.is_hovering());
        t.cancelled();
        assert!(!t.is_hovering());
        t.hovered();
        t.dropped("C:\\a".into(), 0.0, 0.0);
        assert!(!t.is_hovering());
    }

    #[test]
    fn pane_at_split() {
        assert!(matches!(
            pane_at(639.0, 10.0, SIDE_BY_SIDE),
            PaneSide::Local
        ));
        assert!(matches!(
            pane_at(640.0, 10.0, SIDE_BY_SIDE),
            PaneSide::Remote
        ));
    }

    #[test]
    fn side_by_side_ignores_y() {
        assert_eq!(pane_at(100.0, 0.0, SIDE_BY_SIDE), PaneSide::Local);
        assert_eq!(pane_at(100.0, 5000.0, SIDE_BY_SIDE), PaneSide::Local);
        assert_eq!(pane_at(900.0, 0.0, SIDE_BY_SIDE), PaneSide::Remote);
        assert_eq!(pane_at(900.0, 5000.0, SIDE_BY_SIDE), PaneSide::Remote);
    }

    #[test]
    fn stacked_panes_split_on_y() {
        // The lower pane, with x left of `split_x`: the cloud pane.
        assert_eq!(pane_at(100.0, 450.0, STACKED), PaneSide::Remote);
        // The upper pane, with x right of `split_x`: still This PC.
        assert_eq!(pane_at(900.0, 100.0, STACKED), PaneSide::Local);
        assert_eq!(pane_at(900.0, 399.0, STACKED), PaneSide::Local);
        assert_eq!(pane_at(900.0, 400.0, STACKED), PaneSide::Remote);
    }

    #[test]
    fn a_drop_in_stacked_panes_is_placed_by_y() {
        let mut t = DropTracker::default();
        t.dropped("C:\\a".into(), 100.0, 450.0);
        assert_eq!(t.take(STACKED).unwrap().side, PaneSide::Remote);
        t.dropped("C:\\b".into(), 900.0, 100.0);
        assert_eq!(t.take(STACKED).unwrap().side, PaneSide::Local);
    }

    #[test]
    fn the_default_layout_has_no_cloud_pane() {
        assert_eq!(
            pane_at(5000.0, 5000.0, PaneLayout::default()),
            PaneSide::Local
        );
        let stacked = PaneLayout {
            stacked: true,
            ..PaneLayout::default()
        };
        assert_eq!(pane_at(5000.0, 5000.0, stacked), PaneSide::Local);
    }

    #[test]
    fn the_cursor_is_moved_into_the_window_and_scaled_to_logical_pixels() {
        let geometry = Geometry {
            origin: (100, 50),
            scale: 1.5,
        };
        assert_eq!(to_logical((1000, 650), geometry), (600.0, 400.0));
        // Left of and above the window's content: negative, so over no pane.
        let (x, y) = to_logical((99, 49), geometry);
        assert!(x < 0.0 && y < 0.0);
        assert_eq!(pane_at(x, y, SIDE_BY_SIDE), PaneSide::Local);
        assert_eq!(pane_at(NOWHERE.0, NOWHERE.1, STACKED), PaneSide::Local);
    }

    // ---- The hook's timers, on a clock that only moves when a test moves it ----
    //
    // The testing backend keeps its own time (`mock_elapsed_time`): a Slint timer fires when the
    // test says that long has passed, however long the test really took. So these tests neither
    // sleep nor depend on how busy the machine is.

    use std::time::Duration;

    use crate::AppWindow;
    use i_slint_backend_testing::{init_no_event_loop, mock_elapsed_time};
    use slint::ComponentHandle;

    /// A hook over a window of the testing backend, with a cursor to move and the calls it made.
    struct Bed {
        hook: Rc<Hook>,
        window: AppWindow,
        cursor: Rc<Cell<(i32, i32)>>,
        hovers: Rc<RefCell<Vec<bool>>>,
        drops: Rc<RefCell<Vec<DropBatch>>>,
    }

    /// The cursor starts over This PC (x = 100); the cloud pane starts at x = 640.
    fn bed() -> Bed {
        init_no_event_loop();
        let cursor = Rc::new(Cell::new((100, 10)));
        let hovers = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(RefCell::new(Vec::new()));
        let hook = Hook::new(
            Box::new({
                let hovers = hovers.clone();
                move |over_remote| hovers.borrow_mut().push(over_remote)
            }),
            Box::new({
                let drops = drops.clone();
                move |batch| drops.borrow_mut().push(batch)
            }),
            Box::new(|| SIDE_BY_SIDE),
            Box::new({
                let cursor = cursor.clone();
                move || Some(cursor.get())
            }),
        );
        let window = AppWindow::new().unwrap();
        window.show().unwrap();
        Bed {
            hook,
            window,
            cursor,
            hovers,
            drops,
        }
    }

    /// Lets `ms` milliseconds of the test's clock go by.
    fn pass(ms: u128) {
        mock_elapsed_time(Duration::from_millis(ms as u64));
    }

    const POLL_MS: u128 = HOVER_POLL.as_millis();

    #[test]
    fn hovering_looks_up_the_pane_every_poll_and_reports_only_a_change() {
        let b = bed();
        b.hook.start_hover(b.window.window());
        assert!(
            b.hovers.borrow().is_empty(),
            "over This PC: nothing to report"
        );

        // The cursor moves into the cloud pane, but nobody looks until the next poll.
        b.cursor.set((900, 10));
        pass(POLL_MS - 1);
        assert!(b.hovers.borrow().is_empty());
        pass(1);
        assert_eq!(*b.hovers.borrow(), [true]);

        // Polls that find the same answer say nothing.
        pass(4 * POLL_MS);
        assert_eq!(*b.hovers.borrow(), [true]);

        b.cursor.set((100, 10));
        pass(POLL_MS);
        assert_eq!(*b.hovers.borrow(), [true, false]);
    }

    #[test]
    fn the_polling_stops_when_the_files_stop_hovering() {
        let b = bed();
        b.hook.start_hover(b.window.window());
        b.cursor.set((900, 10));
        pass(POLL_MS);
        assert_eq!(*b.hovers.borrow(), [true]);

        // They left the window: the cloud pane is told at once, and the timer is gone.
        b.hook.end_hover();
        assert_eq!(*b.hovers.borrow(), [true, false]);
        pass(10 * POLL_MS);
        assert_eq!(*b.hovers.borrow(), [true, false]);
    }

    #[test]
    fn the_files_of_a_drop_are_handed_over_once_after_the_last_of_them() {
        let b = bed();
        b.cursor.set((900, 10));
        for name in [r"C:\a.txt", r"C:\b", r"C:\c"] {
            b.hook.drop_file(b.window.window(), Path::new(name));
        }
        assert!(
            b.drops.borrow().is_empty(),
            "winit has not finished delivering the drop"
        );

        pass(0);
        {
            let drops = b.drops.borrow();
            assert_eq!(drops.len(), 1, "one batch for the three files");
            assert_eq!(drops[0].side, PaneSide::Remote);
            assert_eq!(
                drops[0].paths,
                [r"C:\a.txt", r"C:\b", r"C:\c"].map(PathBuf::from)
            );
        }
        pass(10 * POLL_MS);
        assert_eq!(b.drops.borrow().len(), 1, "nothing is handed over twice");

        // The next drop is a batch of its own, wherever it lands.
        b.cursor.set((100, 10));
        b.hook.drop_file(b.window.window(), Path::new(r"C:\d"));
        pass(0);
        let drops = b.drops.borrow();
        assert_eq!(drops.len(), 2);
        assert_eq!(drops[1].side, PaneSide::Local);
        assert_eq!(drops[1].paths, [PathBuf::from(r"C:\d")]);
    }

    #[test]
    fn a_drop_ends_the_hover() {
        let b = bed();
        b.hook.start_hover(b.window.window());
        b.cursor.set((900, 10));
        pass(POLL_MS);
        assert_eq!(*b.hovers.borrow(), [true]);

        b.hook.drop_file(b.window.window(), Path::new(r"C:\a"));
        assert_eq!(*b.hovers.borrow(), [true, false]);
        // The poll is stopped, so it cannot light the pane again.
        pass(10 * POLL_MS);
        assert_eq!(*b.hovers.borrow(), [true, false]);
    }

    #[test]
    fn the_drop_chip_shows_on_the_cloud_pane_only_while_files_hover() {
        use i_slint_backend_testing::ElementHandle;

        init_no_event_loop();
        let w = AppWindow::new().unwrap();
        w.window().set_size(slint::LogicalSize::new(1280.0, 800.0));
        // Only a selected site lights up (see pane_tests for the pane with none).
        let site = crate::keyboard_tests::site("example-shop.com", crate::ProviderTile::Bunny, "B");
        w.set_sites(slint::ModelRc::new(slint::VecModel::from(vec![site])));
        w.set_selected_site(0);
        w.show().unwrap();
        let chips =
            || ElementHandle::find_by_accessible_label(&w, "Release to upload").collect::<Vec<_>>();

        assert!(chips().is_empty());
        w.set_drop_hover_remote(true);
        let shown = chips();
        assert_eq!(shown.len(), 1);
        assert!(!w.get_panes_stacked());
        assert!(shown[0].absolute_position().x >= w.get_pane_split());
        w.set_drop_hover_remote(false);
        assert!(chips().is_empty());
    }
}
