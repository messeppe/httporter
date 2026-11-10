// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Publish sheet, Review Deletions, running a publish and the sidebar's profiles (T7).
//!
//! Opening the sheet shows it at once, comparing (with Cancel), and fills it in when the compare
//! of the profile's folders ends. The plan is kept here, with what each row of the plan list
//! stands for; the checkboxes, Remove extras and the purge choice change it and the sheet is drawn
//! again. Publish closes the sheet and runs the plan on the runtime: the CDN key is asked for
//! first when the purge needs it, the drawer shows the batches, the status line the phase, and the
//! run ends in the history.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use httporter_core::{
    CdnZone, CompareStatus as Status, FileOutcome, HistoryFile, HistorySource, ProfileId,
    ProfileRecord, ProviderKind, PurgeMode, SiteRecord,
};
use httporter_platform::win;
use httporter_transfer::{BatchTarget, CompareResult, PublishPhase, PublishPlan, ScanProgress};
use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use tokio::task::AbortHandle;

use super::{Publisher, copy, flow, lock};
use crate::copy as app_copy;
use crate::format::format_size;
use crate::viewmodel::{kind_for, slint_kind};
use crate::{AppWindow, FileKind, PlanGroup, PlanRow, PublishSheetData};

/// The compare's progress line is redrawn at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// The sidebar's "2h ago" is brought up to date this often.
const AGO_EVERY: Duration = Duration::from_secs(60);

/// How often the toolbar's Publish looks whether a profile page is shown.
const PAGE_POLL: Duration = Duration::from_millis(250);

/// The plan filter waits this long after the last key before it applies.
const FILTER_WAIT: Duration = Duration::from_millis(200);

/// What the sheet remembers between callbacks.
#[derive(Default)]
pub(super) struct SheetState {
    /// Goes up with every sheet opened (and every compare again), so that what a task finds late
    /// for an earlier one is dropped.
    run: u64,
    /// The sheet on screen.
    open: Option<Sheet>,
    /// The compare under way, stopped when the sheet is cancelled.
    comparing: Option<AbortHandle>,
    /// A folder or file picker of the sheet is open (they have no owner window).
    picking: bool,
    /// Goes up with every key typed in the plan filter; a wait that finds it moved is dropped.
    filter_typed: u64,
}

/// The sheet on screen.
struct Sheet {
    run: u64,
    profile: ProfileRecord,
    site: SiteRecord,
    /// Where the batches go, once the site is reached.
    target: Option<BatchTarget>,
    destination: String,
    parallel: u32,
    subtitle: String,
    progress: String,
    error: String,
    /// The plan, once compared.
    plan: Option<PublishPlan>,
    /// The files of the compare that look like secrets.
    secrets: Vec<String>,
    remove_extras: bool,
    purge: PurgeMode,
    /// The purge was made Whole zone because changed files would be this many URLs.
    zone_because: Option<usize>,
    /// What each row of the plan list stands for.
    rows: Vec<RowRef>,
    /// The plan list as folders (Tree) rather than paths (Flat).
    tree: bool,
    /// Only the paths that hold this (any case) are listed.
    filter: String,
    /// The tree's folders closed, as `<group>/<folder>/`.
    collapsed: HashSet<String>,
    /// The tree's folder rows, which [`RowRef::Folder`] points into: the group and the folder's
    /// path with its trailing `/`.
    folders: Vec<(Group, String)>,
}

/// A group of the plan list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Changed,
    New,
    Remote,
}

impl Group {
    const ALL: [Group; 3] = [Group::Changed, Group::New, Group::Remote];

    fn of(status: Status) -> Option<Self> {
        match status {
            Status::Changed => Some(Group::Changed),
            Status::New => Some(Group::New),
            Status::RemoteOnly => Some(Group::Remote),
            Status::Same => None,
        }
    }
}

/// What a row of the plan list stands for: a group header, the plan item at that index, or a
/// folder of the tree (an index into `Sheet::folders`).
#[derive(Debug, Clone, Copy)]
enum RowRef {
    Header(Group),
    Item(usize),
    Folder(usize),
}

/// The key of a tree folder in `Sheet::collapsed`.
fn folder_key(group: Group, prefix: &str) -> String {
    format!("{group:?}/{prefix}")
}

/// What a tree folder holds: files, how many are ticked, and their bytes.
#[derive(Default, Clone, Copy)]
struct FolderCount {
    files: u64,
    included: u64,
    bytes: u64,
}

/// What the site is called next to "only on": Bunny, or an S3 site's own name.
fn service(site: &SiteRecord) -> &str {
    match site.provider {
        ProviderKind::Bunny => app_copy::BUNNY,
        ProviderKind::S3 => &site.name,
    }
}

