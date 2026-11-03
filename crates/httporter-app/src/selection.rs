//! Which rows of a pane are selected, with the Explorer and Finder rules for clicks, and what the
//! toolbar offers for the selection.
//!
//! [`Selection`] is plain data over row indexes, so every rule is a function of what was pressed.
//! The binding keeps one per pane and writes the result back into the rows' `selected` flags.
//! [`toolbar_state`] is the other half: given the selections and where the app stands, which of
//! Upload, Download, New folder and Delete may be pressed (Ruling Q13).

use std::collections::BTreeSet;

use crate::copy;

/// The selected rows of one pane, the row the keyboard is on (the focus) and the row a Shift range
/// starts from (the anchor).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    rows: BTreeSet<usize>,
    anchor: Option<usize>,
    focus: Option<usize>,
}

impl Selection {
    /// A plain click: only row `i` is selected, and it is where a Shift range starts.
    pub fn click(&mut self, i: usize) {
        self.rows.clear();
        self.rows.insert(i);
        self.anchor = Some(i);
        self.focus = Some(i);
    }

    /// Ctrl+click: row `i` joins the selection or leaves it, and becomes the anchor.
    pub fn ctrl_click(&mut self, i: usize) {
        if !self.rows.remove(&i) {
            self.rows.insert(i);
        }
        self.anchor = Some(i);
        self.focus = Some(i);
    }

    /// Shift+click: the rows from the anchor to `i` are the selection (what was selected before
    /// is dropped). The anchor stays; with none yet, `i` is it.
    pub fn shift_click(&mut self, i: usize) {
        let anchor = *self.anchor.get_or_insert(i);
        self.rows = range(anchor, i).collect();
        self.focus = Some(i);
    }

    /// Ctrl+Shift+click: the rows from the anchor to `i` are added to the selection.
    pub fn ctrl_shift_click(&mut self, i: usize) {
        let anchor = *self.anchor.get_or_insert(i);
        self.rows.extend(range(anchor, i));
        self.focus = Some(i);
    }

    /// A click on row `i` with the modifiers that were held.
    pub fn pointer(&mut self, i: usize, ctrl: bool, shift: bool) {
        match (ctrl, shift) {
            (false, false) => self.click(i),
            (true, false) => self.ctrl_click(i),
            (false, true) => self.shift_click(i),
            (true, true) => self.ctrl_shift_click(i),
        }
    }

    /// Ctrl+A in a pane of `n` rows. The focus and the anchor stay where they were.
    pub fn select_all(&mut self, n: usize) {
        self.rows = (0..n).collect();
        if n == 0 {
            self.anchor = None;
            self.focus = None;
        }
    }

    /// The rubber band covers rows `from` to `to` (either way round). Without `additive` (Ctrl not
    /// held) they are the selection; with it they are added to it.
    pub fn band(&mut self, from: usize, to: usize, additive: bool) {
        if !additive {
            self.rows.clear();
        }
        self.rows.extend(range(from, to));
        self.anchor = Some(from);
    }

    /// An arrow key, Page Up or Page Down in a pane of `n` rows: the focus moves by `delta` rows
    /// (stopping at the first and the last). With `extend` (Shift held) the range from the anchor
    /// to the new focus is selected; without, only the new focus is. From no focus, a step down
    /// lands on the first row and a step up on the last.
    // The pane's keys reach the binding as `row-pointer` calls with the row they land on (the
    // window has no callback for a bare step), so only tests take a delta so far.
    #[allow(dead_code)]
    pub fn move_focus(&mut self, delta: isize, extend: bool, n: usize) {
        if n == 0 {
            return;
        }
        let last = n as isize - 1;
        let from = self.focus.map_or_else(
            || if delta >= 0 { -1 } else { n as isize },
            |focus| focus as isize,
        );
        let target = (from + delta).clamp(0, last) as usize;
        self.pointer(target, false, extend);
    }

    /// Nothing selected, no focus.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The selection as it was in an earlier listing of the folder: `rows` selected, `focus` as
    /// the focus and the anchor.
    pub fn restore(&mut self, rows: impl IntoIterator<Item = usize>, focus: Option<usize>) {
        self.rows = rows.into_iter().collect();
        self.anchor = focus;
        self.focus = focus;
    }

    /// The selected rows, in order.
    pub fn rows(&self) -> Vec<usize> {
        self.rows.iter().copied().collect()
    }

    /// How many rows are selected.
    pub fn count(&self) -> usize {
        self.rows.len()
    }

    /// The row the keyboard is on.
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }
}

/// The rows from `a` to `b` inclusive, whichever comes first.
fn range(a: usize, b: usize) -> impl Iterator<Item = usize> {
    a.min(b)..=a.max(b)
}

/// The two file panes, as the window numbers them in its callbacks (0 and 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePane {
    ThisPc,
    Cloud,
}

impl ActivePane {
    /// The pane for the number the window sends (0 This PC, 1 the cloud pane).
    pub fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::ThisPc),
            1 => Some(Self::Cloud),
            _ => None,
        }
    }
}

/// What [`toolbar_state`] needs to know.
#[derive(Clone, Copy, Debug)]
pub struct ToolbarInputs {
    /// The pane last used or focused.
    pub active: ActivePane,
    /// How many rows are selected in This PC. No rule reads it yet: Upload works with none (it
    /// then asks for files) and with some alike.
    #[allow(dead_code)]
    pub local_selected: usize,
    /// How many rows are selected in the cloud pane.
    pub remote_selected: usize,
    /// A site is open: chosen, connected and listed.
    pub site_open: bool,
    /// The open site only allows reading.
    pub read_only: bool,
    /// This PC shows the list of drives, not a folder.
    pub at_drive_list: bool,
}

/// Which toolbar buttons may be pressed, and what to say about Delete when it cannot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolbarState {
    pub can_upload: bool,
    pub can_download: bool,
    pub can_new_folder: bool,
    pub can_delete: bool,
    /// Why Delete is off, when that is a matter of which pane is active; empty otherwise.
    pub delete_note: &'static str,
}

/// The toolbar's rules (Ruling Q13):
///
/// - Upload: a site is open and not read-only. It uploads the This PC selection, or asks for
///   files when nothing is selected there.
/// - Download: a site is open and rows are selected in the cloud pane. At the list of drives the
///   destination is asked for.
/// - New folder: acts on the active pane: the cloud pane needs an open site that is not
///   read-only; This PC needs a folder (the list of drives has none to put one in).
/// - Delete: the cloud pane only, with rows selected, on a site that is not read-only. With This
///   PC active it is off, and says to use Explorer.
pub fn toolbar_state(inputs: ToolbarInputs) -> ToolbarState {
    let writable = inputs.site_open && !inputs.read_only;
    let cloud = inputs.active == ActivePane::Cloud;
    ToolbarState {
        can_upload: writable,
        can_download: inputs.site_open && inputs.remote_selected > 0,
        can_new_folder: if cloud {
            writable
        } else {
            !inputs.at_drive_list
        },
        can_delete: cloud && writable && inputs.remote_selected > 0,
        delete_note: if cloud { "" } else { copy::DELETE_ON_THIS_PC },
    }
}
