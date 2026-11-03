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
//! logical pixels and compared with where the panes and their rows are ([`PaneLayout`], asked of
//! the window each time). While files hover, a timer does this every [`HOVER_POLL`] to tell the UI
//! what is under them: a pane, and in the cloud pane possibly one of its rows ([`hit`]).
//!
//! Nothing here guesses. When the window will not say where its content is, or the system will not
//! say where the cursor is, a drop is not placed on a pane: it is ignored, and the app says so
//! ([`Hook::on_unlocated`]). A point over no pane (the toolbar, the status bar, the transfer
//! drawer, the gap between the panes) hits nothing.

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

/// What a point is over: a pane, and the row of its list when it is over one. A row is any row,
/// folder or file; only the app knows which it is.
pub type Target = (PaneSide, Option<usize>);

/// Where one pane is, in the window's logical pixels: the `local-bounds` and `remote-bounds`
/// outputs of `AppWindow`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PaneGeometry {
    /// The pane's top left corner, in the window.
    pub x: f32,
    pub y: f32,
    /// The pane's size.
    pub w: f32,
    pub h: f32,
    /// The y of the list's top edge, which is where its first row starts when it is not scrolled.
    pub list_top: f32,
    pub row_height: f32,
    /// How far the list is scrolled down, 0 or more.
    pub scroll_y: f32,
    /// How many rows the list has.
    pub rows: usize,
}

impl PaneGeometry {
    /// Whether `pos` is inside the pane.
    fn contains(&self, (x, y): (f32, f32)) -> bool {
        (self.x..self.x + self.w).contains(&x) && (self.y..self.y + self.h).contains(&y)
    }

    /// The row under the height `y`, if it is over the list and over a row (not the header, the
    /// column titles, or the empty space under the last row).
    fn row_at(&self, y: f32) -> Option<usize> {
        if y.is_nan() || !self.row_height.is_finite() || self.row_height <= 0.0 || y < self.list_top
        {
            return None;
        }
        let row = ((y - self.list_top + self.scroll_y) / self.row_height).floor();
        (0.0..self.rows as f32)
            .contains(&row)
            .then_some(row as usize)
    }
}

/// Where both panes are. They sit side by side or one above the other; the rectangles say which,
/// so nothing else has to.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PaneLayout {
    pub local: PaneGeometry,
    pub remote: PaneGeometry,
}

impl PaneLayout {
    /// The pane `side` names.
    pub fn pane(&self, side: PaneSide) -> &PaneGeometry {
        match side {
            PaneSide::Local => &self.local,
            PaneSide::Remote => &self.remote,
        }
    }
}

/// What the point `pos` (window logical pixels) is over: a pane, and its row when over one. A point
/// outside both panes' rectangles (the toolbar, the status bar, the drawer, the gap between the
/// panes) hits nothing. A pane with no size, such as the default layout's, is never hit.
pub fn hit(pos: (f32, f32), layout: &PaneLayout) -> Option<Target> {
    [PaneSide::Remote, PaneSide::Local]
        .into_iter()
        .find(|&side| layout.pane(side).contains(pos))
        .map(|side| (side, layout.pane(side).row_at(pos.1)))
}

/// The files of one drop and what they landed on.
#[derive(Debug, PartialEq, Eq)]
pub struct DropBatch {
    pub side: PaneSide,
    /// The row of the pane's list they landed on, if on one. Any row: the app checks for a folder.
    pub row: Option<usize>,
    pub paths: Vec<PathBuf>,
}

/// What became of the files of a drop once the pane layout is known.
#[derive(Debug, PartialEq, Eq)]
pub enum Dropped {
    /// They landed on a pane.
    On(DropBatch),
    /// They were released over no pane (the toolbar, the status bar, the drawer): nothing to do.
    Nowhere,
    /// Where they were released could not be told.
    Unlocated,
}

/// Collects the files of a drag: whether files are hovering and the files dropped so far.
#[derive(Default)]
pub struct DropTracker {
    hovering: bool,
    paths: Vec<PathBuf>,
    /// Cursor position, in logical pixels, when the first file of `paths` was dropped; `None`
    /// when it could not be told.
    at: Option<(f32, f32)>,
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