/// A count as the window's `int`.
fn int(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

impl Sheet {
    fn new(run: u64, profile: ProfileRecord, site: SiteRecord, parallel: u32) -> Self {
        let purge = flow::purge_for_site(&site, profile.purge);
        Self {
            run,
            target: None,
            destination: app_copy::chip(&site.host),
            parallel,
            subtitle: copy::sheet_comparing(&site.name),
            progress: copy::sheet_connecting(&site.name),
            error: String::new(),
            plan: None,
            secrets: Vec::new(),
            remove_extras: profile.remove_extras,
            purge,
            zone_because: None,
            rows: Vec::new(),
            tree: false,
            filter: String::new(),
            collapsed: HashSet::new(),
            folders: Vec::new(),
            profile,
            site,
        }
    }

    /// The plan items of `group` the filter lets through, in plan order.
    fn members(&self, plan: &PublishPlan, group: Group) -> Vec<usize> {
        let filter = self.filter.trim().to_lowercase();
        (0..plan.items.len())
            .filter(|&i| Group::of(plan.items[i].status) == Some(group))
            .filter(|&i| filter.is_empty() || plan.items[i].rel.to_lowercase().contains(&filter))
            .collect()
    }

    /// The plan list row of item `i` of `group`: its path (Flat) or name (Tree) at `depth`.
    fn item_row(plan: &PublishPlan, i: usize, group: Group, depth: i32, tree: bool) -> PlanRow {
        let item = &plan.items[i];
        let (group, label) = match group {
            Group::Changed => (PlanGroup::Changed, copy::ITEM_CHANGED),
            Group::New => (PlanGroup::New, copy::ITEM_NEW),
            Group::Remote if item.include => (PlanGroup::Deleted, copy::WILL_BE_DELETED),
            Group::Remote => (PlanGroup::Kept, copy::WILL_BE_KEPT),
        };
        let name = item.rel.rsplit('/').next().unwrap_or(&item.rel);
        PlanRow {
            path: if tree { name } else { item.rel.as_str() }.into(),
            kind: slint_kind(kind_for(name, false)),
            group,
            label: label.into(),
            size: format_size(item.size).into(),
            checked: item.include,
            header: false,
            depth,
            folder: false,
            expanded: false,
        }
    }

    /// The tree rows of `members` of `group`: each folder (with what it holds) before what is in
    /// it, the contents of closed folders left out. Appends to `rows` and `self.rows`.
    fn tree_rows(
        &mut self,
        plan: &PublishPlan,
        group: Group,
        header_group: PlanGroup,
        members: &[usize],
        rows: &mut Vec<PlanRow>,
    ) {
        let mut sorted = members.to_vec();
        sorted.sort_by(|&a, &b| plan.items[a].rel.cmp(&plan.items[b].rel));
        // Every folder's totals, by its path with a trailing `/`.
        let mut counts: BTreeMap<String, FolderCount> = BTreeMap::new();
        for &i in &sorted {
            let item = &plan.items[i];
            let mut at = 0;
            while let Some(slash) = item.rel[at..].find('/') {
                at += slash + 1;
                let count = counts.entry(item.rel[..at].to_owned()).or_default();
                count.files += 1;
                count.included += u64::from(item.include);
                count.bytes += item.size;
            }
        }
        // The folders of the item before, outermost first.
        let mut open: Vec<String> = Vec::new();
        for &i in &sorted {
            let rel = plan.items[i].rel.clone();
            let dirs: Vec<String> = rel
                .match_indices('/')
                .map(|(at, _)| rel[..=at].to_owned())
                .collect();
            let same = open.iter().zip(&dirs).take_while(|(a, b)| a == b).count();
            open.truncate(same);
            for dir in &dirs[same..] {
                open.push(dir.clone());
                let depth = open.len() as i32;
                // Inside a closed folder: not shown.
                let hidden = open[..open.len() - 1]
                    .iter()
                    .any(|outer| self.collapsed.contains(&folder_key(group, outer)));
                if hidden {
                    continue;
                }
                let count = counts.get(dir).copied().unwrap_or_default();
                let name = dir.trim_end_matches('/').rsplit('/').next().unwrap_or(dir);
                let expanded = !self.collapsed.contains(&folder_key(group, dir));
                rows.push(PlanRow {
                    path: name.into(),
                    kind: FileKind::Folder,
                    group: header_group,
                    label: copy::group_count(count.included, count.files).into(),
                    size: format_size(count.bytes).into(),
                    checked: count.included == count.files,
                    header: false,
                    depth,
                    folder: true,
                    expanded,
                });
                self.folders.push((group, dir.clone()));
                self.rows.push(RowRef::Folder(self.folders.len() - 1));
            }
            let hidden = open
                .iter()
                .any(|outer| self.collapsed.contains(&folder_key(group, outer)));
            if !hidden {
                rows.push(Self::item_row(plan, i, group, open.len() as i32 + 1, true));
                self.rows.push(RowRef::Item(i));
            }
        }
    }

    /// The compare ended: the plan, and the purge it calls for. `zones` are the site's pull
    /// zones when it has a CDN link.
    fn compared(&mut self, result: CompareResult, zones: Option<Vec<CdnZone>>) {
        let by_date = result.items.iter().any(|item| item.by_date);
        let by = copy::compared_by(result.weakest_method(), by_date);
        self.subtitle = copy::sheet_compared(result.items.len() as u64, by);
        let plan = PublishPlan::from_compare(&result, self.remove_extras);
        self.secrets = result.secrets;
        // Purging changed files one URL at a time is slow past the limit: the whole zone instead.
        // With no link yet, the count is for one host.
        if let Some(urls) = flow::zone_instead(&self.site, self.purge, zones, &plan) {
            self.purge = PurgeMode::Zone;
            self.zone_because = Some(urls);
        }
        self.plan = Some(plan);
    }

    fn purge_note(&self) -> String {
        if self.site.provider != ProviderKind::Bunny {
            copy::PURGE_NEEDS_BUNNY.to_owned()
        } else if let (PurgeMode::Zone, Some(urls)) = (self.purge, self.zone_because) {
            copy::zone_is_faster(urls)
        } else {
            String::new()
        }
    }

    /// The delete guard trips for the plan as it is now.
    fn needs_review(&self, plan: &PublishPlan) -> bool {
        flow::guard_trips(&self.profile, plan)
    }

    /// What the sheet shows, and the plan list's rows (remembering what each stands for).
    fn render(&mut self) -> (PublishSheetData, Vec<PlanRow>) {
        let service = service(&self.site).to_owned();
        let mut data = PublishSheetData {
            title: copy::sheet_title(&self.site.name).into(),
            subtitle: self.subtitle.as_str().into(),
            folder: self.profile.local_dir.display().to_string().into(),
            destination: self.destination.as_str().into(),
            destination_note: copy::destination_note(self.parallel).into(),
            busy: self.plan.is_none() && self.error.is_empty(),
            progress: self.progress.as_str().into(),
            error: self.error.as_str().into(),
            remove_extras: self.remove_extras,
            remove_label: copy::remove_label(&service).into(),
            remote_label: copy::sheet_remote_label(&service).into(),
            purge: match self.purge {
                PurgeMode::Off => 0,
                PurgeMode::Changed => 1,
                PurgeMode::Zone => 2,
            },
            purge_enabled: self.site.provider == ProviderKind::Bunny,
            purge_note: self.purge_note().into(),
            button_label: copy::PUBLISH.into(),
            review_name: self.site.container.as_str().into(),
            ..Default::default()
        };
        self.rows.clear();
        self.folders.clear();
        let mut rows = Vec::new();
        let Some(plan) = self.plan.take() else {
            return (data, rows);
        };
        self.render_plan(&plan, &service, &mut data, &mut rows);
        self.plan = Some(plan);
        (data, rows)
    }

    /// [`Self::render`] once there is a plan.
    fn render_plan(
        &mut self,
        plan: &PublishPlan,
        service: &str,
        data: &mut PublishSheetData,
        rows: &mut Vec<PlanRow>,
    ) {
        let count = |status: Status| plan.items.iter().filter(|i| i.status == status).count();
        data.changed = int(count(Status::Changed) as u64);
        data.new = int(count(Status::New) as u64);
        data.remote_only = int(count(Status::RemoteOnly) as u64);
        data.same = int(plan.same);

        for group in Group::ALL {
            let members = self.members(plan, group);
            if members.is_empty() {
                continue;
            }
            let included = members.iter().filter(|&&i| plan.items[i].include).count() as u64;
            let all = included == members.len() as u64;
            let bytes: u64 = members.iter().map(|&i| plan.items[i].size).sum();
            let (title, header_group) = match group {
                Group::Changed => (copy::GROUP_CHANGED.to_owned(), PlanGroup::Changed),
                Group::New => (copy::GROUP_NEW.to_owned(), PlanGroup::New),
                Group::Remote if all => (copy::group_remote(service), PlanGroup::Deleted),
                Group::Remote => (copy::group_remote(service), PlanGroup::Kept),
            };
            rows.push(PlanRow {
                path: title.into(),
                kind: FileKind::Folder,
                group: header_group,
                label: copy::group_count(included, members.len() as u64).into(),
                size: format_size(bytes).into(),
                checked: all,
                header: true,
                depth: 0,
                folder: false,
                expanded: false,
            });
            self.rows.push(RowRef::Header(group));
            if self.tree {
                self.tree_rows(plan, group, header_group, &members, rows);
            } else {
                for i in members {
                    rows.push(Self::item_row(plan, i, group, 0, false));
                    self.rows.push(RowRef::Item(i));
                }
            }
        }

        let (files, bytes) = plan.upload_totals();
        let deletes = plan.delete_count();
        let total = files + deletes;
        data.upload_text = copy::upload_text(files, bytes, deletes).into();
        data.deletes = int(deletes);
        data.needs_review = self.needs_review(plan);
        data.button_label = if data.needs_review {
            copy::REVIEW_DELETIONS.into()
        } else if total == 0 {
            copy::PUBLISH.into()
        } else {
            copy::publish_button(total).into()
        };
        data.can_publish = total > 0 && self.target.is_some();
        // Only the secrets that would go up.
        let uploaded = flow::secrets_uploaded(&self.secrets, plan);
        if !uploaded.is_empty() {
            data.warning = copy::secrets_warning(&uploaded).into();
        }
    }
}

/// Shows `data` and `rows` in the sheet. Rows that changed are replaced in place, so the list
/// keeps where it is scrolled to when a checkbox is ticked.
fn apply(ui: &AppWindow, data: PublishSheetData, rows: Vec<PlanRow>) {
    ui.set_publish_sheet(data);
    let model = ui.get_plan_rows();
    if !rows.is_empty() && model.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    } else {
        ui.set_plan_rows(ModelRc::new(VecModel::from(rows)));
    }
}

