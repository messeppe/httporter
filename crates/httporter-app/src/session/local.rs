// SPDX-License-Identifier: MIT OR Apache-2.0

//! The This PC pane: the folder (or the list of drives) shown from the PC, and the requests that
//! move it.

use std::io;
use std::path::{Path, PathBuf};

use super::{Course, Session, Step, Ticket, lock};
use crate::copy;
use crate::local_fs::{LocalEntry, LocalListing, list_drives, list_local};
use crate::sort::{SortKey, sort_rows};
use crate::viewmodel::{RowData, local_crumbs, rows_from_local_sorted};

/// What the This PC pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalView {
    /// As [`super::RemoteView::version`], for this pane.
    pub version: u64,
    pub rows: Vec<RowData>,
    pub crumbs: Vec<String>,
    /// For the status bar: "{n} items", plus how many could not be read.
    pub status: String,
    pub can_back: bool,
    pub can_forward: bool,
    /// As [`super::RemoteView::loading`].
    pub loading: bool,
    pub error: Option<String>,
}

/// The This PC pane.
pub(super) struct LocalPane {
    /// As the cloud pane's `latest`: the latest request that took effect.
    latest: Ticket,
    /// See [`LocalView::version`].
    pub(super) version: u64,
    sort: (SortKey, bool),
    /// Places: `None` is the list of drives. Also where the request on its way is going.
    course: Course<Option<PathBuf>>,
    /// `None` for the list of drives.
    dir: Option<PathBuf>,
    /// In the order the rows show them, so a row index is an index into this list.
    pub(super) entries: Vec<LocalEntry>,
    skipped: usize,
    error: Option<String>,
    /// As [`super::RemoteView::loading`].
    loading: bool,
}

impl LocalPane {
    /// A pane at the list of drives, sorted by `sort`.
    pub(super) fn new(sort: (SortKey, bool)) -> Self {
        let mut drives = drive_entries();
        sort_rows(&mut drives, sort.0, sort.1);
        Self {
            latest: Ticket(0),
            version: 0,
            sort,
            course: Course::default(),
            dir: None,
            entries: drives,
            skipped: 0,
            error: None,
            loading: false,
        }
    }
}

/// The PC's drives, as folders named like their breadcrumb ("C:").
fn drive_entries() -> Vec<LocalEntry> {
    list_drives()
        .iter()
        .map(|root| LocalEntry {
            name: root.to_string_lossy().trim_end_matches('\\').to_owned(),
            is_folder: true,
            size: 0,
            modified: None,
        })
        .collect()
}

/// Where the item `name` of the folder `dir` is; at the list of drives (`None`), the drive.
fn entry_path(dir: Option<&Path>, name: &str) -> PathBuf {
    match dir {
        Some(dir) => dir.join(name),
        None => PathBuf::from(format!("{name}\\")),
    }
}

/// The folder This PC breadcrumb `index` of `dir` stands for (0 is the drive), or `None` for the
/// current folder and past it.
pub(super) fn local_ancestor(dir: &Path, index: usize) -> Option<PathBuf> {
    let mut chain: Vec<&Path> = dir.ancestors().collect();
    chain.reverse();
    // The last one is `dir` itself.
    (index + 1 < chain.len()).then(|| chain[index].to_path_buf())
}

impl Session {
    pub fn local_view(&self) -> LocalView {
        self.local_view_of(&lock(&self.local))
    }

    /// Opens row `row` of the This PC pane if it is a folder (or a drive), and if the pane is
    /// still at view version `seen` (as for [`Self::open_remote`]).
    pub async fn open_local(
        &self,
        ticket: Ticket,
        row: usize,
        seen: u64,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        let pick = |pane: &LocalPane| {
            if pane.version != seen {
                return None;
            }
            let entry = pane.entries.get(row).filter(|entry| entry.is_folder)?;
            Some(Some(entry_path(pane.dir.as_deref(), &entry.name)))
        };
        self.navigate_local(ticket, Step::Go, pick, started).await
    }

    /// Goes to the parent folder; from a drive's root, to the list of drives.
    pub async fn local_up(
        &self,
        ticket: Ticket,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        let pick = |pane: &LocalPane| {
            pane.dir
                .as_deref()
                .map(|dir| dir.parent().map(Path::to_path_buf))
        };
        self.navigate_local(ticket, Step::Go, pick, started).await
    }

    /// Goes to the folder of This PC breadcrumb `index` (0 is the drive), if the crumbs are
    /// still those of view version `seen`.
    pub async fn local_crumb(
        &self,
        ticket: Ticket,
        index: usize,
        seen: u64,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        let pick = |pane: &LocalPane| {
            if pane.version != seen {
                return None;
            }
            pane.dir
                .as_deref()
                .and_then(|dir| local_ancestor(dir, index))
                .map(Some)
        };
        self.navigate_local(ticket, Step::Go, pick, started).await
    }

    pub async fn local_back(
        &self,
        ticket: Ticket,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        // Counted from where a Back already on its way is going: two quick Backs are two steps.
        let pick = |pane: &LocalPane| pane.course.ahead().back_to();
        self.navigate_local(ticket, Step::Back, pick, started).await
    }

