// SPDX-License-Identifier: MIT OR Apache-2.0

//! A line diff of two texts (Myers), for Quick Look's "Changes vs Bunny".
//!
//! The linear-space variant: each range is split where the shortest edit paths from its two ends
//! meet, and the halves are diffed in turn, so memory stays proportional to the texts. Common
//! leading and trailing lines are taken off first, which is most of a typical edit. A diff that
//! takes too long (texts that share almost nothing) gives up on the ranges left and shows each as
//! removed, then added.

use std::time::{Duration, Instant};

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

/// How long a diff may search before the ranges left are shown as replaced.
const BUDGET: Duration = Duration::from_secs(2);

/// The lines of `old` and `new`, in order, each marked same, added or removed (Myers' shortest
/// edit script). `None` when either text has more than [`DIFF_MAX_LINES`] lines.
pub fn diff_lines(old: &str, new: &str) -> Option<Vec<DiffLine>> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() > DIFF_MAX_LINES || b.len() > DIFF_MAX_LINES {
        return None;
    }
    let mut differ = Differ {
        a: &a,
        b: &b,
        deadline: Instant::now() + BUDGET,
        ops: Vec::with_capacity(a.len().max(b.len())),
    };
    differ.conquer(0, a.len(), 0, b.len());
    Some(number(&a, &b, &differ.ops))
}

/// The edit script as lines with their numbers.
fn number(a: &[&str], b: &[&str], ops: &[DiffOp]) -> Vec<DiffLine> {
    let (mut i, mut j) = (0usize, 0usize);
    let no = |n: usize| u32::try_from(n + 1).ok();
    let mut out = Vec::with_capacity(ops.len());
    for &op in ops {
        let line = match op {
            DiffOp::Same => {
                let line = DiffLine {
                    op,
                    old_no: no(i),
                    new_no: no(j),
                    text: b[j].to_owned(),
                };
                i += 1;
                j += 1;
                line
            }
            DiffOp::Removed => {
                let line = DiffLine {
                    op,
                    old_no: no(i),
                    new_no: None,
                    text: a[i].to_owned(),
                };
                i += 1;
                line
            }
            DiffOp::Added => {
                let line = DiffLine {
                    op,
                    old_no: None,
                    new_no: no(j),
                    text: b[j].to_owned(),
                };
                j += 1;
                line
            }
        };
        out.push(line);
    }
    out
}

struct Differ<'a> {
    a: &'a [&'a str],
    b: &'a [&'a str],
    deadline: Instant,
    ops: Vec<DiffOp>,
}

impl Differ<'_> {
    fn push(&mut self, op: DiffOp, n: usize) {
        self.ops.extend(std::iter::repeat_n(op, n));
    }

    /// Diffs `a[a_lo..a_hi]` against `b[b_lo..b_hi]` into `ops`.
    fn conquer(&mut self, mut a_lo: usize, mut a_hi: usize, mut b_lo: usize, mut b_hi: usize) {
        let mut prefix = 0;
        while a_lo < a_hi && b_lo < b_hi && self.a[a_lo] == self.b[b_lo] {
            a_lo += 1;
            b_lo += 1;
            prefix += 1;
        }
        self.push(DiffOp::Same, prefix);
        let mut suffix = 0;
        while a_hi > a_lo && b_hi > b_lo && self.a[a_hi - 1] == self.b[b_hi - 1] {
            a_hi -= 1;
            b_hi -= 1;
            suffix += 1;
        }
        if a_lo == a_hi {
            self.push(DiffOp::Added, b_hi - b_lo);
        } else if b_lo == b_hi {
            self.push(DiffOp::Removed, a_hi - a_lo);
        } else if let Some((x, y)) = self.split(a_lo, a_hi, b_lo, b_hi) {
            self.conquer(a_lo, x, b_lo, y);
            self.conquer(x, a_hi, y, b_hi);
        } else {
            self.push(DiffOp::Removed, a_hi - a_lo);
            self.push(DiffOp::Added, b_hi - b_lo);
        }
        self.push(DiffOp::Same, suffix);
    }

    /// A point on a shortest edit path of the two ranges, where the paths searched from both
    /// ends meet, as indexes into `a` and `b`; `None` when the search ran out of time. Both
    /// ranges are non-empty and differ in their first and last lines.
    fn split(&self, a_lo: usize, a_hi: usize, b_lo: usize, b_hi: usize) -> Option<(usize, usize)> {
        let (a, b) = (&self.a[a_lo..a_hi], &self.b[b_lo..b_hi]);
        let (n, m) = (a.len() as isize, b.len() as isize);
        let max_d = (n + m + 1) / 2;
        let offset = max_d;
        let len = 2 * max_d + 2;
        let mut v1 = vec![-1isize; len as usize];
        let mut v2 = vec![-1isize; len as usize];
        v1[(offset + 1) as usize] = 0;
        v2[(offset + 1) as usize] = 0;
        let delta = n - m;
        // With an odd delta the forward search finds the meeting point, else the backward one.
        let front = delta % 2 != 0;
        let (mut k1_start, mut k1_end, mut k2_start, mut k2_end) = (0, 0, 0, 0);
        let at = |k: isize| (offset + k) as usize;
        for d in 0..max_d {
            if d % 32 == 31 && Instant::now() > self.deadline {
                return None;
            }
            let mut k1 = -d + k1_start;
            while k1 <= d - k1_end {
                let mut x1 = if k1 == -d || (k1 != d && v1[at(k1 - 1)] < v1[at(k1 + 1)]) {
                    v1[at(k1 + 1)]
                } else {
                    v1[at(k1 - 1)] + 1
                };
                let mut y1 = x1 - k1;
                while x1 < n && y1 < m && a[x1 as usize] == b[y1 as usize] {
                    x1 += 1;
                    y1 += 1;
                }
                v1[at(k1)] = x1;
                if x1 > n {
                    // Off the right edge.
                    k1_end += 2;
                } else if y1 > m {
                    // Off the bottom edge.
                    k1_start += 2;
                } else if front {
                    let k2 = offset + delta - k1;
                    if k2 >= 0 && k2 < len && v2[k2 as usize] != -1 && x1 >= n - v2[k2 as usize] {
                        return Some((a_lo + x1 as usize, b_lo + y1 as usize));
                    }
                }
                k1 += 2;
            }
            let mut k2 = -d + k2_start;
            while k2 <= d - k2_end {
                let mut x2 = if k2 == -d || (k2 != d && v2[at(k2 - 1)] < v2[at(k2 + 1)]) {
                    v2[at(k2 + 1)]
                } else {
                    v2[at(k2 - 1)] + 1
                };
                let mut y2 = x2 - k2;
                while x2 < n && y2 < m && a[(n - x2 - 1) as usize] == b[(m - y2 - 1) as usize] {
                    x2 += 1;
                    y2 += 1;
                }
                v2[at(k2)] = x2;
                if x2 > n {
                    k2_end += 2;
                } else if y2 > m {
                    k2_start += 2;
                } else if !front {
                    let k1 = offset + delta - k2;
                    if k1 >= 0 && k1 < len && v1[k1 as usize] != -1 {
                        let x1 = v1[k1 as usize];
                        let y1 = offset + x1 - k1;
                        if x1 >= n - x2 {
                            return Some((a_lo + x1 as usize, b_lo + y1 as usize));
                        }
                    }
                }
                k2 += 2;
            }
        }
        None
    }
}