/// `name` with the characters a Windows file name can't hold replaced.
fn safe_file_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect()
}

impl Publisher {
    // ---- The sidebar and the profile page ----

    /// The profiles, as stored (the sidebar's order).
    async fn read_profiles(&self) -> Vec<ProfileRecord> {
        self.session.profiles().await
    }

    /// The profile publish `history` belongs to.
    pub(super) async fn profile_of_history(
        self: &Arc<Self>,
        history: i64,
    ) -> Option<ProfileRecord> {
        for profile in self.read_profiles().await {
            let records = self
                .session
                .history(profile.id.clone(), String::new())
                .await;
            if records.iter().any(|record| record.id == history) {
                return Some(profile);
            }
        }
        None
    }

    // ---- The sheet ----

    /// Opens the sheet for `profile` of `site`, comparing; any sheet before it is dropped.
    fn start(self: &Arc<Self>, profile: ProfileRecord, site: SiteRecord) {
        let parallel = self.session.settings().simultaneous;
        let run = {
            let mut sheet = lock(&self.sheet);
            if let Some(task) = sheet.comparing.take() {
                task.abort();
            }
            sheet.run += 1;
            let run = sheet.run;
            sheet.open = Some(Sheet::new(run, profile, site, parallel));
            run
        };
        self.show(run, true);
        let this = self.clone();
        let task = self.link.rt().spawn(async move { this.compare(run).await });
        let mut sheet = lock(&self.sheet);
        if sheet.run == run {
            sheet.comparing = Some(task.abort_handle());
        } else {
            task.abort();
        }
    }

