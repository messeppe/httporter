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
        }
    }
}

/// The settings key [`UiState`] is kept under.
pub const UI_STATE_KEY: &str = "ui_state";
