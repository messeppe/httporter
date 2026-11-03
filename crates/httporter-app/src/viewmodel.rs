//! Turns directory listings (local and cloud) into the rows and breadcrumbs the file panes show.

use crate::format::{format_date, format_size};
use crate::local_fs::LocalEntry;
use crate::sort::{SortKey, Sortable, sort_rows};
use crate::{FileKind, FileRow};
use chrono::{DateTime, Local, NaiveDateTime, Utc};
use httporter_core::{Entry, RemotePath};
use slint::{ModelRc, VecModel};
use std::ffi::OsStr;
use std::path::{Component, Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKindData {
    Folder,
    Code,
    Image,
    Doc,
}

/// One row of a pane, with the size and date already formatted for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowData {
    pub name: String,
    pub kind: FileKindData,
    pub size: String,
    pub modified: String,
    pub folder: bool,
}

/// A listing entry's time in the PC's time zone, which is what the panes show.
fn to_local(t: DateTime<Utc>) -> NaiveDateTime {
    t.with_timezone(&Local).naive_local()
}

impl Sortable for Entry {
    fn is_folder(&self) -> bool {
        self.is_folder
    }
    fn name(&self) -> &str {
        Entry::name(self)
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn modified(&self) -> Option<NaiveDateTime> {
        self.modified.map(to_local)
    }
}

/// Lets `sort_rows` order a list of borrowed entries, so a listing is sorted without copying it.
impl<T: Sortable + ?Sized> Sortable for &T {
    fn is_folder(&self) -> bool {
        (**self).is_folder()
    }
    fn name(&self) -> &str {
        (**self).name()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn modified(&self) -> Option<NaiveDateTime> {
        (**self).modified()
    }
}

/// Shown in place of a folder's size and of a time that is not known (an em dash).
const NONE: &str = "\u{2014}";

const CODE: [&str; 8] = ["html", "htm", "css", "js", "mjs", "json", "xml", "xsl"];
const IMAGE: [&str; 8] = ["png", "jpg", "jpeg", "webp", "gif", "svg", "avif", "ico"];

/// Picks the icon for a row from the file's last extension, ignoring case. Anything that is not
/// a folder and not a known code or image extension is a document.
pub fn kind_for(name: &str, folder: bool) -> FileKindData {
    if folder {
        return FileKindData::Folder;
    }
    let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension);
    let is = |list: &[&str]| list.iter().any(|e| extension.eq_ignore_ascii_case(e));
    if is(&CODE) {
        FileKindData::Code
    } else if is(&IMAGE) {
        FileKindData::Image
    } else {
        FileKindData::Doc
    }
}

fn row(
    name: &str,
    folder: bool,
    size: u64,
    modified: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> RowData {
    RowData {
        name: name.to_owned(),
        kind: kind_for(name, folder),
        size: if folder {
            NONE.to_owned()
        } else {
            format_size(size)
        },
        modified: modified.map_or_else(|| NONE.to_owned(), |t| format_date(t, now)),
        folder,
    }
}

/// Rows for a cloud listing in the order `key` and `ascending` give (folders always first).
/// Times are converted from UTC to the PC's time zone; `now` is local time.
pub fn rows_from_remote_sorted(
    entries: &[Entry],
    now: NaiveDateTime,
    key: SortKey,
    ascending: bool,
) -> Vec<RowData> {
    // The name and the local time are worked out once per entry here, not on every comparison:
    // finding the last path segment and the time-zone lookup (about a microsecond) would
    // otherwise run O(n log n) times in a sort of tens of thousands of entries.
    let mut listed: Vec<Listed> = entries
        .iter()
        .map(|entry| Listed {
            name: entry.name(),
            is_folder: entry.is_folder,
            size: entry.size,
            modified: entry.modified.map(to_local),
        })
        .collect();
    sort_rows(&mut listed, key, ascending);
    listed
        .into_iter()
        .map(|l| row(l.name, l.is_folder, l.size, l.modified, now))
        .collect()
}

/// Sorts a cloud listing in place into the order [`rows_from_remote_sorted`] shows it in: the same
/// comparison on the same values, so a row's index is also its entry's index. As there, each
/// entry's local time is worked out once rather than on every comparison.
pub fn sort_remote_entries(entries: &mut Vec<Entry>, key: SortKey, ascending: bool) {
    struct Keyed {
        entry: Entry,
        modified: Option<NaiveDateTime>,
    }
    impl Sortable for Keyed {
        fn is_folder(&self) -> bool {
            self.entry.is_folder
        }
        fn name(&self) -> &str {
            self.entry.name()
        }
        fn size(&self) -> u64 {
            self.entry.size
        }
        fn modified(&self) -> Option<NaiveDateTime> {
            self.modified
        }
    }

    let mut keyed: Vec<Keyed> = std::mem::take(entries)
        .into_iter()
        .map(|entry| Keyed {
            modified: entry.modified.map(to_local),
            entry,
        })
        .collect();
    sort_rows(&mut keyed, key, ascending);
    entries.extend(keyed.into_iter().map(|k| k.entry));
}

/// The fields of a cloud entry that a pane row needs, ready to be compared.
struct Listed<'a> {
    name: &'a str,
    is_folder: bool,
    size: u64,
    modified: Option<NaiveDateTime>,
}

impl Sortable for Listed<'_> {
    fn is_folder(&self) -> bool {
        self.is_folder
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

/// Rows for a local folder in the order `key` and `ascending` give (folders always first).
pub fn rows_from_local_sorted(
    entries: &[LocalEntry],
    now: NaiveDateTime,
    key: SortKey,
    ascending: bool,
) -> Vec<RowData> {
    let mut sorted: Vec<&LocalEntry> = entries.iter().collect();
    sort_rows(&mut sorted, key, ascending);
    sorted
        .into_iter()
        .map(|e| row(&e.name, e.is_folder, e.size, e.modified, now))
        .collect()
}

/// The sort key for a column number sent by the pane's `sort` callback
/// (0 name, 1 size, 2 modified; anything else sorts by name).
pub fn sort_key_for_column(column: i32) -> SortKey {
    match column {
        1 => SortKey::Size,
        2 => SortKey::Modified,
        _ => SortKey::Name,
    }
}

/// Breadcrumbs for the cloud pane: the zone, then one entry per folder, so crumb `i` is the
/// folder with `i` segments (the zone is the root).
pub fn remote_crumbs(zone: &str, dir: &RemotePath) -> Vec<String> {
    std::iter::once(zone)
        .chain(dir.segments())
        .map(str::to_owned)
        .collect()
}

/// Breadcrumbs for the local pane: the drive (`D:`) or network share, then one entry per folder.
/// Empty for an empty path.
pub fn local_crumbs(dir: &Path) -> Vec<String> {
    dir.components()
        .filter_map(|c| match c {
            Component::Prefix(prefix) => Some(prefix.as_os_str()),
            Component::Normal(name) => Some(name),
            Component::ParentDir => Some(OsStr::new("..")),
            Component::RootDir | Component::CurDir => None,
        })
        .map(|part| part.to_string_lossy().into_owned())
        .collect()
}

pub(crate) fn slint_kind(kind: FileKindData) -> FileKind {
    match kind {
        FileKindData::Folder => FileKind::Folder,
        FileKindData::Code => FileKind::Code,
        FileKindData::Image => FileKind::Image,
        FileKindData::Doc => FileKind::Doc,
    }
}

/// The model a pane's `rows` property takes.
pub fn to_slint_rows(rows: &[RowData]) -> ModelRc<FileRow> {
    let rows: Vec<FileRow> = rows
        .iter()
        .map(|r| FileRow {
            name: r.name.as_str().into(),
            kind: slint_kind(r.kind),
            size: r.size.as_str().into(),
            modified: r.modified.as_str().into(),
            folder: r.folder,
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppWindow;
    use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
    use slint::Model;
    use std::time::{Duration, Instant};

    /// How long 50,000 rows may take. Unoptimised code is several times slower than the release
    /// build the number is about, and a test run right after a compile is slower again, so a debug
    /// build gets four times the time (ledger 229). The release number is what S4 promises.
    fn budget_for_50k() -> Duration {
        Duration::from_millis(500) * if cfg!(debug_assertions) { 4 } else { 1 }
    }

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 29)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    fn entry(path: &str, folder: bool, size: u64, modified: Option<DateTime<Utc>>) -> Entry {
        Entry {
            path: RemotePath::parse(path).unwrap(),
            is_folder: folder,
            size,
            modified,
            checksum: None,
            content_type: None,
            replicated_regions: Vec::new(),
        }
    }

    fn local(name: &str, folder: bool, size: u64, modified: Option<NaiveDateTime>) -> LocalEntry {
        LocalEntry {
            name: name.into(),
            is_folder: folder,
            size,
            modified,
        }
    }

    fn names(rows: &[RowData]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    /// Folders first, then by name: the order a pane starts in.
    fn rows_from_remote(entries: &[Entry], now: NaiveDateTime) -> Vec<RowData> {
        rows_from_remote_sorted(entries, now, SortKey::Name, true)
    }

    fn rows_from_local(entries: &[LocalEntry], now: NaiveDateTime) -> Vec<RowData> {
        rows_from_local_sorted(entries, now, SortKey::Name, true)
    }

    fn synthetic_entries(count: usize) -> Vec<Entry> {
        const EXTENSIONS: [&str; 5] = ["html", "webp", "css", "txt", "json"];
        (0..count)
            .map(|i| {
                let folder = i % 10 == 0;
                let name = if folder {
                    format!("folder-{i:05}")
                } else {
                    format!("file-{i:05}.{}", EXTENSIONS[i % EXTENSIONS.len()])
                };
                let modified = Utc
                    .timestamp_opt(1_700_000_000 + (i as i64) * 61, 0)
                    .single();
                entry(
                    &format!("wp-content/{name}"),
                    folder,
                    i as u64 * 977,
                    modified,
                )
            })
            .collect()
    }

    #[test]
    fn kinds() {
        assert!(matches!(kind_for("index.html", false), FileKindData::Code));
        assert!(matches!(
            kind_for("og-cover.webp", false),
            FileKindData::Image
        ));
        assert!(matches!(kind_for("robots.txt", false), FileKindData::Doc));
        assert!(matches!(kind_for("wp-content", true), FileKindData::Folder));
    }

    #[test]
    fn every_listed_extension_has_its_kind() {
        for ext in ["html", "htm", "css", "js", "mjs", "json", "xml", "xsl"] {
            assert_eq!(
                kind_for(&format!("a.{ext}"), false),
                FileKindData::Code,
                "{ext}"
            );
        }
        for ext in ["png", "jpg", "jpeg", "webp", "gif", "svg", "avif", "ico"] {
            assert_eq!(
                kind_for(&format!("a.{ext}"), false),
                FileKindData::Image,
                "{ext}"
            );
        }
    }

    #[test]
    fn kind_ignores_case_and_uses_the_last_extension() {
        assert_eq!(kind_for("PHOTO.JPG", false), FileKindData::Image);
        assert_eq!(kind_for("Style.CsS", false), FileKindData::Code);
        assert_eq!(kind_for("backup.html.gz", false), FileKindData::Doc);
        assert_eq!(kind_for("archive.tar.gz", false), FileKindData::Doc);
        assert_eq!(kind_for("README", false), FileKindData::Doc);
        assert_eq!(kind_for("trailing.", false), FileKindData::Doc);
        assert_eq!(kind_for(".htaccess", false), FileKindData::Doc);
        // A folder is a folder whatever its name looks like.
        assert_eq!(kind_for("site.html", true), FileKindData::Folder);
    }

    #[test]
    fn remote_rows_format_and_order() {
        let entries = [
            entry("index.html", false, 392_641, None),
            entry("wp-content", true, 0, None),
        ];
        let rows = rows_from_remote(&entries, now());
        assert_eq!(names(&rows), ["wp-content", "index.html"]);
        assert_eq!(rows[0].size, "\u{2014}");
        assert_eq!(rows[0].kind, FileKindData::Folder);
        assert!(rows[0].folder);
        assert_eq!(rows[1].size, "383.4 KB");
        assert_eq!(rows[1].kind, FileKindData::Code);
        assert!(!rows[1].folder);
    }

    #[test]
    fn local_rows_format_and_order() {
        let modified = NaiveDate::from_ymd_opt(2026, 10, 29)
            .unwrap()
            .and_hms_opt(10, 2, 0)
            .unwrap();
        let entries = [
            local("robots.txt", false, 67, Some(modified)),
            local("og-cover.webp", false, 214 * 1024, Some(modified)),
            local("wp-content", true, 0, Some(modified)),
        ];
        let rows = rows_from_local(&entries, now());
        assert_eq!(names(&rows), ["wp-content", "og-cover.webp", "robots.txt"]);
        assert_eq!(rows[0].size, "\u{2014}");
        assert_eq!(rows[1].size, "214.0 KB");
        assert_eq!(rows[1].kind, FileKindData::Image);
        assert_eq!(rows[2].size, "67 B");
        assert_eq!(rows[2].modified, "Today 10:02");
    }

    #[test]
    fn unknown_modified_time_shows_a_dash() {
        let rows = rows_from_local(&[local("a.txt", false, 1, None)], now());
        assert_eq!(rows[0].modified, "\u{2014}");
        let rows = rows_from_remote(&[entry("a.txt", false, 1, None)], now());
        assert_eq!(rows[0].modified, "\u{2014}");
    }

    #[test]
    fn remote_times_are_shown_in_local_time() {
        let t = Utc::now();
        let local_now = t.with_timezone(&Local).naive_local();
        let rows = rows_from_remote(&[entry("a.txt", false, 1, Some(t))], local_now);
        assert_eq!(
            rows[0].modified,
            format!("Today {}", local_now.format("%H:%M"))
        );
    }

    #[test]
    fn entries_sort_by_their_local_modified_time() {
        let at = |secs| Utc.timestamp_opt(secs, 0).single();
        let older = entry("older.txt", false, 1, at(1_700_000_000));
        let newer = entry("newer.txt", false, 1, at(1_700_100_000));
        assert!(Sortable::modified(&older) < Sortable::modified(&newer));
        assert_eq!(
            Sortable::modified(&older),
            older
                .modified
                .map(|t| t.with_timezone(&Local).naive_local())
        );
        assert_eq!(Sortable::name(&older), "older.txt");
    }

    #[test]
    fn sorted_variants_apply_key_and_direction_with_folders_first() {
        let entries = [
            entry("small.txt", false, 10, None),
            entry("big.txt", false, 5_000, None),
            entry("zeta", true, 0, None),
            entry("mid.txt", false, 300, None),
        ];
        let rows = rows_from_remote_sorted(&entries, now(), SortKey::Size, false);
        assert_eq!(names(&rows), ["zeta", "big.txt", "mid.txt", "small.txt"]);

        let locals = [
            local("small.txt", false, 10, None),
            local("zeta", true, 0, None),
            local("big.txt", false, 5_000, None),
        ];
        let rows = rows_from_local_sorted(&locals, now(), SortKey::Size, true);
        assert_eq!(names(&rows), ["zeta", "small.txt", "big.txt"]);
    }

    #[test]
    fn column_numbers_map_to_sort_keys() {
        assert_eq!(sort_key_for_column(0), SortKey::Name);
        assert_eq!(sort_key_for_column(1), SortKey::Size);
        assert_eq!(sort_key_for_column(2), SortKey::Modified);
        assert_eq!(sort_key_for_column(7), SortKey::Name);
        assert_eq!(sort_key_for_column(-1), SortKey::Name);
    }

    #[test]
    fn crumbs() {
        assert_eq!(
            remote_crumbs(
                "exampleshop",
                &RemotePath::parse("wp-content/uploads").unwrap()
            ),
            vec!["exampleshop", "wp-content", "uploads"]
        );
        assert_eq!(
            local_crumbs(Path::new("D:\\site-export")),
            vec!["D:", "site-export"]
        );
    }

    #[test]
    fn crumbs_at_the_top_of_each_tree() {
        assert_eq!(
            remote_crumbs("exampleshop", &RemotePath::root()),
            vec!["exampleshop"]
        );
        assert_eq!(local_crumbs(Path::new("D:\\")), vec!["D:"]);
        assert_eq!(local_crumbs(Path::new("")), Vec::<String>::new());
    }

    #[test]
    fn local_crumbs_handle_nested_and_network_paths() {
        assert_eq!(
            local_crumbs(Path::new("C:\\Users\\me\\Documents\\site")),
            vec!["C:", "Users", "me", "Documents", "site"]
        );
        assert_eq!(
            local_crumbs(Path::new("\\\\server\\share\\dir")),
            vec!["\\\\server\\share", "dir"]
        );
    }

    #[test]
    fn the_50k_budget_is_four_times_longer_in_debug_builds() {
        let release = Duration::from_millis(500);
        assert_eq!(
            budget_for_50k(),
            if cfg!(debug_assertions) {
                release * 4
            } else {
                release
            }
        );
    }

    #[test]
    fn converts_50k_entries_quickly() {
        let entries = synthetic_entries(50_000);
        let t = Instant::now();
        let rows = rows_from_remote(&entries, now());
        assert_eq!(rows.len(), 50_000);
        assert!(t.elapsed() < budget_for_50k(), "{:?}", t.elapsed());
    }

    #[test]
    fn sorting_50k_entries_by_date_is_also_quick() {
        let entries = synthetic_entries(50_000);
        let t = Instant::now();
        let rows = rows_from_remote_sorted(&entries, now(), SortKey::Modified, false);
        assert_eq!(rows.len(), 50_000);
        assert!(t.elapsed() < budget_for_50k(), "{:?}", t.elapsed());
    }

    #[test]
    fn sorted_entries_are_in_row_order_for_every_key_and_direction() {
        let mut entries = synthetic_entries(5_000);
        // Ties on size and on the modified time, so the tie-break by name matters too.
        for (i, e) in entries.iter_mut().enumerate() {
            e.size = (i % 7) as u64;
            if i % 3 == 0 {
                e.modified = None;
            }
        }
        for key in [SortKey::Name, SortKey::Size, SortKey::Modified] {
            for ascending in [true, false] {
                sort_remote_entries(&mut entries, key, ascending);
                let rows = rows_from_remote_sorted(&entries, now(), key, ascending);
                let entry_names: Vec<&str> = entries.iter().map(Entry::name).collect();
                assert_eq!(names(&rows), entry_names, "{key:?} ascending={ascending}");
            }
        }
    }

    #[test]
    fn sorting_50k_entries_in_place_by_date_is_quick() {
        let mut entries = synthetic_entries(50_000);
        let t = Instant::now();
        sort_remote_entries(&mut entries, SortKey::Modified, false);
        assert!(t.elapsed() < budget_for_50k(), "{:?}", t.elapsed());
        assert_eq!(entries.len(), 50_000);
    }

    #[test]
    fn slint_rows_carry_every_field_and_kind() {
        let rows = [
            RowData {
                name: "wp-content".into(),
                kind: FileKindData::Folder,
                size: "\u{2014}".into(),
                modified: "Today 10:02".into(),
                folder: true,
            },
            RowData {
                name: "index.html".into(),
                kind: FileKindData::Code,
                size: "383.4 KB".into(),
                modified: "Yesterday 17:02".into(),
                folder: false,
            },
            RowData {
                name: "a.png".into(),
                kind: FileKindData::Image,
                size: "1 B".into(),
                modified: "".into(),
                folder: false,
            },
            RowData {
                name: "robots.txt".into(),
                kind: FileKindData::Doc,
                size: "67 B".into(),
                modified: "".into(),
                folder: false,
            },
        ];
        let model = to_slint_rows(&rows);
        assert_eq!(model.row_count(), 4);
        let kinds: Vec<FileKind> = model.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                FileKind::Folder,
                FileKind::Code,
                FileKind::Image,
                FileKind::Doc
            ]
        );
        let second = model.row_data(1).unwrap();
        assert_eq!(second.name, "index.html");
        assert_eq!(second.size, "383.4 KB");
        assert_eq!(second.modified, "Yesterday 17:02");
        assert!(model.row_data(0).unwrap().folder && !second.folder);
    }

    #[test]
    fn window_accepts_rows() {
        i_slint_backend_testing::init_no_event_loop();
        let w = AppWindow::new().unwrap();
        let rows: Vec<RowData> = (0..3)
            .map(|i| RowData {
                name: format!("file-{i}.txt"),
                kind: FileKindData::Doc,
                size: "1 B".into(),
                modified: "".into(),
                folder: false,
            })
            .collect();
        w.set_remote_rows(to_slint_rows(&rows));
        assert_eq!(w.get_remote_rows().row_count(), 3);
        w.set_local_rows(to_slint_rows(&rows[..2]));
        assert_eq!(w.get_local_rows().row_count(), 2);
    }
}