    pub async fn local_forward(
        &self,
        ticket: Ticket,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        let pick = |pane: &LocalPane| pane.course.ahead().forward_to();
        self.navigate_local(ticket, Step::Forward, pick, started)
            .await
    }

    /// Lists the folder (or the drives) shown again, as [`Self::remote_refresh`].
    pub async fn local_refresh(
        &self,
        ticket: Ticket,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        let pick = |pane: &LocalPane| Some(pane.course.refresh_target(&pane.dir));
        self.navigate_local(ticket, Step::Refresh, pick, started)
            .await
    }

    /// Reorders the This PC pane (and every later listing in it).
    pub fn sort_local(&self, key: SortKey, ascending: bool) -> LocalView {
        let mut pane = lock(&self.local);
        pane.sort = (key, ascending);
        sort_rows(&mut pane.entries, key, ascending);
        pane.version += 1;
        self.local_view_of(&pane)
    }

    /// Lists the place `pick` chooses (if it chooses one; `None` inside is the list of drives) on
    /// tokio's blocking threads and goes there by `step`. As for the cloud pane, a request older
    /// than one that has taken effect is dropped, `started` gets the pane as it is loading, an
    /// answer is used only while its request is the latest, and a failure leaves the pane where
    /// it was.
    pub(super) async fn navigate_local(
        &self,
        ticket: Ticket,
        step: Step,
        pick: impl FnOnce(&LocalPane) -> Option<Option<PathBuf>>,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        let (target, loading) = {
            let mut pane = lock(&self.local);
            if ticket < pane.latest {
                return self.local_view_of(&pane);
            }
            let Some(target) = pick(&pane) else {
                return self.local_view_of(&pane);
            };
            let shown = pane.dir.clone();
            pane.course.begin(step, &shown, target.clone());
            pane.latest = ticket;
            pane.loading = true;
            pane.version += 1;
            (target, self.local_view_of(&pane))
        };
        started(loading);
        let listed = match &target {
            None => Ok(LocalListing {
                entries: drive_entries(),
                skipped: 0,
            }),
            Some(dir) => {
                let dir = dir.clone();
                crate::rt::spawn_blocking(move || list_local(&dir))
                    .await
                    .unwrap_or_else(|e| Err(io::Error::other(e)))
            }
        };
        let mut pane = lock(&self.local);
        if pane.latest != ticket {
            return self.local_view_of(&pane);
        }
        match listed {
            Ok(listing) => {
                let (key, ascending) = pane.sort;
                pane.dir = target;
                pane.course.arrive();
                pane.entries = listing.entries;
                sort_rows(&mut pane.entries, key, ascending);
                pane.skipped = listing.skipped;
                pane.error = None;
            }
            Err(e) => {
                let place = target
                    .as_deref()
                    .map_or_else(|| copy::THIS_PC.to_owned(), |dir| dir.display().to_string());
                pane.course.fail();
                pane.error = Some(if e.kind() == io::ErrorKind::NotFound {
                    copy::local_not_found(&place)
                } else {
                    copy::local_open_failed(&place, e)
                });
            }
        }
        pane.loading = false;
        pane.version += 1;
        self.local_view_of(&pane)
    }

    /// The folder the This PC pane shows; `None` at the list of drives.
    pub fn local_dir(&self) -> Option<PathBuf> {
        lock(&self.local).dir.clone()
    }

    /// The folder the This PC pane shows, while no listing for it is on its way (`Some(None)` is
    /// the list of drives).
    pub fn local_shows(&self) -> Option<Option<PathBuf>> {
        let pane = lock(&self.local);
        (!pane.loading).then(|| pane.dir.clone())
    }

    /// The paths of the This PC pane's items named `names` (the rows selected), in the order
    /// given: files and folders in the folder shown, or drives at the list of drives.
    pub fn local_paths(&self, names: &[String]) -> Vec<PathBuf> {
        let pane = lock(&self.local);
        names
            .iter()
            .filter(|name| pane.entries.iter().any(|entry| entry.name == **name))
            .map(|name| entry_path(pane.dir.as_deref(), name))
            .collect()
    }

    /// The folder (or drive) of the This PC pane's row `row`, if it is one.
    pub fn local_folder_at(&self, row: usize) -> Option<PathBuf> {
        let pane = lock(&self.local);
        let entry = pane.entries.get(row).filter(|entry| entry.is_folder)?;
        Some(entry_path(pane.dir.as_deref(), &entry.name))
    }

    pub(super) fn local_view_of(&self, pane: &LocalPane) -> LocalView {
        let (key, ascending) = pane.sort;
        LocalView {
            version: pane.version,
            rows: rows_from_local_sorted(&pane.entries, (self.now)(), key, ascending),
            crumbs: pane.dir.as_deref().map(local_crumbs).unwrap_or_default(),
            status: copy::items_status(pane.entries.len(), pane.skipped),
            can_back: !pane.course.history.back.is_empty(),
            can_forward: !pane.course.history.forward.is_empty(),
            loading: pane.loading,
            error: pane.error.clone(),
        }
    }
}