    /// Draws the sheet of `run` (and opens it, with `open`), unless another took its place.
    fn show(self: &Arc<Self>, run: u64, open: bool) {
        let this = self.clone();
        let _ = self.link.ui().upgrade_in_event_loop(move |ui| {
            let drawn = match lock(&this.sheet).open.as_mut() {
                Some(sheet) if sheet.run == run => Some(sheet.render()),
                _ => None,
            };
            if let Some((data, rows)) = drawn {
                apply(&ui, data, rows);
                if open {
                    ui.set_publish_open(true);
                }
            }
        });
    }

    /// Draws the sheet again (on the UI thread, after a callback changed it).
    fn redraw(&self, ui: &AppWindow) {
        let drawn = lock(&self.sheet).open.as_mut().map(Sheet::render);
        if let Some((data, rows)) = drawn {
            apply(ui, data, rows);
        }
    }

    /// Changes the sheet of `run` with `change` and draws it again; nothing if it is gone.
    fn edit_run(self: &Arc<Self>, run: u64, change: impl FnOnce(&mut Sheet)) {
        {
            let mut sheet = lock(&self.sheet);
            match sheet.open.as_mut() {
                Some(open) if open.run == run => change(open),
                _ => return,
            }
        }
        self.show(run, false);
    }

    /// The profile and site of the sheet of `run`, while it is on screen.
    fn sheet_of(&self, run: u64) -> Option<(ProfileRecord, SiteRecord)> {
        match lock(&self.sheet).open.as_ref() {
            Some(sheet) if sheet.run == run => Some((sheet.profile.clone(), sheet.site.clone())),
            _ => None,
        }
    }

    /// Reaches the site, compares the profile's folders and fills the sheet of `run` in.
    async fn compare(self: Arc<Self>, run: u64) {
        let Some((profile, site)) = self.sheet_of(run) else {
            return;
        };
        let Some(target) = self.transfers.batch_target(&site).await else {
            let text = copy::no_connection(&site.name);
            self.edit_run(run, |sheet| sheet.error = text);
            return;
        };
        let destination = copy::sheet_destination(&target.provider.endpoint(), &profile.remote_dir);
        let provider = target.provider.clone();
        self.edit_run(run, |sheet| {
            sheet.destination = destination;
            sheet.target = Some(target);
        });
        let service = service(&site).to_owned();
        let last = Mutex::new(None::<Instant>);
        let progress = |scan: ScanProgress| {
            let now = Instant::now();
            {
                let mut last = lock(&last);
                if last.is_some_and(|then| now - then < PROGRESS_EVERY) {
                    return;
                }
                *last = Some(now);
            }
            let text = copy::sheet_progress(&scan, &service);
            self.edit_run(run, |sheet| sheet.progress = text);
        };
        let found = flow::compare(
            &self.session,
            &self.hashes,
            &*provider,
            &site,
            &profile,
            &progress,
        )
        .await;
        match found {
            Ok(flow::Compared { result, zones }) => {
                self.edit_run(run, move |sheet| sheet.compared(result, zones));
            }
            Err(failed) => {
                self.edit_run(run, |sheet| sheet.error = failed.text);
            }
        }
        let mut sheet = lock(&self.sheet);
        if sheet.run == run {
            sheet.comparing = None;
        }
    }

    /// A row's checkbox: an item's own, or a header's for its whole group.
    fn toggle(&self, ui: &AppWindow, row: usize) {
        {
            let mut state = lock(&self.sheet);
            let Some(sheet) = state.open.as_mut() else {
                return;
            };
            let Some(plan) = sheet.plan.as_mut() else {
                return;
            };
            match sheet.rows.get(row).copied() {
                Some(RowRef::Item(i)) => {
                    if let Some(item) = plan.items.get_mut(i) {
                        item.include = !item.include;
                    }
                }
                Some(RowRef::Header(group)) => {
                    let filter = sheet.filter.trim().to_lowercase();
                    let shown = |item: &httporter_transfer::PlanItem| {
                        Group::of(item.status) == Some(group)
                            && (filter.is_empty() || item.rel.to_lowercase().contains(&filter))
                    };
                    let all = plan
                        .items
                        .iter()
                        .filter(|item| shown(item))
                        .all(|item| item.include);
                    for item in &mut plan.items {
                        if shown(item) {
                            item.include = !all;
                        }
                    }
                }
                Some(RowRef::Folder(index)) => {
                    let Some((group, prefix)) = sheet.folders.get(index).cloned() else {
                        return;
                    };
                    let filter = sheet.filter.trim().to_lowercase();
                    let inside = |item: &httporter_transfer::PlanItem| {
                        Group::of(item.status) == Some(group)
                            && item.rel.starts_with(&prefix)
                            && (filter.is_empty() || item.rel.to_lowercase().contains(&filter))
                    };
                    let all = plan
                        .items
                        .iter()
                        .filter(|item| inside(item))
                        .all(|item| item.include);
                    for item in &mut plan.items {
                        if inside(item) {
                            item.include = !all;
                        }
                    }
                }
                None => return,
            }
        }
        self.redraw(ui);
    }

    /// Flat (0) or Tree (1).
    fn set_view(&self, ui: &AppWindow, view: i32) {
        if let Some(sheet) = lock(&self.sheet).open.as_mut() {
            sheet.tree = view == 1;
        }
        self.redraw(ui);
    }

