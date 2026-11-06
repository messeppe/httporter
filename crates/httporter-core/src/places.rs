//! Places the user keeps: favorites, the window's tabs and how the window was left.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{RemotePath, SiteId};

/// Where a favorite or a place is: on this PC or on a site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceKind {
    Local,
    Remote,
}

/// A folder in the sidebar's Favorites: a local folder (`local`) or a folder of a site (`site`
/// and `remote`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Favorite {
    /// 0 until stored.
    pub id: i64,
    pub kind: PlaceKind,
    pub name: String,
    pub local: Option<PathBuf>,
    pub site: Option<SiteId>,
    pub remote: Option<RemotePath>,
}

/// One tab of the main window: the site and the folder each pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabState {
    pub site: Option<SiteId>,
    pub remote: Option<RemotePath>,
    pub local: Option<PathBuf>,
}

/// The main window's layout: two panes, or one pane with the inspector.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    TwoPanes,
    Inspector,
}

/// Where the main window was left: its normal (restored) outer rectangle in physical pixels, and
/// whether it was maximized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

/// The widths of the list's Size, Modified and Status columns, in logical pixels; 0 is the
/// column's default width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnWidths {
    pub size: u16,
    pub modified: u16,
    pub status: u16,
}

/// How the window was left and the General settings, kept as JSON under [`UI_STATE_KEY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub tabs: Vec<TabState>,
    pub current: usize,
    pub sync: bool,
    pub view: ViewMode,
    pub grid: bool,
    /// 0 system, 1 light, 2 dark.
    pub appearance: u8,
    /// On by default.
    pub restore_tabs: bool,
    /// On by default.
    pub ask_big_preview: bool,
    /// None until the window has been closed once.
    pub window: Option<WindowPlacement>,
    pub columns: ColumnWidths,
    /// "Keep running in the tray when the window is closed during transfers": on by default.
    pub tray: bool,
    /// The update check at start: off until the user turns it on.
    pub check_updates: bool,
    /// When the release feed was last read, in Unix seconds; 0 is never.
    pub last_update_check: i64,
    /// The renderer (Settings › Advanced), applied at the next start: 0 software (the default),
    /// 1 GPU.
    pub renderer: u8,
    /// The GPU renderer failed when it was last used (an error, or a crash or hang before its
    /// first frame): the renderer went back to software and Settings › Advanced says why. It is
    /// also set while a start with the GPU renderer has drawn nothing yet, so that a start that
    /// crashes or hangs there is taken as one that failed. Choosing a renderer clears it.
    pub gpu_failed: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            current: 0,
            sync: false,
            view: ViewMode::TwoPanes,
            grid: false,
            appearance: 0,
            restore_tabs: true,
            ask_big_preview: true,
            window: None,
            columns: ColumnWidths::default(),
            tray: true,
            check_updates: false,
            last_update_check: 0,
            renderer: 0,
            gpu_failed: false,
        }
    }
}

/// The settings key [`UiState`] is kept under.
pub const UI_STATE_KEY: &str = "ui_state";
