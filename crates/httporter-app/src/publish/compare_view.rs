// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compare in the main window (T6): the toolbar toggle, the Status column, the "Comparing" chip
//! and the status bar's summary.
//!
//! Turning Compare on compares the folders the two panes show (the roots) on the runtime, with the
//! skip rules of a profile for those same folders, else the defaults. Each pane's rows inside its
//! root then show how they compare: a listing marks its rows as it builds them
//! ([`Publisher::rows_built`]), and the rows on screen are marked again when a compare ends. A
//! batch that finishes on the site compares again; turning Compare off, or opening another site,
//! gives the compare up.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use httporter_core::{
    CompareStatus as Status, RemotePath, SiteId, SiteRecord, SkipRules, StorageProvider,
};
use httporter_transfer::{CompareCounts, CompareMode, CompareResult, ScanProgress, compare_trees};
use slint::{ComponentHandle, Model, ModelRc};
use tokio::task::JoinHandle;

use super::{Publisher, copy};
use crate::selection::ActivePane;
use crate::viewmodel::local_crumbs;
use crate::{AppWindow, CompareStatus as RowStatus, CompareSummary, FileRow};

/// How long the watch on the site's batches waits after looking at the engine, so that a busy
/// engine is looked at a few times a second at most.
const WATCH_EVERY: Duration = Duration::from_millis(300);

thread_local! {
    /// The window's Publisher, for the listings to mark their rows with ([`Publisher::rows_built`]).
    /// Set by [`install`]; only the UI thread uses it. Empty in a window without the transfer
    /// engine (the window's tests), where the listings mark nothing.
    static PUBLISHER: RefCell<Weak<Publisher>> = const { RefCell::new(Weak::new()) };
}

/// What the compare remembers between callbacks.
#[derive(Default)]
pub(super) struct CompareState {
    /// Counts the compares started: what a compare that is not the latest finds is dropped.
    run: u64,
    /// The compare that is on, if any.
    on: Option<Compared>,
}

/// A compare that is on: its two folders, what it found, and the tasks working for it (dropping
/// it stops them).
struct Compared {
    site: SiteRecord,
    provider: Arc<dyn StorageProvider>,
    local_root: PathBuf,
    remote_root: RemotePath,
    /// This PC's crumbs at the local root, which the crumbs of every folder within it start with.
    local_crumbs: Vec<String>,
    /// The site's side in "Only on …": Bunny, or the site's name.
    label: String,
    /// What the latest compare found; kept while the next one runs.
    result: Option<Arc<CompareResult>>,
    counts: CompareCounts,
    /// `result`'s statuses of a folder's children, by the folder's relative path, as the
    /// listings ask for them.
    children: HashMap<String, Arc<HashMap<String, Status>>>,
    /// While a compare runs: how far it is.
    progress: String,
    /// Why the last compare failed.
    error: String,
    /// The compare on its way, and the watch on the site's batches.
    task: Option<JoinHandle<()>>,
    watch: Option<JoinHandle<()>>,
}

impl Drop for Compared {
    fn drop(&mut self) {
        // A compare given up is dropped, which cancels it.
        for task in [self.task.take(), self.watch.take()].into_iter().flatten() {
            task.abort();
        }
    }
}

impl Compared {
    /// What the status bar and the panes show of it.
    fn summary(&self) -> CompareSummary {
        let count = |n: u64| i32::try_from(n).unwrap_or(i32::MAX);
        CompareSummary {
            active: true,
            busy: self.task.is_some(),
            progress: self.progress.as_str().into(),
            changed: count(self.counts.changed),
            new: count(self.counts.new),
            remote_only: count(self.counts.remote_only),
            remote_label: self.label.as_str().into(),
            error: self.error.as_str().into(),
        }
    }

    /// The statuses of the rows of `pane` that show the folder at `crumbs`, by name: `None` for
    /// a folder outside the pane's root, or before the first compare has found anything.
    fn statuses_at(
        &mut self,
        pane: ActivePane,
        crumbs: &[String],
    ) -> Option<Arc<HashMap<String, Status>>> {
        let rel = match pane {
            ActivePane::ThisPc => {
                // Windows paths ignore case.
                rel_below(&self.local_crumbs, crumbs, |a, b| {
                    a.to_lowercase() == b.to_lowercase()
                })?
            }
            ActivePane::Cloud => {
                // The first crumb is the zone or bucket, the root of the site.
                let (_, folders) = crumbs.split_first()?;
                let root: Vec<&str> = self.remote_root.segments().collect();
                rel_below(&root, folders, |a, b| a == b)?
            }
        };
        let result = self.result.clone()?;
        let statuses = self
            .children
            .entry(rel)
            .or_insert_with_key(|rel| Arc::new(result.child_statuses(rel)));
        Some(statuses.clone())
    }
}

