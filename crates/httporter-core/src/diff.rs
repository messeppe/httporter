//! A line diff of two texts (Myers), for Quick Look's "Changes vs Bunny" (T6).
#![allow(unused_variables, dead_code)]

/// What happened to one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    Same,
    Added,
    Removed,
}

/// One line of a diff, with its number on each side it is on (1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: DiffOp,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

/// Texts with more lines than this on either side are not diffed.
pub const DIFF_MAX_LINES: usize = 20_000;

/// The lines of `old` and `new`, in order, each marked same, added or removed (Myers' shortest
/// edit script). `None` when either text has more than [`DIFF_MAX_LINES`] lines.
pub fn diff_lines(old: &str, new: &str) -> Option<Vec<DiffLine>> {
    todo!("T6")
}
