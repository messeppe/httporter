//! The Publish sheet, Review Deletions, running a publish and the sidebar's profiles (T7).
//!
//! Opening the sheet shows it at once, comparing (with Cancel), and fills it in when the compare
//! of the profile's folders ends. The plan is kept here, with what each row of the plan list
//! stands for; the checkboxes, Remove extras and the purge choice change it and the sheet is drawn
//! again. Publish closes the sheet and runs the plan on the runtime: the CDN key is asked for
//! first when the purge needs it, the drawer shows the batches, the status line the phase, and the
//! run ends in the history.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use httporter_core::{
    CdnZone, CompareStatus as Status, FileOutcome, HistoryFile, PURGE_URL_LIMIT, ProfileId,
    ProfileRecord, ProviderKind, PurgeMode, PurgeResult, SiteRecord, SkipRules, purge_urls,
};
use httporter_platform::win;
use httporter_transfer::{
    BatchTarget, CompareError, CompareMode, CompareResult, PublishPhase, PublishPlan, ScanProgress,
    compare_trees, run_publish,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::task::AbortHandle;

use super::{Publisher, copy, lock};
use crate::copy::{self as app_copy, Attempt};
use crate::format::format_size;
use crate::viewmodel::{kind_for, slint_kind};
use crate::{AppWindow, FileKind, PlanGroup, PlanRow, ProfileItem, PublishSheetData};

/// The compare's progress line is redrawn at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// The sidebar's "2h ago" is brought up to date this often.
const AGO_EVERY: Duration = Duration::from_secs(60);

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

/// What a row of the plan list stands for: a group header, or the plan item at that index.
#[derive(Debug, Clone, Copy)]
enum RowRef {
    Header(Group),
    Item(usize),
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
        let purge = match site.provider {
            ProviderKind::Bunny => profile.purge,
            ProviderKind::S3 => PurgeMode::Off,
        };
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
            profile,
            site,
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
        if self.purge == PurgeMode::Changed {
            let zones = zones.unwrap_or_else(|| {
                vec![CdnZone {
                    id: 0,
                    name: String::new(),
                    hostnames: vec![self.site.host.clone()],
                }]
            });
            let urls = purge_urls(&zones, &plan.touched_paths()).len();
            if urls > PURGE_URL_LIMIT {
                self.purge = PurgeMode::Zone;
                self.zone_because = Some(urls);
            }
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
        let deletes = plan.delete_count();
        deletes > 0 && self.profile.guard.trips(deletes, plan.remote_files)
    }

    /// What the sheet shows, and the plan list's rows (remembering what each stands for).
    fn render(&mut self) -> (PublishSheetData, Vec<PlanRow>) {
        let service = service(&self.site);
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
            remove_label: copy::remove_label(service).into(),
            remote_label: copy::sheet_remote_label(service).into(),
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
        let mut rows = Vec::new();
        let Some(plan) = &self.plan else {
            return (data, rows);
        };

        let count = |status: Status| plan.items.iter().filter(|i| i.status == status).count();
        data.changed = int(count(Status::Changed) as u64);
        data.new = int(count(Status::New) as u64);
        data.remote_only = int(count(Status::RemoteOnly) as u64);
        data.same = int(plan.same);

        for group in Group::ALL {
            let members: Vec<usize> = (0..plan.items.len())
                .filter(|&i| Group::of(plan.items[i].status) == Some(group))
                .collect();
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
            });
            self.rows.push(RowRef::Header(group));
            for i in members {
                let item = &plan.items[i];
                let (group, label) = match group {
                    Group::Changed => (PlanGroup::Changed, copy::ITEM_CHANGED),
                    Group::New => (PlanGroup::New, copy::ITEM_NEW),
                    Group::Remote if item.include => (PlanGroup::Deleted, copy::WILL_BE_DELETED),
                    Group::Remote => (PlanGroup::Kept, copy::WILL_BE_KEPT),
                };
                let name = item.rel.rsplit('/').next().unwrap_or(&item.rel);
                rows.push(PlanRow {
                    path: item.rel.as_str().into(),
                    kind: slint_kind(kind_for(name, false)),
                    group,
                    label: label.into(),
                    size: format_size(item.size).into(),
                    checked: item.include,
                    header: false,
                });
                self.rows.push(RowRef::Item(i));
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
        let uploaded: Vec<String> = self
            .secrets
            .iter()
            .filter(|rel| {
                plan.items.iter().any(|item| {
                    item.rel == **rel && item.include && item.status != Status::RemoteOnly
                })
            })
            .cloned()
            .collect();
        if !uploaded.is_empty() {
            data.warning = copy::secrets_warning(&uploaded).into();
        }
        (data, rows)
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

/// The sidebar's rows for `profiles`.
fn sidebar_items(profiles: &[ProfileRecord], now: DateTime<Utc>) -> Vec<ProfileItem> {
    profiles
        .iter()
        .map(|profile| ProfileItem {
            id: profile.id.0.as_str().into(),
            name: profile.name.as_str().into(),
            meta: profile
                .last_published_at
                .map_or_else(
                    || copy::SIDEBAR_NEVER.to_owned(),
                    |t| copy::ago_short(t, now),
                )
                .into(),
        })
        .collect()
}

/// Shows `items` in the sidebar. The selected profile stays selected (by id: a publish may move
/// it in the list); one that is gone is unselected, which shows the panes again.
fn apply_profiles(ui: &AppWindow, items: Vec<ProfileItem>) {
    let selected = ui.get_selected_profile();
    let selected_id = usize::try_from(selected)
        .ok()
        .and_then(|row| ui.get_profiles().row_data(row))
        .map(|item| item.id);
    ui.set_profiles(ModelRc::new(VecModel::from(items)));
    if let Some(id) = selected_id {
        let row = ui
            .get_profiles()
            .iter()
            .position(|item| item.id == id)
            .map_or(-1, |row| i32::try_from(row).unwrap_or(-1));
        if row != selected {
            ui.set_selected_profile(row);
        }
    }
}

impl Publisher {
    // ---- The sidebar ----

    /// Reads the profiles and shows them in the sidebar. Read on a task of its own: a store that
    /// fails leaves the list as it was.
    pub(super) async fn reload_profiles(self: &Arc<Self>) {
        let session = self.session.clone();
        let read = self
            .link
            .rt()
            .spawn(async move { session.profiles().await });
        let Ok(profiles) = read.await else {
            return;
        };
        lock(&self.state).profiles = profiles;
        self.show_profiles();
    }

    fn show_profiles(&self) {
        let items = sidebar_items(&lock(&self.state).profiles, Utc::now());
        let _ = self
            .link
            .ui()
            .upgrade_in_event_loop(move |ui| apply_profiles(&ui, items));
    }

    /// The profile page shows the history again (its loader runs for the selected profile).
    pub(super) fn refresh_profile_page(&self) {
        let _ = self.link.ui().upgrade_in_event_loop(|ui| {
            let selected = ui.get_selected_profile();
            if selected >= 0 {
                ui.invoke_profile_selected(selected);
            }
        });
    }

    /// The profile publish `history` belongs to.
    pub(super) async fn profile_of_history(
        self: &Arc<Self>,
        history: i64,
    ) -> Option<ProfileRecord> {
        self.reload_profiles().await;
        let profiles = lock(&self.state).profiles.clone();
        for profile in profiles {
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
        let mode = CompareMode::for_site(site.provider, self.session.settings().verify_checksums);
        let skip = SkipRules::new(&profile.skip_rules);
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
        let found = compare_trees(
            &*provider,
            &profile.local_dir,
            &profile.remote_dir,
            &skip,
            mode,
            &self.hashes,
            &progress,
        )
        .await;
        match found {
            Ok(result) => {
                let zones = match site.provider {
                    ProviderKind::Bunny => self
                        .session
                        .cdn_link(site.id.clone())
                        .await
                        .map(|link| link.zones),
                    ProviderKind::S3 => None,
                };
                self.edit_run(run, move |sheet| sheet.compared(result, zones));
            }
            Err(CompareError::Local { path, message }) => {
                let text = copy::compare_local_failed(&path, &message);
                self.edit_run(run, |sheet| sheet.error = text);
            }
            Err(CompareError::Remote(e)) => {
                let text = app_copy::describe_for(
                    site.provider,
                    &e,
                    &site.container,
                    &site.host,
                    Attempt::List(&profile.remote_dir),
                );
                self.edit_run(run, |sheet| sheet.error = text);
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
                    let all = plan
                        .items
                        .iter()
                        .filter(|item| Group::of(item.status) == Some(group))
                        .all(|item| item.include);
                    for item in &mut plan.items {
                        if Group::of(item.status) == Some(group) {
                            item.include = !all;
                        }
                    }
                }
                None => return,
            }
        }
        self.redraw(ui);
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
            this.run_plan(profile, site, target, plan, purge).await;
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
        self.reload_profiles().await;
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

    /// Runs `plan` for `profile`: the purger first (asking for the key when needed; without one
    /// the publish goes on unpurged), then [`run_publish`], then the history, the sidebar, the
    /// profile page and the result in the status line.
    async fn run_plan(
        self: Arc<Self>,
        profile: ProfileRecord,
        site: SiteRecord,
        target: BatchTarget,
        plan: PublishPlan,
        purge: PurgeMode,
    ) {
        let (purger, unavailable) = if purge == PurgeMode::Off {
            (None, None)
        } else {
            match super::cdn::purger_or_key(&self, &site).await {
                Ok(purger) => (Some(purger), None),
                Err(why) => (None, Some(why)),
            }
        };
        let mode = if purger.is_some() {
            purge
        } else {
            PurgeMode::Off
        };
        let link = self.link.clone();
        let name = site.name.clone();
        let on_phase = move |phase: PublishPhase| link.status(copy::phase(&name, phase));
        let ran = run_publish(
            self.transfers.engine(),
            target,
            &plan,
            mode,
            purger,
            &on_phase,
        )
        .await;
        let mut outcome = match ran {
            Ok(outcome) => outcome,
            Err(e) => {
                self.link.status(copy::publish_not_started(&e.0));
                return;
            }
        };
        if let Some(why) = unavailable {
            outcome.purge = PurgeResult::Unavailable(why);
        }
        let verified = self.session.settings().verify_checksums;
        let record = outcome.into_record(profile.id.clone(), &plan, verified);
        let message = copy::published(&site.name, &record);
        let saved = self.session.add_history(record).await;
        self.link.refresh_remote_at(&site.id, &profile.remote_dir);
        self.reload_profiles().await;
        self.refresh_profile_page();
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
        this.reload_profiles().await;
        let found = lock(&this.state)
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .cloned();
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

/// The toolbar's Publish and Ctrl+P: the selected profile; else the open site's profile that
/// published last; else a new profile for the open site, with the two panes' folders.
pub(super) fn publish_current(p: &Arc<Publisher>) {
    let Some(ui) = p.link.ui().upgrade() else {
        return;
    };
    if let Ok(row) = usize::try_from(ui.get_selected_profile())
        && let Some(profile) = p.profile_at(row)
    {
        p.open_sheet(profile.id);
        return;
    }
    let Some(place) = p.session.remote_place() else {
        p.link.status(copy::OPEN_A_SITE.to_owned());
        return;
    };
    let last = lock(&p.state)
        .profiles
        .iter()
        .filter(|profile| profile.site == place.site.id)
        .max_by_key(|profile| profile.last_published_at)
        .cloned();
    match last {
        Some(profile) => p.open_sheet(profile.id),
        None => super::profile_view::open_new(
            &ui,
            &place.site,
            p.session.local_dir().unwrap_or_default(),
            place.dir.clone(),
        ),
    }
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
        let purge = match site.provider {
            ProviderKind::Bunny => profile.purge,
            ProviderKind::S3 => PurgeMode::Off,
        };
        this.run_plan(profile, site, target, plan, purge).await;
    });
}

/// Wires the Publish sheet, CDN key sheet, delete-alert purge and sidebar callbacks. Runs at
/// startup: it only reads the profiles for the sidebar, on a task of its own.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    ui.on_publish({
        let p = p.clone();
        move || p.publish_current()
    });
    ui.on_plan_toggle({
        let (p, ui) = (p.clone(), ui.as_weak());
        move |row| {
            if let (Some(ui), Ok(row)) = (ui.upgrade(), usize::try_from(row)) {
                p.toggle(&ui, row);
            }
        }
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
    p.refresh_profiles();
    let weak = Arc::downgrade(p);
    p.link.rt().spawn(async move {
        loop {
            tokio::time::sleep(AGO_EVERY).await;
            let Some(p) = weak.upgrade() else {
                break;
            };
            p.show_profiles();
        }
    });
}