/// The relative path ('/' between folders, "" for the root) of the folder at `crumbs` within the
/// folder at `root` (both as crumbs), or `None` when it isn't within it.
fn rel_below<R: AsRef<str>, C: AsRef<str>>(
    root: &[R],
    crumbs: &[C],
    same: impl Fn(&str, &str) -> bool,
) -> Option<String> {
    if crumbs.len() < root.len() {
        return None;
    }
    let (head, below) = crumbs.split_at(root.len());
    head.iter()
        .zip(root)
        .all(|(c, r)| same(c.as_ref(), r.as_ref()))
        .then(|| {
            below
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<&str>>()
                .join("/")
        })
}

/// The Slint status of a compared file or folder.
fn row_status(status: Status) -> RowStatus {
    match status {
        Status::Changed => RowStatus::Changed,
        Status::New => RowStatus::New,
        Status::RemoteOnly => RowStatus::RemoteOnly,
        Status::Same => RowStatus::Same,
    }
}

/// Sets each row's status from `statuses` (by name; none for a row it doesn't name, or with no
/// `statuses`). Only rows whose status changes are written back.
fn mark(rows: &ModelRc<FileRow>, statuses: Option<&HashMap<String, Status>>) {
    for index in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(index) else {
            continue;
        };
        let status = statuses
            .and_then(|statuses| statuses.get(row.name.as_str()))
            .map_or(RowStatus::None, |status| row_status(*status));
        if row.status != status {
            row.status = status;
            rows.set_row_data(index, row);
        }
    }
}

