// SPDX-License-Identifier: MIT OR Apache-2.0

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

    /// The number the window uses for the pane (0 This PC, 1 the cloud pane).
    pub fn index(self) -> i32 {
        match self {
            Self::ThisPc => 0,
            Self::Cloud => 1,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewmodel::{FileKindData, RowData};

    fn picked(s: &Selection) -> Vec<usize> {
        s.rows()
    }

    // ---- Click, Ctrl+click, Shift+click, the rubber band, Ctrl+A ----

    #[test]
    fn selection_click_ctrl_shift_band_all() {
        let mut s = Selection::default();
        assert!(s.rows().is_empty() && s.focus().is_none());

        // A click selects that row alone, and starts a range from it.
        s.click(3);
        assert_eq!((picked(&s), s.focus()), (vec![3], Some(3)));
        s.click(5);
        assert_eq!((picked(&s), s.focus()), (vec![5], Some(5)));

        // Shift extends from the anchor, to either side, and replaces the range before it.
        s.shift_click(8);
        assert_eq!((picked(&s), s.focus()), (vec![5, 6, 7, 8], Some(8)));
        s.shift_click(2);
        assert_eq!(picked(&s), vec![2, 3, 4, 5], "the anchor (5) stays");
        assert_eq!(s.focus(), Some(2));
        s.shift_click(5);
        assert_eq!(picked(&s), vec![5]);

        // Ctrl toggles one row and moves the anchor to it.
        s.ctrl_click(9);
        assert_eq!((picked(&s), s.focus()), (vec![5, 9], Some(9)));
        s.ctrl_click(5);
        assert_eq!(picked(&s), vec![9], "a second Ctrl+click takes the row out");
        s.ctrl_click(5);
        assert_eq!(picked(&s), vec![5, 9]);
        // The anchor is the last Ctrl+clicked row, so a Shift+click ranges from there and drops
        // the rest (Explorer's rule).
        s.shift_click(7);
        assert_eq!(picked(&s), vec![5, 6, 7]);

        // Ctrl+Shift adds the range to what is selected.
        s.click(0);
        s.ctrl_click(4);
        s.ctrl_shift_click(6);
        assert_eq!(picked(&s), vec![0, 4, 5, 6]);
        assert_eq!(s.focus(), Some(6));
        s.ctrl_shift_click(2);
        assert_eq!(
            picked(&s),
            vec![0, 2, 3, 4, 5, 6],
            "from the anchor (4) down to 2"
        );

        // Shift with nothing clicked yet selects just that row, which becomes the anchor.
        let mut fresh = Selection::default();
        fresh.shift_click(4);
        assert_eq!(picked(&fresh), vec![4]);
        fresh.shift_click(6);
        assert_eq!(picked(&fresh), vec![4, 5, 6]);
        let mut fresh = Selection::default();
        fresh.ctrl_shift_click(2);
        assert_eq!(picked(&fresh), vec![2]);

        // The rubber band: rows from..=to, either way round, replacing the selection …
        s.click(1);
        s.band(3, 6, false);
        assert_eq!(picked(&s), vec![3, 4, 5, 6]);
        s.band(8, 7, false);
        assert_eq!(picked(&s), vec![7, 8]);
        // … or, with Ctrl, joining it.
        s.band(0, 1, true);
        assert_eq!(picked(&s), vec![0, 1, 7, 8]);
        // A band's first row is where a Shift range starts.
        s.shift_click(3);
        assert_eq!(picked(&s), vec![0, 1, 2, 3]);

        // Ctrl+A: every row of the pane. The focus stays where it was.
        s.click(4);
        s.select_all(6);
        assert_eq!(picked(&s), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(s.focus(), Some(4));
        assert_eq!(s.count(), 6);
        s.select_all(0);
        assert!(
            s.rows().is_empty() && s.focus().is_none(),
            "an empty pane has none"
        );

        s.click(2);
        s.clear();
        assert_eq!(s, Selection::default());
    }

    #[test]
    fn the_window_sends_modifiers_as_one_call() {
        let mut s = Selection::default();
        s.pointer(2, false, false);
        s.pointer(5, true, false);
        assert_eq!(picked(&s), vec![2, 5]);
        s.pointer(8, false, true);
        assert_eq!(picked(&s), vec![5, 6, 7, 8]);
        s.pointer(0, true, true);
        assert_eq!(picked(&s), vec![0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_restored_selection_starts_ranges_from_the_focus() {
        let mut s = Selection::default();
        s.restore([1, 4], Some(4));
        assert_eq!((picked(&s), s.focus()), (vec![1, 4], Some(4)));
        s.shift_click(6);
        assert_eq!(picked(&s), vec![4, 5, 6]);
        s.restore([], None);
        assert!(s.rows().is_empty() && s.focus().is_none());
    }

    // ---- The keyboard ----

    #[test]
    fn move_focus_extends_with_shift() {
        let mut s = Selection::default();
        // From no focus a step down lands on the first row and a step up on the last.
        s.move_focus(1, false, 5);
        assert_eq!((picked(&s), s.focus()), (vec![0], Some(0)));
        let mut up = Selection::default();
        up.move_focus(-1, false, 5);
        assert_eq!((picked(&up), up.focus()), (vec![4], Some(4)));

        // Without Shift the selection follows the focus.
        s.move_focus(1, false, 5);
        s.move_focus(1, false, 5);
        assert_eq!((picked(&s), s.focus()), (vec![2], Some(2)));

        // With Shift the range from the anchor (2) to the focus is selected.
        s.move_focus(1, true, 5);
        s.move_focus(1, true, 5);
        assert_eq!((picked(&s), s.focus()), (vec![2, 3, 4], Some(4)));
        s.move_focus(-1, true, 5);
        assert_eq!(picked(&s), vec![2, 3]);
        s.move_focus(-1, true, 5);
        s.move_focus(-1, true, 5);
        assert_eq!(picked(&s), vec![1, 2], "past the anchor the range flips");
        assert_eq!(s.focus(), Some(1));

        // A plain step collapses the range onto the new focus.
        s.move_focus(1, false, 5);
        assert_eq!((picked(&s), s.focus()), (vec![2], Some(2)));
    }

    #[test]
    fn the_focus_stops_at_the_ends_and_a_page_is_a_bigger_step() {
        let mut s = Selection::default();
        s.move_focus(100, false, 5);
        assert_eq!(s.focus(), Some(4), "past the end stops on the last row");
        s.move_focus(1, false, 5);
        assert_eq!(s.focus(), Some(4));
        s.move_focus(-100, true, 5);
        assert_eq!((picked(&s), s.focus()), (vec![0, 1, 2, 3, 4], Some(0)));
        s.move_focus(-1, false, 5);
        assert_eq!((picked(&s), s.focus()), (vec![0], Some(0)));
        // From nothing, a page down counts from before the first row.
        let mut fresh = Selection::default();
        fresh.move_focus(10, false, 30);
        assert_eq!(fresh.focus(), Some(9));
        // An empty pane has nowhere to go.
        let mut empty = Selection::default();
        empty.move_focus(1, true, 0);
        empty.move_focus(-1, false, 0);
        assert_eq!(empty, Selection::default());
    }

    // ---- The status bar's words ----

    fn row(name: &str, folder: bool, bytes: u64) -> RowData {
        RowData {
            name: name.into(),
            kind: if folder {
                FileKindData::Folder
            } else {
                FileKindData::Doc
            },
            size: String::new(),
            modified: String::new(),
            folder,
            bytes,
        }
    }

    fn summary(rows: &[RowData]) -> String {
        copy::selection_summary(&rows.iter().collect::<Vec<_>>())
    }

    #[test]
    fn summary_texts() {
        // Nothing selected: empty, so the item count shows.
        assert_eq!(summary(&[]), "");
        // One file: its name and size; one folder: just its name.
        assert_eq!(
            summary(&[row("index.html", false, 392_641)]),
            "index.html selected · 383.4 KB"
        );
        assert_eq!(summary(&[row("uploads", true, 0)]), "uploads selected");
        assert_eq!(
            summary(&[row("empty.txt", false, 0)]),
            "empty.txt selected · 0 B"
        );
        // Several: the count, and the files' bytes added up.
        let twelve: Vec<RowData> = (0..12)
            .map(|i| row(&format!("f{i}.bin"), false, 367_001)) // 12 × 367,001 B = 4.2 MB
            .collect();
        assert_eq!(summary(&twelve), "12 items selected · 4.2 MB");
        assert_eq!(
            summary(&[row("a", false, 1024), row("b", false, 1024)]),
            "2 items selected · 2.0 KB"
        );
        // Folders count as items but add no bytes.
        assert_eq!(
            summary(&[row("docs", true, 0), row("a.txt", false, 2048)]),
            "2 items selected · 2.0 KB"
        );
        // Only folders: no size part.
        assert_eq!(
            summary(&[row("a", true, 0), row("b", true, 0), row("c", true, 0)]),
            "3 items selected"
        );
        assert_eq!(
            summary(&[row("a", true, 0), row("b", true, 0)]),
            "2 items selected"
        );
    }

    // ---- The toolbar ----

    fn state(change: impl FnOnce(&mut ToolbarInputs), active: ActivePane) -> ToolbarState {
        let mut inputs = ToolbarInputs {
            active,
            local_selected: 0,
            remote_selected: 0,
            site_open: true,
            read_only: false,
            at_drive_list: false,
        };
        change(&mut inputs);
        toolbar_state(inputs)
    }

    #[test]
    fn upload_needs_an_open_writable_site_not_a_selection() {
        for active in [ActivePane::Cloud, ActivePane::ThisPc] {
            // With nothing selected in This PC it asks for files; with something it uploads that.
            assert!(state(|_| {}, active).can_upload);
            assert!(state(|i| i.local_selected = 3, active).can_upload);
            assert!(!state(|i| i.site_open = false, active).can_upload);
            assert!(!state(|i| i.read_only = true, active).can_upload);
        }
    }

    #[test]
    fn download_needs_a_cloud_selection() {
        for active in [ActivePane::Cloud, ActivePane::ThisPc] {
            assert!(!state(|_| {}, active).can_download, "nothing selected");
            assert!(!state(|i| i.local_selected = 4, active).can_download);
            assert!(state(|i| i.remote_selected = 1, active).can_download);
            let no_site = |i: &mut ToolbarInputs| {
                i.remote_selected = 1;
                i.site_open = false;
            };
            assert!(!state(no_site, active).can_download);
            // Reading from a read-only site is fine, and so is the list of drives (the
            // destination is asked for).
            let read_only = |i: &mut ToolbarInputs| {
                i.remote_selected = 2;
                i.read_only = true;
                i.at_drive_list = true;
            };
            assert!(state(read_only, active).can_download);
        }
    }

    #[test]
    fn new_folder_acts_on_the_active_pane() {
        let cloud = ActivePane::Cloud;
        assert!(state(|_| {}, cloud).can_new_folder);
        assert!(!state(|i| i.site_open = false, cloud).can_new_folder);
        assert!(!state(|i| i.read_only = true, cloud).can_new_folder);
        // The list of drives does not matter to the cloud pane.
        assert!(state(|i| i.at_drive_list = true, cloud).can_new_folder);

        let pc = ActivePane::ThisPc;
        assert!(state(|_| {}, pc).can_new_folder);
        // This PC needs a folder; a site, or its being read-only, is none of its business.
        assert!(!state(|i| i.at_drive_list = true, pc).can_new_folder);
        assert!(state(|i| i.site_open = false, pc).can_new_folder);
        assert!(state(|i| i.read_only = true, pc).can_new_folder);
    }

    #[test]
    fn delete_is_for_the_cloud_pane_and_says_why_not_on_this_pc() {
        let cloud = ActivePane::Cloud;
        let none = state(|_| {}, cloud);
        assert!(!none.can_delete, "nothing selected");
        assert_eq!(none.delete_note, "");
        let some = state(|i| i.remote_selected = 2, cloud);
        assert!(some.can_delete);
        assert_eq!(some.delete_note, "");
        let read_only = |i: &mut ToolbarInputs| {
            i.remote_selected = 2;
            i.read_only = true;
        };
        assert!(!state(read_only, cloud).can_delete);
        let no_site = |i: &mut ToolbarInputs| {
            i.remote_selected = 2;
            i.site_open = false;
        };
        assert!(!state(no_site, cloud).can_delete);

        // With This PC active it is off, selection or not, and the note says to use Explorer.
        for selected in [0, 3] {
            let rows = |i: &mut ToolbarInputs| {
                i.remote_selected = selected;
                i.local_selected = selected;
            };
            let pc = state(rows, ActivePane::ThisPc);
            assert!(!pc.can_delete);
            assert_eq!(
                pc.delete_note,
                "Delete works in the cloud pane. Use Explorer for files on this PC."
            );
        }
    }

    #[test]
    fn nothing_is_on_without_a_site() {
        let no_site = |i: &mut ToolbarInputs| {
            i.site_open = false;
            i.remote_selected = 5;
        };
        assert_eq!(
            state(no_site, ActivePane::Cloud),
            ToolbarState {
                can_upload: false,
                can_download: false,
                can_new_folder: false,
                can_delete: false,
                delete_note: "",
            }
        );
    }

    #[test]
    fn the_panes_are_numbered_as_the_window_numbers_them() {
        assert_eq!(ActivePane::from_index(0), Some(ActivePane::ThisPc));
        assert_eq!(ActivePane::from_index(1), Some(ActivePane::Cloud));
        assert_eq!(ActivePane::from_index(-1), None);
        assert_eq!(ActivePane::from_index(2), None);
        for pane in [ActivePane::ThisPc, ActivePane::Cloud] {
            assert_eq!(ActivePane::from_index(pane.index()), Some(pane));
        }
    }
}
