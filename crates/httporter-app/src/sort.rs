// SPDX-License-Identifier: MIT OR Apache-2.0

//! Folders-first sorting shared by the local and cloud file panes.

use chrono::NaiveDateTime;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

/// A row that can be ordered by [`sort_rows`].
pub trait Sortable {
    fn is_folder(&self) -> bool;
    fn name(&self) -> &str;
    fn size(&self) -> u64;
    fn modified(&self) -> Option<NaiveDateTime>;
}

/// Sorts `rows` by `key`. Folders always come first, in either direction; names compare
/// case-insensitively.
///
/// Rows that tie on `key` (every folder when sorting by size, say) are ordered by name, ascending,
/// so the result does not depend on the input order. An unknown modified time sorts before any
/// known one when ascending.
pub fn sort_rows<T: Sortable>(rows: &mut [T], key: SortKey, ascending: bool) {
    rows.sort_by(|a, b| {
        b.is_folder().cmp(&a.is_folder()).then_with(|| {
            let by_key = match key {
                SortKey::Name => compare_names(a.name(), b.name()),
                SortKey::Size => a.size().cmp(&b.size()),
                SortKey::Modified => a.modified().cmp(&b.modified()),
            };
            let by_key = if ascending { by_key } else { by_key.reverse() };
            by_key.then_with(|| compare_names(a.name(), b.name()))
        })
    });
}

/// Case-insensitive, with the exact spelling as the last resort so the order is total.
fn compare_names(a: &str, b: &str) -> Ordering {
    // ASCII fast path, with the same result as the general case below. Most file names are ASCII
    // and a listing of tens of thousands makes this the hottest line of a sort, so it skips the
    // Unicode case mapping for as long as both names stay ASCII.
    let (x, y) = (a.as_bytes(), b.as_bytes());
    for i in 0..x.len().min(y.len()) {
        let (p, q) = (x[i], y[i]);
        if !p.is_ascii() || !q.is_ascii() {
            return compare_names_unicode(a, b);
        }
        let (p, q) = (p.to_ascii_lowercase(), q.to_ascii_lowercase());
        if p != q {
            return p.cmp(&q);
        }
    }
    // One name's lowercase form is a prefix of the other's, so the shorter comes first.
    x.len().cmp(&y.len()).then_with(|| a.cmp(b))
}

fn compare_names_unicode(a: &str, b: &str) -> Ordering {
    fn lower(s: &str) -> impl Iterator<Item = char> + '_ {
        s.chars().flat_map(char::to_lowercase)
    }
    lower(a).cmp(lower(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Row {
        name: &'static str,
        folder: bool,
        size: u64,
        modified: Option<NaiveDateTime>,
    }

    impl Sortable for Row {
        fn is_folder(&self) -> bool {
            self.folder
        }
        fn name(&self) -> &str {
            self.name
        }
        fn size(&self) -> u64 {
            self.size
        }
        fn modified(&self) -> Option<NaiveDateTime> {
            self.modified
        }
    }

    fn file(name: &'static str, size: u64) -> Row {
        Row {
            name,
            folder: false,
            size,
            modified: None,
        }
    }

    fn folder(name: &'static str) -> Row {
        Row {
            name,
            folder: true,
            size: 0,
            modified: None,
        }
    }

    fn dated(name: &'static str, modified: Option<NaiveDateTime>) -> Row {
        Row {
            modified,
            ..file(name, 1)
        }
    }

    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.name).collect()
    }

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn folders_first_then_case_insensitive_names() {
        let mut rows = vec![
            file("index.html", 10),
            folder("Wp-content"),
            folder("about"),
            file("B.txt", 20),
        ];
        sort_rows(&mut rows, SortKey::Name, true);
        assert_eq!(names(&rows), ["about", "Wp-content", "B.txt", "index.html"]);
    }

    #[test]
    fn name_descending_reverses_within_each_group() {
        let mut rows = vec![
            file("index.html", 10),
            folder("Wp-content"),
            folder("about"),
            file("B.txt", 20),
        ];
        sort_rows(&mut rows, SortKey::Name, false);
        assert_eq!(names(&rows), ["Wp-content", "about", "index.html", "B.txt"]);
    }

    #[test]
    fn descending_keeps_folders_first() {
        let mut rows = vec![
            file("small.txt", 10),
            folder("zeta"),
            file("big.bin", 5_000),
            folder("alpha"),
            file("mid.css", 300),
        ];
        sort_rows(&mut rows, SortKey::Size, false);
        // Folders have no size, so they fall back to name order; files go largest first.
        assert_eq!(
            names(&rows),
            ["alpha", "zeta", "big.bin", "mid.css", "small.txt"]
        );
    }

    #[test]
    fn size_ascending_breaks_ties_by_name() {
        let mut rows = vec![file("b.txt", 5), file("a.txt", 5), file("c.txt", 1)];
        sort_rows(&mut rows, SortKey::Size, true);
        assert_eq!(names(&rows), ["c.txt", "a.txt", "b.txt"]);
    }

    #[test]
    fn modified_sorts_by_time_with_unknown_dates_first_ascending() {
        let mut rows = vec![
            dated("new.txt", Some(dt("2026-10-29 09:00"))),
            dated("unknown.txt", None),
            dated("old.txt", Some(dt("2020-01-01 00:00"))),
        ];
        sort_rows(&mut rows, SortKey::Modified, true);
        assert_eq!(names(&rows), ["unknown.txt", "old.txt", "new.txt"]);
        sort_rows(&mut rows, SortKey::Modified, false);
        assert_eq!(names(&rows), ["new.txt", "old.txt", "unknown.txt"]);
    }

    #[test]
    fn the_ascii_fast_path_orders_names_like_the_unicode_path() {
        let names = [
            "",
            "a",
            "A",
            "ab",
            "aB",
            "Ab",
            "abc",
            "b",
            "B.txt",
            "b.txt",
            "index.html",
            "Index.html",
            "file-1",
            "file-10",
            "file-2",
            "FILE-2",
            "a b",
            "a-b",
            "a_b",
            "a\u{e9}",
            "a\u{c9}",
            "ae",
            "\u{e9}a",
            "k",
            "K",
            "kk",
            // The Kelvin sign lowercases to an ASCII "k", so it must not take the fast path.
            "\u{212a}",
            "k\u{212a}",
            "\u{3a3}",
            "\u{3c3}",
        ];
        for a in names {
            for b in names {
                assert_eq!(
                    compare_names(a, b),
                    compare_names_unicode(a, b),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }
}