/// The crumbs a pane shows, as text.
fn crumbs_of(model: &ModelRc<slint::SharedString>) -> Vec<String> {
    model.iter().map(|crumb| crumb.to_string()).collect()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a compare task needs of the compare it runs for.
struct Job {
    run: u64,
    site: SiteRecord,
    provider: Arc<dyn StorageProvider>,
    local_root: PathBuf,
    remote_root: RemotePath,
}

impl Publisher {
    /// A pane's rows were just built from a listing of the folder at `crumbs` (on the UI thread,
    /// before they are shown): each row inside the compared folder gets its status, and the
    /// toolbar's Compare and Publish follow what the panes now show. A cloud listing of another
    /// site than the one compared (`site`) ends the compare. Does nothing in a window without a
    /// Publisher.
    pub(crate) fn rows_built(
        ui: &AppWindow,
        pane: ActivePane,
        site: Option<&SiteId>,
        crumbs: &[String],
        rows: &ModelRc<FileRow>,
    ) {
        let Some(p) = PUBLISHER.with(|cell| cell.borrow().upgrade()) else {
            return;
        };
        let site_open = p.session.remote_place().is_some();
        ui.set_can_publish(site_open);
        ui.set_can_compare(site_open && p.session.local_dir().is_some());

        let mut state = lock(&p.compare);
        let Some(on) = state.on.as_mut() else {
            return;
        };
        if pane == ActivePane::Cloud && site != Some(&on.site.id) {
            drop(state);
            p.stop_compare(ui);
            return;
        }
        let statuses = on.statuses_at(pane, crumbs);
        drop(state);
        mark(rows, statuses.as_deref());
    }

    /// The toolbar's Compare toggle.
    fn toggle_compare(self: &Arc<Self>, ui: &AppWindow) {
        if lock(&self.compare).on.is_some() {
            self.stop_compare(ui);
        } else {
            self.start_compare(ui);
        }
    }

    /// Turns Compare on for the folders the panes show, if each shows one.
    fn start_compare(self: &Arc<Self>, ui: &AppWindow) {
        let (Some(place), Some(local_root)) =
            (self.session.remote_place(), self.session.local_dir())
        else {
            return;
        };
        // Batches that finished before Compare was on are in what it finds.
        let seen = self
            .transfers
            .engine()
            .snapshot()
            .last_batch
            .map(|batch| batch.finished_at);
        let site_name = place.site.name.clone();
        lock(&self.compare).on = Some(Compared {
            label: copy::remote_label(&place.site),
            local_crumbs: local_crumbs(&local_root),
            site: place.site,
            provider: place.provider,
            local_root,
            remote_root: place.dir,
            result: None,
            counts: CompareCounts::default(),
            children: HashMap::new(),
            progress: String::new(),
            error: String::new(),
            task: None,
            watch: None,
        });
        ui.set_comparing(true);
        self.compare_again(ui);
        let watch = self
            .link
            .rt()
            .spawn(self.clone().watch_batches(site_name, seen));
        match lock(&self.compare).on.as_mut() {
            Some(on) => on.watch = Some(watch),
            None => watch.abort(),
        }
    }

    /// Turns Compare off: the compare on its way is given up and the rows lose their statuses.
    fn stop_compare(&self, ui: &AppWindow) {
        let on = lock(&self.compare).on.take();
        drop(on);
        ui.set_comparing(false);
        ui.set_compare(CompareSummary::default());
        mark(&ui.get_local_rows(), None);
        mark(&ui.get_remote_rows(), None);
    }

    /// Runs the compare that is on (again), giving up one on its way. The panes keep what the
    /// last one found until this one ends.
    fn compare_again(self: &Arc<Self>, ui: &AppWindow) {
        let mut state = lock(&self.compare);
        state.run += 1;
        let run = state.run;
        let Some(on) = state.on.as_mut() else {
            return;
        };
        on.error.clear();
        on.progress = copy::COMPARING.to_owned();
        let job = Job {
            run,
            site: on.site.clone(),
            provider: on.provider.clone(),
            local_root: on.local_root.clone(),
            remote_root: on.remote_root.clone(),
        };
        let task = self.link.rt().spawn(self.clone().run_compare(job));
        if let Some(old) = on.task.replace(task) {
            old.abort();
        }
        let summary = on.summary();
        drop(state);
        ui.set_compare(summary);
    }

    /// Compares on the runtime, telling the window how far it is, then what it found.
    async fn run_compare(self: Arc<Self>, job: Job) {
        let mode =
            CompareMode::for_site(job.site.provider, self.session.settings().verify_checksums);
        let rules = self
            .session
            .profiles()
            .await
            .into_iter()
            .find(|profile| {
                profile.site == job.site.id
                    && profile.local_dir == job.local_root
                    && profile.remote_dir == job.remote_root
            })
            .map_or_else(SkipRules::defaults, |profile| profile.skip_rules);
        let skip = SkipRules::new(&rules);
        let progress = {
            let this = self.clone();
            move |progress: ScanProgress| {
                this.post(job.run, false, move |on| {
                    on.progress = copy::compare_progress(progress);
                });
            }
        };
        let found = compare_trees(
            job.provider.as_ref(),
            &job.local_root,
            &job.remote_root,
            &skip,
            mode,
            &self.hashes,
            &progress,
        )
        .await
        .map(|result| {
            let counts = result.counts();
            (Arc::new(result), counts)
        });
        self.post(job.run, true, move |on| {
            on.task = None;
            match found {
                Ok((result, counts)) => {
                    on.result = Some(result);
                    on.counts = counts;
                    on.children.clear();
                }
                Err(error) => on.error = copy::compare_failed(&error),
            }
        });
    }

    /// Changes the compare that is on (on the UI thread), if compare `run` is still the latest,
    /// and shows its summary; with `remark`, marks the rows on screen again.
    fn post(
        self: &Arc<Self>,
        run: u64,
        remark: bool,
        change: impl FnOnce(&mut Compared) + Send + 'static,
    ) {
        let this = self.clone();
        // An error means the window is gone.
        let _ = self.link.ui().upgrade_in_event_loop(move |ui| {
            let mut state = lock(&this.compare);
            if state.run != run {
                return;
            }
            let Some(on) = state.on.as_mut() else {
                return;
            };
            change(on);
            let summary = on.summary();
            drop(state);
            ui.set_compare(summary);
            if remark {
                this.mark_shown(&ui);
            }
        });
    }

    /// Marks the rows the panes show with what the compare that is on found.
    fn mark_shown(&self, ui: &AppWindow) {
        let local = crumbs_of(&ui.get_local_crumbs());
        let remote = crumbs_of(&ui.get_remote_crumbs());
        let shown_site = usize::try_from(ui.get_selected_site())
            .ok()
            .and_then(|index| self.session.site_id_at(index));
        let mut state = lock(&self.compare);
        let (local, remote) = match state.on.as_mut() {
            Some(on) => {
                let local = on.statuses_at(ActivePane::ThisPc, &local);
                let remote = if shown_site.as_ref() == Some(&on.site.id) {
                    on.statuses_at(ActivePane::Cloud, &remote)
                } else {
                    None
                };
                (local, remote)
            }
            None => (None, None),
        };
        drop(state);
        mark(&ui.get_local_rows(), local.as_deref());
        mark(&ui.get_remote_rows(), remote.as_deref());
    }

    /// Compares again each time a batch on the compared site (`site`, by name, as the engine
    /// names batches) finishes after `seen`. Runs until the compare is dropped.
    async fn watch_batches(self: Arc<Self>, site: String, mut seen: Option<DateTime<Utc>>) {
        let engine = self.transfers.engine();
        let mut changes = engine.changes();
        while changes.changed().await.is_ok() {
            changes.borrow_and_update();
            let last = engine.snapshot().last_batch;
            let finished = last.as_ref().map(|batch| batch.finished_at);
            if finished != seen {
                seen = finished;
                if last.is_some_and(|batch| batch.site == site) {
                    let this = self.clone();
                    let _ = self
                        .link
                        .ui()
                        .upgrade_in_event_loop(move |ui| this.compare_again(&ui));
                }
            }
            tokio::time::sleep(WATCH_EVERY).await;
        }
    }
}

/// Wires the compare callbacks. Runs at startup: must not start anything.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    PUBLISHER.with(|cell| *cell.borrow_mut() = Arc::downgrade(p));
    let weak = ui.as_weak();
    let p = p.clone();
    ui.on_toggle_compare(move || {
        if let Some(ui) = weak.upgrade() {
            p.toggle_compare(&ui);
        }
    });
}