    /// One file of a drop landed with the cursor at `at` in the window's logical pixels (`None`
    /// when the cursor could not be placed). Returns true for the first file of a batch, the one
    /// that should schedule the `take`.
    pub fn dropped(&mut self, path: PathBuf, at: Option<(f32, f32)>) -> bool {
        self.hovering = false;
        let first = self.paths.is_empty();
        if first {
            self.at = at;
        }
        self.paths.push(path);
        first
    }

    /// The files dropped since the last `take`, with what the first of them landed on, or `None`
    /// when there are none.
    pub fn take(&mut self, layout: &PaneLayout) -> Option<Dropped> {
        if self.paths.is_empty() {
            return None;
        }
        let paths = std::mem::take(&mut self.paths);
        Some(match self.at {
            None => Dropped::Unlocated,
            Some(at) => match hit(at, layout) {
                None => Dropped::Nowhere,
                Some((side, row)) => Dropped::On(DropBatch { side, row, paths }),
            },
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

/// A screen position in the window's logical pixels. The origin is negative when the window is on
/// a monitor left of or above the primary one; so is the result for a point left of or above the
/// window's content.
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

/// Where `window`'s content is on the screen: the drag events do not say, and the window cannot
/// move while the user is dragging files over it. `None` when the window will not say (no winit
/// window under it, or no position for it).
fn window_geometry(window: &slint::Window) -> Option<Geometry> {
    use slint::winit_030::WinitWindowAccessor;
    let inner = window
        .with_winit_window(|w| w.inner_position().ok())
        .flatten()?;
    Some(Geometry {
        origin: (inner.x, inner.y),
        scale: window.scale_factor(),
    })
}

/// Says where a window's content is on the screen.
type GeometryOf = dyn Fn(&slint::Window) -> Option<Geometry>;

/// What the winit callback and the timers share.
struct Hook {
    tracker: RefCell<DropTracker>,
    /// Where the window's content is, as of the last drag event. `None` until one has said, and
    /// again when the window would not say: nothing is placed by a guess.
    geometry: Cell<Option<Geometry>>,
    /// What `on_hover` was last told.
    hover: Cell<Option<Target>>,
    /// Looks up what is under the cursor while files hover.
    poll: Timer,
    /// What is under the hovering files, told only when it changes; `None` when they are over no
    /// pane, or are gone.
    on_hover: Box<dyn Fn(Option<Target>)>,
    on_drop: Box<dyn Fn(DropBatch)>,
    /// A drop could not be placed, so it was ignored.
    on_unlocated: Box<dyn Fn()>,
    layout: Box<dyn Fn() -> PaneLayout>,
    /// Where the window's content is on the screen ([`window_geometry`]); a test says where it
    /// likes.
    window_geometry: Box<GeometryOf>,
    /// Where the cursor is on the screen, in physical pixels. The system's answer
    /// ([`cursor_on_screen`]); a test says where it likes.
    cursor_on_screen: Box<dyn Fn() -> Option<(i32, i32)>>,
}

impl Hook {
    fn new(
        on_hover: Box<dyn Fn(Option<Target>)>,
        on_drop: Box<dyn Fn(DropBatch)>,
        on_unlocated: Box<dyn Fn()>,
        layout: Box<dyn Fn() -> PaneLayout>,
        window_geometry: Box<GeometryOf>,
        cursor_on_screen: Box<dyn Fn() -> Option<(i32, i32)>>,
    ) -> Rc<Self> {
        Rc::new(Hook {
            tracker: RefCell::new(DropTracker::default()),
            geometry: Cell::new(None),
            hover: Cell::new(None),
            poll: Timer::default(),
            on_hover,
            on_drop,
            on_unlocated,
            layout,
            window_geometry,
            cursor_on_screen,
        })
    }

    /// Remember where the window is.
    fn capture_geometry(&self, window: &slint::Window) {
        self.geometry.set((self.window_geometry)(window));
    }

    /// The cursor in the window's logical pixels, or `None` when the window or the system will
    /// not say where either is.
    fn cursor(&self) -> Option<(f32, f32)> {
        let geometry = self.geometry.get()?;
        (self.cursor_on_screen)().map(|c| to_logical(c, geometry))
    }

    fn set_hover(&self, target: Option<Target>) {
        if self.hover.replace(target) != target {
            (self.on_hover)(target);
        }
    }

    /// Tell the UI what is under the hovering files.
    fn refresh_hover(&self) {
        let target = self.cursor().and_then(|at| hit(at, &(self.layout)()));
        self.set_hover(target);
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

    /// The files stopped hovering, dropped or not. The UI is told once, however many times this
    /// is called.
    fn end_hover(&self) {
        self.poll.stop();
        self.set_hover(None);
    }

    /// One dropped file. The first of a drop schedules the hand-over of all of them.
    fn drop_file(self: &Rc<Self>, window: &slint::Window, path: &Path) {
        self.capture_geometry(window);
        self.end_hover();
        let at = self.cursor();
        let first = self.tracker.borrow_mut().dropped(path.to_path_buf(), at);
        if first {
            let hook = Rc::clone(self);
            // The timer fires once the event loop is done with the events of this drop, so all of
            // its files are in the tracker by then.
            Timer::single_shot(Duration::ZERO, move || {
                let layout = (hook.layout)();
                let taken = hook.tracker.borrow_mut().take(&layout);
                match taken {
                    Some(Dropped::On(batch)) => (hook.on_drop)(batch),
                    Some(Dropped::Unlocated) => (hook.on_unlocated)(),
                    Some(Dropped::Nowhere) | None => {}
                }
            });
        }
    }
}

/// Listen for files dragged from Explorer onto `window`.
///
/// - `on_hover(target)` says what the hovering files are over: a pane and its row (see [`hit`]),
///   or `None` when they are over no pane, leave the window, are dropped, or the drag is
///   cancelled. Called only when the answer changes.
/// - `on_drop` gets the files of one drop and what they were dropped on.
/// - `on_unlocated` is called instead when the drop could not be placed (the window or the system
///   would not say where it was); the files are ignored.
/// - `layout` says where the panes and their rows are at the moment it is asked, in logical
///   pixels.
///
/// Call once per window, from the thread that runs the event loop.
pub fn install_drop_hook(
    window: &slint::Window,
    on_hover: impl Fn(Option<Target>) + 'static,
    on_drop: impl Fn(DropBatch) + 'static,
    on_unlocated: impl Fn() + 'static,
    layout: impl Fn() -> PaneLayout + 'static,
) {
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

    let hook = Hook::new(
        Box::new(on_hover),
        Box::new(on_drop),
        Box::new(on_unlocated),
        Box::new(layout),
        Box::new(window_geometry),
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

    /// A pane at (`x`, `y`) of 630 x 600 with ten 28 px rows below a 63 px header.
    fn pane(x: f32, y: f32) -> PaneGeometry {
        PaneGeometry {
            x,
            y,
            w: 630.0,
            h: 600.0,
            list_top: y + 63.0,
            row_height: 28.0,
            scroll_y: 0.0,
            rows: 10,
        }
    }

    /// Panes side by side, the cloud pane starting at x = 640.
    fn side_by_side() -> PaneLayout {
        PaneLayout {
            local: pane(8.0, 60.0),
            remote: pane(640.0, 60.0),
        }
    }

    /// Panes one above the other, the cloud pane starting at y = 700.
    fn stacked() -> PaneLayout {
        PaneLayout {
            local: pane(8.0, 60.0),
            remote: pane(8.0, 700.0),
        }
    }

    #[test]
    fn hover_then_drop_collects_one_batch() {
        let mut t = DropTracker::default();
        t.hovered();
        assert!(t.dropped("C:\\a.txt".into(), Some((900.0, 100.0))));
        assert!(!t.dropped("C:\\b".into(), Some((900.0, 100.0))));
        let Some(Dropped::On(b)) = t.take(&side_by_side()) else {
            panic!("a drop on the cloud pane")
        };
        assert_eq!((b.side, b.row), (PaneSide::Remote, None));
        assert_eq!(
            b.paths,
            vec![PathBuf::from("C:\\a.txt"), PathBuf::from("C:\\b")]
        );
        assert!(t.take(&side_by_side()).is_none());
    }

    #[test]
    fn cancel_discards_files_not_yet_taken() {
        let mut t = DropTracker::default();
        assert!(t.dropped("C:\\old".into(), Some((900.0, 100.0))));
        t.cancelled();
        assert!(t.dropped("C:\\new".into(), Some((100.0, 100.0))));
        let Some(Dropped::On(b)) = t.take(&side_by_side()) else {
            panic!("a drop on This PC")
        };
        assert_eq!(b.side, PaneSide::Local);
        assert_eq!(b.paths, vec![PathBuf::from("C:\\new")]);
    }

    #[test]
    fn a_batch_lands_where_its_first_file_was_dropped() {
        let mut t = DropTracker::default();
        assert!(t.dropped("C:\\a".into(), Some((100.0, 100.0))));
        assert!(!t.dropped("C:\\b".into(), Some((900.0, 100.0))));
        let Some(Dropped::On(b)) = t.take(&side_by_side()) else {
            panic!("a drop on This PC")
        };
        assert_eq!(b.side, PaneSide::Local);
    }

    #[test]
    fn nothing_dropped_takes_nothing() {
        let mut t = DropTracker::default();
        t.hovered();
        assert!(t.take(&side_by_side()).is_none());
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
        t.dropped("C:\\a".into(), Some((0.0, 0.0)));
        assert!(!t.is_hovering());
    }

    #[test]
    fn a_drop_in_stacked_panes_is_placed_by_the_rectangles() {
        let mut t = DropTracker::default();
        t.dropped("C:\\a".into(), Some((100.0, 750.0)));
        let Some(Dropped::On(b)) = t.take(&stacked()) else {
            panic!("a drop on the lower pane")
        };
        assert_eq!(b.side, PaneSide::Remote);
        t.dropped("C:\\b".into(), Some((100.0, 100.0)));
        let Some(Dropped::On(b)) = t.take(&stacked()) else {
            panic!("a drop on the upper pane")
        };
        assert_eq!(b.side, PaneSide::Local);
    }

    #[test]
    fn a_drop_over_no_pane_or_nowhere_known_is_not_placed() {
        let mut t = DropTracker::default();
        // The toolbar.
        t.dropped("C:\\a".into(), Some((900.0, 20.0)));
        assert_eq!(t.take(&side_by_side()), Some(Dropped::Nowhere));
        // Position unknown.
        t.dropped("C:\\b".into(), None);
        assert_eq!(t.take(&side_by_side()), Some(Dropped::Unlocated));
    }

    #[test]
    fn the_default_layout_has_no_panes() {
        assert_eq!(hit((5000.0, 5000.0), &PaneLayout::default()), None);
        assert_eq!(hit((0.0, 0.0), &PaneLayout::default()), None);
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
        assert_eq!(hit((x, y), &side_by_side()), None);
        // A window on a monitor left of the primary one has a negative origin.
        let geometry = Geometry {
            origin: (-1800, -20),
            scale: 1.0,
        };
        assert_eq!(to_logical((-1700, 80), geometry), (100.0, 100.0));
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
        hovers: Rc<RefCell<Vec<Option<Target>>>>,
        drops: Rc<RefCell<Vec<DropBatch>>>,
        unlocated: Rc<Cell<usize>>,
    }

    /// The window's content is at the screen's origin. The cursor starts over This PC's header
    /// (x = 100); the cloud pane starts at x = 640.
    fn bed() -> Bed {
        bed_with_geometry(Some(Geometry {
            origin: (0, 0),
            scale: 1.0,
        }))
    }

    fn bed_with_geometry(geometry: Option<Geometry>) -> Bed {
        init_no_event_loop();
        let cursor = Rc::new(Cell::new((100, 100)));
        let hovers = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(RefCell::new(Vec::new()));
        let unlocated = Rc::new(Cell::new(0));
        let hook = Hook::new(
            Box::new({
                let hovers = hovers.clone();
                move |target| hovers.borrow_mut().push(target)
            }),
            Box::new({
                let drops = drops.clone();
                move |batch| drops.borrow_mut().push(batch)
            }),
            Box::new({
                let unlocated = unlocated.clone();
                move || unlocated.set(unlocated.get() + 1)
            }),
            Box::new(side_by_side),
            Box::new(move |_| geometry),
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
            unlocated,
        }
    }

    /// Lets `ms` milliseconds of the test's clock go by.
    fn pass(ms: u128) {
        mock_elapsed_time(Duration::from_millis(ms as u64));
    }

    const POLL_MS: u128 = HOVER_POLL.as_millis();

    const OVER_LOCAL: Target = (PaneSide::Local, None);
    const OVER_CLOUD: Target = (PaneSide::Remote, None);

    #[test]
    fn hovering_looks_up_the_target_every_poll_and_reports_only_a_change() {
        let b = bed();
        b.hook.start_hover(b.window.window());
        assert_eq!(*b.hovers.borrow(), [Some(OVER_LOCAL)]);

        // The cursor moves into the cloud pane, but nobody looks until the next poll.
        b.cursor.set((900, 100));
        pass(POLL_MS - 1);
        assert_eq!(*b.hovers.borrow(), [Some(OVER_LOCAL)]);
        pass(1);
        assert_eq!(*b.hovers.borrow(), [Some(OVER_LOCAL), Some(OVER_CLOUD)]);

        // Polls that find the same answer say nothing.
        pass(4 * POLL_MS);
        assert_eq!(b.hovers.borrow().len(), 2);

        // Row 0 of the cloud pane starts at y = 123.
        b.cursor.set((900, 130));
        pass(POLL_MS);
        assert_eq!(b.hovers.borrow().len(), 3);
        assert_eq!(b.hovers.borrow()[2], Some((PaneSide::Remote, Some(0))));

        // The toolbar: over no pane.
        b.cursor.set((900, 20));
        pass(POLL_MS);
        assert_eq!(b.hovers.borrow().last(), Some(&None));
    }

    #[test]
    fn the_polling_stops_when_the_files_stop_hovering() {
        let b = bed();
        b.hook.start_hover(b.window.window());
        b.cursor.set((900, 100));
        pass(POLL_MS);
        assert_eq!(b.hovers.borrow().len(), 2);

        // They left the window: the panes are told at once, once, and the timer is gone.
        b.hook.end_hover();
        b.hook.end_hover();
        assert_eq!(b.hovers.borrow().len(), 3);
        assert_eq!(b.hovers.borrow()[2], None);
        pass(10 * POLL_MS);
        assert_eq!(b.hovers.borrow().len(), 3);
    }

    #[test]
    fn the_files_of_a_drop_are_handed_over_once_after_the_last_of_them() {
        let b = bed();
        b.cursor.set((900, 130));
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
            assert_eq!(drops[0].row, Some(0));
            assert_eq!(
                drops[0].paths,
                [r"C:\a.txt", r"C:\b", r"C:\c"].map(PathBuf::from)
            );
        }
        pass(10 * POLL_MS);
        assert_eq!(b.drops.borrow().len(), 1, "nothing is handed over twice");

        // The next drop is a batch of its own, wherever it lands.
        b.cursor.set((100, 100));
        b.hook.drop_file(b.window.window(), Path::new(r"C:\d"));
        pass(0);
        let drops = b.drops.borrow();
        assert_eq!(drops.len(), 2);
        assert_eq!(drops[1].side, PaneSide::Local);
        assert_eq!(drops[1].paths, [PathBuf::from(r"C:\d")]);
    }

    #[test]
    fn a_drop_over_no_pane_is_ignored() {
        let b = bed();
        b.cursor.set((900, 20));
        b.hook.drop_file(b.window.window(), Path::new(r"C:\a"));
        pass(0);
        assert!(b.drops.borrow().is_empty());
        assert_eq!(b.unlocated.get(), 0);
    }

    #[test]
    fn with_no_window_position_a_drop_is_ignored_and_reported() {
        let b = bed_with_geometry(None);
        b.hook.start_hover(b.window.window());
        assert!(b.hovers.borrow().is_empty());
        b.hook.drop_file(b.window.window(), Path::new(r"C:\a"));
        pass(0);
        assert!(b.drops.borrow().is_empty());
        assert_eq!(b.unlocated.get(), 1);
    }

    #[test]
    fn a_drop_ends_the_hover() {
        let b = bed();
        b.hook.start_hover(b.window.window());
        b.cursor.set((900, 100));
        pass(POLL_MS);
        assert_eq!(b.hovers.borrow().len(), 2);

        b.hook.drop_file(b.window.window(), Path::new(r"C:\a"));
        assert_eq!(b.hovers.borrow().len(), 3);
        assert_eq!(b.hovers.borrow()[2], None);
        // The poll is stopped, so it cannot light the pane again.
        pass(10 * POLL_MS);
        assert_eq!(b.hovers.borrow().len(), 3);
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