    /// A tree folder's disclosure triangle: opens or closes it.
    fn expand(&self, ui: &AppWindow, row: usize) {
        {
            let mut state = lock(&self.sheet);
            let Some(sheet) = state.open.as_mut() else {
                return;
            };
            let Some(RowRef::Folder(index)) = sheet.rows.get(row).copied() else {
                return;
            };
            let Some((group, prefix)) = sheet.folders.get(index) else {
                return;
            };
            let key = folder_key(*group, prefix);
            if !sheet.collapsed.remove(&key) {
                sheet.collapsed.insert(key);
            }
        }
        self.redraw(ui);
    }

    /// The plan filter as typed: applied once typing pauses for [`FILTER_WAIT`].
    fn filter_typed(self: &Arc<Self>, text: String) {
        let typed = {
            let mut state = lock(&self.sheet);
            state.filter_typed += 1;
            state.filter_typed
        };
        let this = self.clone();
        self.link.rt().spawn(async move {
            tokio::time::sleep(FILTER_WAIT).await;
            let ui = this.link.ui().clone();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                {
                    let mut state = lock(&this.sheet);
                    if state.filter_typed != typed {
                        return;
                    }
                    let Some(sheet) = state.open.as_mut() else {
                        return;
                    };
                    sheet.filter = text;
                }
                this.redraw(&ui);
            });
        });
    }

    fn set_remove_extras(&self, ui: &AppWindow, on: bool) {
        if let Some(sheet) = lock(&self.sheet).open.as_mut() {
            sheet.remove_extras = on;
            if let Some(plan) = sheet.plan.as_mut() {
                plan.set_remove_extras(on);
            }
        }
        self.redraw(ui);
    }

    fn set_purge(&self, ui: &AppWindow, choice: i32) {
        if let Some(sheet) = lock(&self.sheet).open.as_mut()
            && sheet.site.provider == ProviderKind::Bunny
        {
            sheet.purge = match choice {
                0 => PurgeMode::Off,
                2 => PurgeMode::Zone,
                _ => PurgeMode::Changed,
            };
        }
        self.redraw(ui);
    }

    /// Cancel: the sheet (closed already) and its compare are dropped.
    fn cancel_sheet(&self, ui: &AppWindow) {
        let mut sheet = lock(&self.sheet);
        sheet.open = None;
        if let Some(task) = sheet.comparing.take() {
            task.abort();
        }
        drop(sheet);
        ui.set_plan_rows(ModelRc::default());
    }

    /// Publish (or Delete N Files and Publish, with the name `typed`): the sheet is closed
    /// already; its plan runs.
    fn confirm(self: &Arc<Self>, ui: &AppWindow, typed: Option<&str>) {
        let open = {
            let mut sheet = lock(&self.sheet);
            if let Some(task) = sheet.comparing.take() {
                task.abort();
            }
            sheet.open.take()
        };
        ui.set_plan_rows(ModelRc::default());
        let Some(sheet) = open else {
            return;
        };
        let guarded = sheet
            .plan
            .as_ref()
            .is_some_and(|plan| sheet.needs_review(plan));
        let Sheet {
            profile,
            site,
            target,
            plan,
            purge,
            ..
        } = sheet;
        let (Some(target), Some(plan)) = (target, plan) else {
            return;
        };
        match typed {
            // The sheet sends a publish the guard stops through Review Deletions.
            None if guarded => return,
            Some(name) if name != site.container => {
                self.link.status(copy::REVIEW_NAME_WRONG.to_owned());
                return;
            }
            _ => {}
        }
        let this = self.clone();
        self.link.rt().spawn(async move {
            this.run_plan(profile, site, target, plan, purge, false)
                .await;
        });
    }

    /// Change Folder…: the profile is saved with the folder chosen, and compared again.
    fn change_folder(self: &Arc<Self>) {
        let run = {
            let mut sheet = lock(&self.sheet);
            if sheet.picking {
                return;
            }
            let Some(run) = sheet.open.as_ref().map(|open| open.run) else {
                return;
            };
            sheet.picking = true;
            run
        };
        let this = self.clone();
        self.link.rt().spawn_blocking(move || {
            let dir = win::pick_folder(copy::PICK_PUBLISH_FOLDER);
            lock(&this.sheet).picking = false;
            if let Some(dir) = dir {
                let rt = this.link.rt().clone();
                rt.spawn(async move { this.folder_changed(run, dir).await });
            }
        });
    }

    async fn folder_changed(self: Arc<Self>, run: u64, dir: PathBuf) {
        let Some((mut profile, site)) = self.sheet_of(run) else {
            return;
        };
        profile.local_dir = dir;
        if let Err(e) = self.session.save_profile(profile.clone()).await {
            self.link.status(copy::folder_not_saved(&e));
            return;
        }
        super::profile_view::refresh(&self);
        if self.sheet_of(run).is_some() {
            self.start(profile, site);
        }
    }

    /// Export Plan…: the plan as text, in a file the user picks.
    fn export(self: &Arc<Self>) {
        let (text, name) = {
            let mut state = lock(&self.sheet);
            if state.picking {
                return;
            }
            let found = state.open.as_ref().and_then(|sheet| {
                let plan = sheet.plan.as_ref()?;
                let title = copy::sheet_title(&sheet.site.name);
                Some((
                    plan.export_text(&title, &sheet.destination),
                    copy::export_file_name(&safe_file_name(&sheet.profile.name)),
                ))
            });
            let Some(found) = found else {
                return;
            };
            state.picking = true;
            found
        };
        let this = self.clone();
        self.link.rt().spawn_blocking(move || {
            let filters: &[(&str, &[&str])] = &[(copy::EXPORT_FILTER, &["txt"])];
            let path = win::pick_save_file(copy::EXPORT_TITLE, &name, filters);
            lock(&this.sheet).picking = false;
            let Some(path) = path else {
                return;
            };
            let message = match std::fs::write(&path, text) {
                Ok(()) => copy::plan_saved(&path),
                Err(e) => copy::plan_not_saved(e),
            };
            this.link.status(message);
        });
    }

    // ---- Running ----

    /// Runs `plan` for `profile`: its publish lock first (another publish of the profile, from
    /// the command line, a schedule or the watch folder, makes it say so and stop), then the
    /// purger (asking for the key when needed; without one the publish goes on unpurged), then
    /// [`flow::execute`], then the history, the sidebar, the profile page and the result in the
    /// status line. A `retry` (of a publish's failed files) keeps the profile's last "On Bunny now"
    /// counts: its plan only knows the files retried.
    async fn run_plan(
        self: Arc<Self>,
        profile: ProfileRecord,
        site: SiteRecord,
        target: BatchTarget,
        plan: PublishPlan,
        purge: PurgeMode,
        retry: bool,
    ) {
        let Some(_lock) = flow::lock_profile(&profile.id, Duration::ZERO).await else {
            self.link.status(copy::publish_busy(&profile.name));
            return;
        };
        let purger = if purge == PurgeMode::Off {
            Ok(None)
        } else {
            super::cdn::purger_or_key(&self, &site).await.map(Some)
        };
        let link = self.link.clone();
        let name = site.name.clone();
        let on_phase = move |phase: PublishPhase| link.status(copy::phase(&name, phase));
        let run = flow::Run {
            profile,
            site,
            target,
            plan,
            purge,
            source: HistorySource::App,
        };
        let engine = self.transfers.engine();
        let mut record = match flow::execute(engine, &self.session, &run, purger, &on_phase).await {
            Ok(record) => record,
            Err(text) => {
                self.link.status(text);
                return;
            }
        };
        if retry {
            let history = self
                .session
                .history(run.profile.id.clone(), String::new())
                .await;
            if let Some(last) = history.first() {
                record.remote_files = last.remote_files;
                record.remote_folders = last.remote_folders;
                record.regions = last.regions;
            }
        }
        let (message, saved) = flow::record(&self.session, &run, record).await;
        self.link
            .refresh_remote_at(&run.site.id, &run.profile.remote_dir);
        super::profile_view::refresh(&self);
        self.link.status(match saved {
            Ok(_) => message,
            Err(e) => copy::history_not_saved(&e),
        });
    }
}

