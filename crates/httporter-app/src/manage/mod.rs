//! Manage+ (I4): the inspector, Quick Look, editing with re-upload, the sidebar's Favorites and
//! Editing, the command palette, search, tabs, rename / move / presign, the activity log and the
//! Settings tabs General, Sites and Advanced.
//!
//! The parts, one file each (each task owns its own; see the I4 plan, §0.4):
//! - this file (T1): the [`Manager`], shared by the window's callbacks and the tasks they start;
//! - [`cache`] (T4): fetching a site's file into the local cache;
//! - [`inspector_view`], [`thumbs`], [`links`] (T5);
//! - [`quicklook_view`], [`highlight`], [`edit`], [`places`] (T6);
//! - [`palette_view`], [`search_view`], [`tabs`] (T7);
//! - [`settings_tabs`] (T8);
//! - [`activity_view`], [`move_view`] (T9);
//! - [`copy`]: the words of all of the above, one section per task.
//!
//! The window owns the manager (through its callbacks); [`manager`] finds it on the UI thread.

pub mod activity_view;
mod cache;
mod copy;
mod edit;
mod highlight;
mod inspector_view;
mod links;
mod move_view;
mod palette_view;
pub(crate) mod places;
mod quicklook_view;
mod search_view;
pub mod settings_tabs;
pub mod tabs;
mod thumbs;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use httporter_core::{Entry, SiteId, SiteRecord};

use crate::AppWindow;
use crate::binding::Link;
use crate::publish::Publisher;
use crate::session::Session;
use crate::transfers::Transfers;

/// A pane of the main window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Local,
    Remote,
}

/// Manage+. Shared as `Arc<Manager>` by the window's callbacks and the tasks they start. Each
/// view keeps its own state here, in its own file's type; the views read the fields.
#[allow(dead_code)] // T4–T9 read the fields.
pub struct Manager {
    // Crate-wide for the I5 modules (`window`, `comfort`, `system`).
    pub(crate) session: Arc<Session>,
    pub(crate) transfers: Arc<Transfers>,
    pub(crate) publisher: Arc<Publisher>,
    pub(crate) link: Link,
    inspector: Mutex<inspector_view::InspectorState>,
    quicklook: Mutex<quicklook_view::QuickLookState>,
    edits: Mutex<edit::Edits>,
    palette: Mutex<palette_view::PaletteState>,
    tabs: Mutex<tabs::TabsState>,
    search: Mutex<search_view::SearchState>,
    moves: Mutex<move_view::MoveState>,
    activity: Mutex<activity_view::ActivityState>,
    settings: Mutex<settings_tabs::SettingsState>,
}

impl Manager {
    pub fn new(
        session: Arc<Session>,
        transfers: Arc<Transfers>,
        publisher: Arc<Publisher>,
        link: Link,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            transfers,
            publisher,
            link,
            inspector: Mutex::default(),
            quicklook: Mutex::default(),
            edits: Mutex::default(),
            palette: Mutex::default(),
            tabs: Mutex::default(),
            search: Mutex::default(),
            moves: Mutex::default(),
            activity: Mutex::default(),
            settings: Mutex::default(),
        })
    }

    /// Whether `site` is read-only: every write to it is off (T8).
    pub fn read_only(&self, site: &SiteId) -> bool {
        settings_tabs::read_only(self, site)
    }

    /// Enter, a double click or the menu's Open on a file row of `side` (`row` into its rows):
    /// a local file opens in its program, a site's file is edited with re-upload (T6).
    pub fn open_file(self: &Arc<Self>, side: Side, row: usize) {
        edit::open_file(self, side, row);
    }

    /// The window is closing: the re-uploads still pending go now (T6).
    pub fn flush_edits(&self) {
        edit::flush_edits(self);
    }

    /// Downloads `entry` of `site` straight into `cache_dir(kind)/<site>/<path>` (no engine row),
    /// checking the listed checksum; over 50 MB asks unless `ask_big_preview` is off; reuses a
    /// cached copy with the same size and checksum (T4).
    #[allow(dead_code)] // T5 and T6 call it.
    pub async fn fetch_to_cache(
        &self,
        site: &SiteRecord,
        entry: &Entry,
        kind: &str,
    ) -> Result<PathBuf, String> {
        cache::fetch_to_cache(self, site, entry, kind).await
    }
}

thread_local! {
    /// The manager of the window on this thread (the UI thread), set by [`install`]. Weak: the
    /// window owns the manager, and a closed window lets it go.
    static MANAGER: RefCell<Weak<Manager>> = const { RefCell::new(Weak::new()) };
}

/// The manager [`install`] set on this thread (the UI thread), while its window lives.
pub fn manager() -> Option<Arc<Manager>> {
    MANAGER.with(|m| m.borrow().upgrade())
}

/// Wires every Manage+ view of `ui` to `m`, and makes `m` the thread's [`manager`].
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    MANAGER.with(|slot| *slot.borrow_mut() = Arc::downgrade(m));
    cache::install(ui, m);
    inspector_view::install(ui, m);
    thumbs::install(ui, m);
    links::install(ui, m);
    quicklook_view::install(ui, m);
    edit::install(ui, m);
    places::install(ui, m);
    palette_view::install(ui, m);
    search_view::install(ui, m);
    tabs::install(ui, m);
    settings_tabs::install(ui, m);
    activity_view::install(ui, m);
    move_view::install(ui, m);
}