/// Finds profile `id` (read again, so an edit made elsewhere counts), then opens its sheet.
pub(super) fn open(p: &Arc<Publisher>, id: ProfileId) {
    let this = p.clone();
    p.link.rt().spawn(async move {
        let found = this
            .read_profiles()
            .await
            .into_iter()
            .find(|profile| profile.id == id);
        let Some(profile) = found else {
            this.link.status(copy::PROFILE_GONE.to_owned());
            return;
        };
        let Some(site) = this.session.site(&profile.site) else {
            this.link.status(copy::PROFILE_SITE_GONE.to_owned());
            return;
        };
        this.start(profile, site);
    });
}

/// The toolbar's Publish and Ctrl+P: the profile of the page shown; else the open site's profile
/// that published last; else a new profile for the open site, with the two panes' folders.
pub(super) fn publish_current(p: &Arc<Publisher>) {
    let Some(ui) = p.link.ui().upgrade() else {
        return;
    };
    let shown = usize::try_from(ui.get_selected_profile())
        .ok()
        .and_then(|row| ui.get_profiles().row_data(row));
    if let Some(item) = shown {
        p.open_sheet(ProfileId(item.id.to_string()));
        return;
    }
    let Some(place) = p.session.remote_place() else {
        p.link.status(copy::OPEN_A_SITE.to_owned());
        return;
    };
    let local = p.session.local_dir().unwrap_or_default();
    let this = p.clone();
    p.link.rt().spawn(async move {
        let last = this
            .read_profiles()
            .await
            .into_iter()
            .filter(|profile| profile.site == place.site.id)
            .max_by_key(|profile| profile.last_published_at);
        match last {
            Some(profile) => this.open_sheet(profile.id),
            None => {
                let _ = this.link.ui().upgrade_in_event_loop(move |ui| {
                    super::profile_view::open_new(&ui, &place.site, local, place.dir);
                });
            }
        }
    });
}

/// Publishes again the files of publish `history` that failed, were cancelled or didn't run.
pub(super) fn retry_failed(p: &Arc<Publisher>, history: i64) {
    let this = p.clone();
    p.link.rt().spawn(async move {
        let Some(profile) = this.profile_of_history(history).await else {
            this.link.status(copy::HISTORY_GONE.to_owned());
            return;
        };
        let Some(site) = this.session.site(&profile.site) else {
            this.link.status(copy::PROFILE_SITE_GONE.to_owned());
            return;
        };
        let failed: Vec<HistoryFile> = this
            .session
            .history_files(history)
            .await
            .into_iter()
            .filter(|file| file.outcome != FileOutcome::Done)
            .collect();
        if failed.is_empty() {
            this.link.status(copy::NOTHING_TO_RETRY.to_owned());
            return;
        }
        let plan = PublishPlan::retry_of(
            profile.local_dir.clone(),
            profile.remote_dir.clone(),
            &failed,
        );
        let Some(target) = this.transfers.batch_target(&site).await else {
            this.link.status(copy::no_connection(&site.name));
            return;
        };
        let purge = flow::purge_for_site(&site, profile.purge);
        this.run_plan(profile, site, target, plan, purge, true)
            .await;
    });
}

/// Wires the Publish sheet, CDN key sheet, delete-alert purge and sidebar callbacks. Runs at
/// startup: it only reads the profiles for the sidebar, on a task of its own.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    // On a profile page the toolbar's Publish and Ctrl+P publish that profile, a site open or not
    // (the compare view sets `can-publish` for the panes). The window has no hook for the page
    // being chosen, so this looks a few times a second; the timer lives as long as the window's
    // Publish callback.
    let page_publish = Timer::default();
    page_publish.start(TimerMode::Repeated, PAGE_POLL, {
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                let on_page = usize::try_from(ui.get_selected_profile())
                    .is_ok_and(|row| row < ui.get_profiles().row_count());
                if on_page && !ui.get_can_publish() {
                    ui.set_can_publish(true);
                }
            }
        }
    });
    ui.on_publish({
        let p = p.clone();
        move || {
            let _alive = &page_publish;
            p.publish_current();
        }
    });
    ui.on_plan_toggle({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |row| {
            if let (Some(ui), Ok(row)) = (ui.upgrade(), usize::try_from(row)) {
                p.toggle(&ui, row);
            }
        }
    });
    ui.on_plan_view({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |view| {
            if let Some(ui) = ui.upgrade() {
                p.set_view(&ui, view);
            }
        }
    });
    ui.on_plan_expand({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |row| {
            if let (Some(ui), Ok(row)) = (ui.upgrade(), usize::try_from(row)) {
                p.expand(&ui, row);
            }
        }
    });
    ui.on_plan_filter_changed({
        let p = p.clone();
        move |text| p.filter_typed(text.to_string())
    });
    ui.on_publish_remove_extras({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |on| {
            if let Some(ui) = ui.upgrade() {
                p.set_remove_extras(&ui, on);
            }
        }
    });
    ui.on_publish_purge({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |choice| {
            if let Some(ui) = ui.upgrade() {
                p.set_purge(&ui, choice);
            }
        }
    });
    ui.on_publish_change_folder({
        let p = p.clone();
        move || p.change_folder()
    });
    ui.on_publish_export({
        let p = p.clone();
        move || p.export()
    });
    ui.on_publish_cancel({
        let (p, ui) = (p.clone(), ui.as_weak());
        move || {
            if let Some(ui) = ui.upgrade() {
                p.cancel_sheet(&ui);
            }
        }
    });
    ui.on_publish_confirm({
        let (p, ui) = (p.clone(), ui.as_weak());
        move || {
            if let Some(ui) = ui.upgrade() {
                p.confirm(&ui, None);
            }
        }
    });
    ui.on_publish_review_confirm({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |typed| {
            if let Some(ui) = ui.upgrade() {
                p.confirm(&ui, Some(typed.as_str()));
            }
        }
    });
    super::cdn::install(ui, p);

    // The sidebar's profiles, and their "2h ago" kept up to date.
    super::profile_view::refresh_sidebar(p);
    let weak = Arc::downgrade(p);
    p.link.rt().spawn(async move {
        loop {
            tokio::time::sleep(AGO_EVERY).await;
            let Some(p) = weak.upgrade() else {
                break;
            };
            super::profile_view::refresh_sidebar(&p);
        }
    });
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use httporter_core::{CompareBy, DeleteGuard, ProfileExtras, RemotePath, SiteId};
    use httporter_transfer::CompareItem;
    use slint::SharedString;

    use super::*;

    fn site(provider: ProviderKind) -> SiteRecord {
        SiteRecord {
            id: SiteId::new(),
            name: "testzone".to_owned(),
            provider,
            container: "testzone".to_owned(),
            region: "sg".to_owned(),
            host: "testzone.b-cdn.net".to_owned(),
            s3: None,
            created_at: Utc::now(),
        }
    }

    fn profile(site: &SiteRecord, remove_extras: bool) -> ProfileRecord {
        ProfileRecord {
            id: ProfileId::new(),
            site: site.id.clone(),
            name: "Blog".to_owned(),
            local_dir: PathBuf::from("D:\\site"),
            remote_dir: RemotePath::root(),
            skip_rules: Vec::new(),
            remove_extras,
            purge: PurgeMode::Changed,
            guard: DeleteGuard::default(),
            created_at: Utc::now(),
            last_published_at: None,
            cache_control: None,
            extras: ProfileExtras::default(),
        }
    }

    fn item(rel: String, status: Status) -> CompareItem {
        CompareItem {
            rel,
            status,
            local_size: (status != Status::RemoteOnly).then_some(10),
            remote_size: (status != Status::New).then_some(10),
            by: CompareBy::Size,
            by_date: false,
        }
    }

    /// `n` files of `status`, named `<prefix><i>.txt`.
    fn files(n: usize, prefix: &str, status: Status) -> Vec<CompareItem> {
        (0..n)
            .map(|i| item(format!("{prefix}{i}.txt"), status))
            .collect()
    }

    fn compared(items: Vec<CompareItem>) -> CompareResult {
        CompareResult {
            local_root: PathBuf::from("D:\\site"),
            remote_root: RemotePath::root(),
            items,
            remote_folders: 0,
            skipped_local: 0,
            secrets: Vec::new(),
            regions: 1,
        }
    }

    /// The sheet for `profile` once the compare found `items`, as it is drawn.
    fn drawn(
        site: &SiteRecord,
        profile: ProfileRecord,
        items: Vec<CompareItem>,
    ) -> (Sheet, PublishSheetData) {
        let mut sheet = Sheet::new(1, profile, site.clone(), 4);
        sheet.compared(compared(items), None);
        let (data, _) = sheet.render();
        (sheet, data)
    }

    #[test]
    fn the_button_says_how_many_files_or_review_deletions() {
        let bunny = site(ProviderKind::Bunny);

        // While comparing: plain Publish, busy.
        let mut comparing = Sheet::new(1, profile(&bunny, false), bunny.clone(), 4);
        let (data, rows) = comparing.render();
        assert_eq!(data.button_label, copy::PUBLISH);
        assert!(data.busy && !data.can_publish && rows.is_empty());

        let mut items = files(3, "new", Status::New);
        items.extend(files(2, "changed", Status::Changed));
        let (_, data) = drawn(&bunny, profile(&bunny, false), items);
        assert_eq!(data.button_label, "Publish 5 Files");
        assert!(!data.needs_review);

        let (_, data) = drawn(&bunny, profile(&bunny, false), files(1, "a", Status::New));
        assert_eq!(data.button_label, "Publish 1 File");

        let (_, data) = drawn(&bunny, profile(&bunny, false), files(4, "s", Status::Same));
        assert_eq!(data.button_label, copy::PUBLISH);
        assert!(!data.can_publish);
        assert_eq!(data.same, 4);

        // Deleting 20 of 20 remote files trips the guard (more than 10 %).
        let removing = files(20, "old", Status::RemoteOnly);
        let (_, data) = drawn(&bunny, profile(&bunny, true), removing.clone());
        assert_eq!(data.button_label, copy::REVIEW_DELETIONS);
        assert_eq!(data.button_label, "Review Deletions\u{2026}");
        assert!(data.needs_review);
        assert_eq!(data.deletes, 20);
        // Without Remove extras nothing is deleted, and nothing is left to do.
        let (_, data) = drawn(&bunny, profile(&bunny, false), removing);
        assert_eq!(data.button_label, copy::PUBLISH);
        assert!(!data.needs_review);
        assert_eq!(data.remote_only, 20);
        // One delete in a hundred remote files is under the guard: it counts as a file to do.
        let mut items = files(99, "same", Status::Same);
        items.extend(files(1, "old", Status::RemoteOnly));
        let (_, data) = drawn(&bunny, profile(&bunny, true), items);
        assert_eq!(data.button_label, "Publish 1 File");
        assert!(!data.needs_review);
    }

    #[test]
    fn more_than_300_urls_purge_the_whole_zone_instead() {
        let bunny = site(ProviderKind::Bunny);
        let (sheet, data) = drawn(&bunny, profile(&bunny, false), files(300, "f", Status::New));
        assert_eq!(sheet.purge, PurgeMode::Changed);
        assert_eq!(sheet.zone_because, None);
        assert_eq!(data.purge, 1);
        assert_eq!(data.purge_note, "");

        let (sheet, data) = drawn(&bunny, profile(&bunny, false), files(301, "f", Status::New));
        assert_eq!(sheet.purge, PurgeMode::Zone);
        assert_eq!(sheet.zone_because, Some(301));
        assert_eq!(data.purge, 2);
        assert_eq!(data.purge_note, copy::zone_is_faster(301));
        assert_eq!(
            data.purge_note,
            "301 URLs to purge \u{2014} purging the whole zone is faster."
        );

        // A folder's index.html is three URLs: 100 of them are exactly 300, 101 are past it.
        let pages = |n: usize| -> Vec<CompareItem> {
            (0..n)
                .map(|i| item(format!("p{i}/index.html"), Status::Changed))
                .collect()
        };
        let (sheet, _) = drawn(&bunny, profile(&bunny, false), pages(100));
        assert_eq!(sheet.zone_because, None);
        let (sheet, _) = drawn(&bunny, profile(&bunny, false), pages(101));
        assert_eq!(sheet.zone_because, Some(303));

        // A profile that doesn't purge is left alone.
        let mut off = profile(&bunny, false);
        off.purge = PurgeMode::Off;
        let (sheet, _) = drawn(&bunny, off, files(500, "f", Status::New));
        assert_eq!(sheet.purge, PurgeMode::Off);

        // S3 has no CDN to purge.
        let s3 = site(ProviderKind::S3);
        let (sheet, data) = drawn(&s3, profile(&s3, false), files(500, "f", Status::New));
        assert_eq!(sheet.purge, PurgeMode::Off);
        assert_eq!(data.purge, 0);
        assert!(!data.purge_enabled);
        assert_eq!(data.purge_note, copy::PURGE_NEEDS_BUNNY);
    }

    #[test]
    fn the_sheet_shows_what_is_drawn() {
        thread_local! {
            static STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        if !STARTED.with(|started| started.replace(true)) {
            i_slint_backend_testing::init_no_event_loop();
        }
        let ui = AppWindow::new().unwrap();
        let bunny = site(ProviderKind::Bunny);
        let mut sheet = Sheet::new(1, profile(&bunny, true), bunny.clone(), 4);
        sheet.compared(compared(files(20, "old", Status::RemoteOnly)), None);
        let (data, rows) = sheet.render();
        apply(&ui, data, rows);
        let shown = ui.get_publish_sheet();
        assert_eq!(
            shown.button_label,
            SharedString::from("Review Deletions\u{2026}")
        );
        assert!(shown.needs_review);
        // The group's header and its 20 files.
        assert_eq!(ui.get_plan_rows().row_count(), 21);
        let header = ui.get_plan_rows().row_data(0).unwrap();
        assert!(header.header && header.checked);
        assert_eq!(header.group, PlanGroup::Deleted);
    }
}
