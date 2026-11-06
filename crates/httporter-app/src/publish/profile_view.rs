// SPDX-License-Identifier: MIT OR Apache-2.0

//! The profile page, Edit Profile and the publish history (T8).
//!
//! - **The page** (mockup `History.dc.html`): choosing a profile in the sidebar reads the profile,
//!   its site, what purging that site's CDN would use and the history, off the UI thread, and puts
//!   them on the page ([`load`]). A newer load wins over an older one still on its way. The
//!   history search waits for a 250 ms pause in the typing.
//! - **The detail** under the history: the publish chosen, or at first the newest one that went
//!   wrong. Its summary shows at once, its files once read. Retry and Retry Purge are the
//!   Publisher's.
//! - **The profile sheet**: Edit Profile, and a new profile from Publish ([`open_new`]), which
//!   opens the Publish sheet once saved. Save checks the name, the folder and the numbers; Delete
//!   Profile has asked first (the sheet does that).
//! - **The sidebar's list** of profiles is read again after one is saved or deleted
//!   ([`refresh_sidebar`]; [`refresh`] also reloads the page, for after a publish).
//! - Wired here too, since both need the session: the Settings window's Publish tab (the kept
//!   Bunny account keys) and the profile a `.env` import asks for.
//!
//! No account key reaches the window or a log: the session is only asked whether one is kept.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use httporter_core::{
    AccountId, DeleteGuard, FileOutcome, HISTORY_KEEP, HistoryFile, HistoryRecord, ProfileExtras,
    ProfileId, ProfileRecord, ProviderKind, PublishResult, PurgeMode, PurgeResult, RemotePath,
    SiteRecord, SkipRules,
};
use httporter_platform::win;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, VecModel};

use super::{Publisher, copy};
use crate::add_site::{self, ImportedPublish};
use crate::format::{format_count, format_size};
use crate::session::blocking;
use crate::settings_view;
use crate::{
    AppWindow, HistoryDetail, HistoryRow, ProfileDraft, ProfileItem, ProfileView, SettingsWindow,
};

/// How long the history search waits after the last edit of the search field.
const SEARCH_PAUSE: Duration = Duration::from_millis(250);
/// How often the profile sheet looks whether the CDN key sheet it opened has closed, and for how
/// many looks it waits for that sheet to open at all.
const KEY_SHEET_POLL: Duration = Duration::from_millis(300);
const KEY_SHEET_OPEN_LOOKS: u32 = 20;

/// What the profile page remembers between callbacks.
#[derive(Default)]
pub(super) struct ProfileState {
    /// The profile the page shows.
    shown: Option<ProfileId>,
    /// What the page last read about it, for Edit Profile and Change Folder….
    page: Option<PageFacts>,
    /// Counts the page's loads: only the latest one's result is shown.
    loads: u64,
    /// Counts the search field's edits: the search runs when one is 250 ms old and still the last.
    searches: u64,
    /// The publishes in the history table, newest first.
    history: Vec<HistoryRecord>,
    /// The publish whose detail is shown.
    detail: Option<i64>,
    /// What the profile sheet edits.
    sheet: Option<SheetTarget>,
    /// A folder picker is open (it has no owner window: a second press is ignored).
    picking: bool,
}

/// What the page knows of the profile it shows.
#[derive(Clone)]
struct PageFacts {
    record: ProfileRecord,
    /// `None` once the site is removed.
    site: Option<SiteRecord>,
    cdn: Cdn,
}

/// What the profile sheet edits.
#[derive(Clone)]
struct SheetTarget {
    /// As saved; for a new profile, as it starts.
    record: ProfileRecord,
    site: SiteRecord,
    /// A new profile, from Publish: the Publish sheet opens once it is saved.
    new: bool,
}

/// What purging a site's CDN would use.
#[derive(Clone, Debug)]
enum Cdn {
    /// An S3 site: there is no pull zone.
    S3,
    /// A Bunny site linked to its pull zones, and whether the account key is kept.
    Linked { zones: Vec<u64>, key: bool },
    /// A Bunny site with no link yet, and whether any account key is kept (the publish looks the
    /// pull zone up with it).
    Unlinked { accounts: bool },
}

impl Cdn {
    fn zones(&self) -> &[u64] {
        match self {
            Self::Linked { zones, .. } => zones,
            _ => &[],
        }
    }

    /// The line under the profile sheet's purge choice; empty when the account key is missing,
    /// and the sheet offers Add Account Key… in its place.
    fn note(&self) -> String {
        match self {
            Self::S3 => copy::CDN_NOTE_S3.to_owned(),
            Self::Linked { zones, key: true } => copy::cdn_note(zones),
            Self::Unlinked { accounts: true } => copy::CDN_NOTE_ACCOUNT.to_owned(),
            Self::Linked { key: false, .. } | Self::Unlinked { accounts: false } => String::new(),
        }
    }
}

thread_local! {
    // The Publisher, for `open_new`, which is handed only the window. The UI thread's.
    static PUBLISHER: RefCell<Weak<Publisher>> = const { RefCell::new(Weak::new()) };
}

fn lock(state: &Mutex<ProfileState>) -> MutexGuard<'_, ProfileState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires the profile page and profile sheet callbacks, the Settings window's Publish tab and the
/// profile of a `.env` import. Runs at startup: must not start anything.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    PUBLISHER.with(|slot| *slot.borrow_mut() = Arc::downgrade(p));

    ui.on_profile_selected(with(ui, p, select));
    ui.on_history_search_changed(with(ui, p, |p, ui, _text: SharedString| {
        search_changed(p, ui);
    }));
    ui.on_history_selected(with(ui, p, |p, ui, id: i32| {
        show_detail(p, ui, i64::from(id), false);
    }));
    ui.on_history_show_all(with(ui, p, |p, ui, id: i32| {
        show_detail(p, ui, i64::from(id), true);
    }));
    ui.on_history_retry({
        let p = p.clone();
        move |id| p.retry_failed(i64::from(id))
    });
    ui.on_history_retry_purge({
        let p = p.clone();
        move |id| p.retry_purge(i64::from(id))
    });
    ui.on_profile_publish({
        let p = p.clone();
        move || {
            let shown = lock(&p.profile).shown.clone();
            if let Some(id) = shown {
                p.open_sheet(id);
            }
        }
    });
    ui.on_profile_change_folder(with0(ui, p, change_folder));
    ui.on_edit_profile(with0(ui, p, edit_profile));
    ui.on_profile_save(with(ui, p, save));
    ui.on_profile_delete(with(ui, p, |p, ui, id: SharedString| {
        delete(p, ui, ProfileId(id.to_string()));
    }));
    ui.on_profile_pick_folder(with0(ui, p, pick_sheet_folder));
    ui.on_profile_add_key(with0(ui, p, add_key));

    settings_view::set_key_keeper(key_keeper(p));
    add_site::on_imported_site_saved({
        let p = p.clone();
        move |site, imported| make_imported(&p, site, imported)
    });
}

/// A window callback with one argument: `f` with the Publisher and the window, while there is one.
fn with<A>(
    ui: &AppWindow,
    p: &Arc<Publisher>,
    f: impl Fn(&Arc<Publisher>, &AppWindow, A) + 'static,
) -> impl Fn(A) + 'static {
    let (p, weak) = (p.clone(), ui.as_weak());
    move |arg| {
        if let Some(ui) = weak.upgrade() {
            f(&p, &ui, arg);
        }
    }
}

/// As [`with`], for a callback without arguments.
fn with0(
    ui: &AppWindow,
    p: &Arc<Publisher>,
    f: impl Fn(&Arc<Publisher>, &AppWindow) + 'static,
) -> impl Fn() + 'static {
    let (p, weak) = (p.clone(), ui.as_weak());
    move || {
        if let Some(ui) = weak.upgrade() {
            f(&p, &ui);
        }
    }
}

// ---- The page ----

/// A profile was chosen in the sidebar (`index` into its list): the page shows it.
fn select(p: &Arc<Publisher>, ui: &AppWindow, index: i32) {
    let Some(item) = usize::try_from(index)
        .ok()
        .and_then(|i| ui.get_profiles().row_data(i))
    else {
        return;
    };
    let id = ProfileId(item.id.to_string());
    let other = {
        let mut state = lock(&p.profile);
        let other = state.shown.as_ref() != Some(&id);
        if other {
            state.page = None;
            state.history.clear();
            state.detail = None;
        }
        state.shown = Some(id);
        other
    };
    if other {
        // What the page showed belongs to another profile: only the name, until the load is in.
        ui.set_profile(ProfileView {
            id: item.id.clone(),
            name: item.name.clone(),
            ..ProfileView::default()
        });
        ui.set_history_rows(ModelRc::default());
        ui.set_history_detail(HistoryDetail::default());
    }
    ui.set_history_search(SharedString::new());
    load(p, ui);
}

/// The search field was edited: the history is searched once the typing pauses.
fn search_changed(p: &Arc<Publisher>, ui: &AppWindow) {
    let edit = {
        let mut state = lock(&p.profile);
        state.searches += 1;
        state.searches
    };
    let (p, weak) = (p.clone(), ui.as_weak());
    Timer::single_shot(SEARCH_PAUSE, move || {
        let latest = lock(&p.profile).searches == edit;
        if let (true, Some(ui)) = (latest, weak.upgrade()) {
            load(&p, &ui);
        }
    });
}

/// What the page shows of a profile, read off the UI thread.
struct Page {
    facts: PageFacts,
    folder_found: bool,
    /// Every publish kept, newest first; and those the search finds (all of them without one).
    all: Vec<HistoryRecord>,
    found: Vec<HistoryRecord>,
}

/// Reads the profile the page shows and its history (as the search field filters it), off the UI
/// thread, and puts them on the page. Nothing without a profile shown.
fn load(p: &Arc<Publisher>, ui: &AppWindow) {
    let search = ui.get_history_search().trim().to_owned();
    let (id, serial) = {
        let mut state = lock(&p.profile);
        let Some(id) = state.shown.clone() else {
            return;
        };
        state.loads += 1;
        (id, state.loads)
    };
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let page = read_page(&p2, &id, &search).await;
        let ui = p2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            show_page(&p2, &ui, serial, page, !search.is_empty());
        });
    });
}

async fn read_page(p: &Publisher, id: &ProfileId, search: &str) -> Option<Page> {
    let record = p
        .session
        .profiles()
        .await
        .into_iter()
        .find(|r| r.id == *id)?;
    let site = p.session.site(&record.site);
    let cdn = match &site {
        Some(site) => read_cdn(p, site).await,
        None => Cdn::Unlinked { accounts: false },
    };
    let dir = record.local_dir.clone();
    let folder_found = blocking(move || dir.is_dir()).await;
    let all = p.session.history(id.clone(), String::new()).await;
    let found = if search.is_empty() {
        all.clone()
    } else {
        p.session.history(id.clone(), search.to_owned()).await
    };
    Some(Page {
        facts: PageFacts { record, site, cdn },
        folder_found,
        all,
        found,
    })
}

/// Reads what purging `site`'s CDN would use. A kept key is only asked about, never held.
async fn read_cdn(p: &Publisher, site: &SiteRecord) -> Cdn {
    if site.provider != ProviderKind::Bunny {
        return Cdn::S3;
    }
    match p.session.cdn_link(site.id.clone()).await {
        Some(link) => Cdn::Linked {
            zones: link.zones.iter().map(|zone| zone.id).collect(),
            key: p.session.account_key(link.account).await.is_some(),
        },
        None => Cdn::Unlinked {
            accounts: !p.session.bunny_accounts().await.is_empty(),
        },
    }
}

/// Puts `page` on the page, unless a newer load was asked for since load `serial`. A profile that
/// is no longer there (deleted meanwhile) leaves the page for the panes.
fn show_page(p: &Arc<Publisher>, ui: &AppWindow, serial: u64, page: Option<Page>, searching: bool) {
    let mut state = lock(&p.profile);
    if state.loads != serial {
        return;
    }
    let Some(page) = page else {
        state.shown = None;
        state.page = None;
        drop(state);
        ui.set_selected_profile(-1);
        return;
    };
    let now = Local::now();
    let view = page_view(&page, searching, now);
    // The oldest publish is the first one, unless older ones have been let go.
    let first = (page.all.len() < HISTORY_KEEP)
        .then(|| page.all.last().map(|h| h.id))
        .flatten();
    let rows: Vec<HistoryRow> = page
        .found
        .iter()
        .map(|h| history_row(h, Some(h.id) == first, now))
        .collect();
    // The detail stays on the publish chosen while it is listed; else the newest that went wrong.
    let detail = state
        .detail
        .filter(|id| page.found.iter().any(|h| h.id == *id))
        .or_else(|| page.found.iter().find(|h| went_wrong(h)).map(|h| h.id));
    state.detail = detail;
    state.history = page.found;
    state.page = Some(page.facts);
    drop(state);

    ui.set_profile(view);
    ui.set_history_rows(ModelRc::new(VecModel::from(rows)));
    match detail {
        Some(id) => show_detail(p, ui, id, false),
        None => ui.set_history_detail(HistoryDetail::default()),
    }
}

/// The header, the chips, the cards and the footer.
fn page_view(page: &Page, searching: bool, now: DateTime<Local>) -> ProfileView {
    let PageFacts { record, site, cdn } = &page.facts;
    let newest = page.all.first();
    let chips: Vec<SharedString> = [
        copy::skip_rules_chip(record.skip_rules.len()),
        copy::remove_extras_chip(record.remove_extras),
        copy::purge_chip(record.purge, cdn.zones()),
        copy::guard_chip(&record.guard),
    ]
    .into_iter()
    .map(SharedString::from)
    .collect();
    let status = if site.is_none() {
        copy::SITE_GONE
    } else if !page.folder_found {
        copy::FOLDER_GONE
    } else {
        ""
    };
    ProfileView {
        id: record.id.0.as_str().into(),
        name: record.name.as_str().into(),
        folder: record.local_dir.display().to_string().into(),
        destination: site
            .as_ref()
            .map(|site| copy::destination(site, &record.remote_dir))
            .unwrap_or_default()
            .into(),
        chips: ModelRc::new(VecModel::from(chips)),
        last_title: newest
            .map_or_else(
                || copy::NEVER.to_owned(),
                |h| copy::history_when(h.finished_at, now),
            )
            .into(),
        last_detail: newest
            .map_or_else(|| copy::LAST_NONE.to_owned(), copy::last_detail)
            .into(),
        remote_title: newest
            .map_or_else(
                || copy::REMOTE_UNKNOWN.to_owned(),
                |h| copy::remote_title(h.remote_files),
            )
            .into(),
        remote_label: copy::on_remote_now(site.as_ref()).into(),
        remote_detail: newest
            .map_or_else(
                || copy::REMOTE_UNKNOWN_DETAIL.to_owned(),
                |h| copy::remote_detail(h.remote_folders, h.regions),
            )
            .into(),
        schedule: copy::SCHEDULE_OFF.into(),
        footer: copy::history_footer(page.all.len(), page.found.len(), searching).into(),
        status: status.into(),
    }
}

/// A publish with failed files, or whose purge failed: the row gets a warning.
fn went_wrong(h: &HistoryRecord) -> bool {
    h.failed > 0
        || matches!(h.result, PublishResult::Partial | PublishResult::Failed)
        || matches!(h.purge, PurgeResult::Failed(_))
}

/// A publish id as the window holds it (ids start at 1; 0 is none).
fn row_id(id: i64) -> i32 {
    i32::try_from(id).unwrap_or(0)
}

fn history_row(h: &HistoryRecord, first: bool, now: DateTime<Local>) -> HistoryRow {
    HistoryRow {
        id: row_id(h.id),
        when: copy::history_when(h.started_at, now).into(),
        result: copy::history_result(h, first).into(),
        failed: went_wrong(h),
        uploaded: format_count(h.uploaded).into(),
        deleted: format_count(h.deleted).into(),
        size: format_size(h.bytes).into(),
        time: copy::history_duration((h.finished_at - h.started_at).num_seconds()).into(),
        purge: copy::history_purge(&h.purge).into(),
    }
}

/// Shows the detail of publish `id`: its summary at once, its files once read. `all`: every file
/// (Show All); otherwise the ones that failed or were not tried.
fn show_detail(p: &Arc<Publisher>, ui: &AppWindow, id: i64, all: bool) {
    let record = {
        let mut state = lock(&p.profile);
        let Some(record) = state.history.iter().find(|h| h.id == id).cloned() else {
            return;
        };
        state.detail = Some(id);
        record
    };
    let when = copy::history_when(record.started_at, Local::now());
    ui.set_history_detail(detail(&record, &when, None, all));
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let files = p2.session.history_files(id).await;
        let ui = p2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if lock(&p2.profile).detail == Some(id) {
                ui.set_history_detail(detail(&record, &when, Some(&files), all));
            }
        });
    });
}

/// The detail of a publish. Until its `files` are read, the counts come from the record and no
/// file is listed.
fn detail(
    h: &HistoryRecord,
    when: &str,
    files: Option<&[HistoryFile]>,
    all: bool,
) -> HistoryDetail {
    let not_done =
        |f: &&HistoryFile| matches!(f.outcome, FileOutcome::Failed(_) | FileOutcome::NotRun(_));
    let (total, failed, paths): (u64, u64, Vec<SharedString>) = match files {
        None => (h.uploaded + h.deleted + h.failed, h.failed, Vec::new()),
        Some(files) => {
            let paths = if all {
                files
                    .iter()
                    .map(|f| copy::history_file_line(f).into())
                    .collect()
            } else {
                files
                    .iter()
                    .filter(not_done)
                    .map(|f| f.path.as_str().into())
                    .collect()
            };
            let failed = files.iter().filter(not_done).count();
            (len_u64(files.len()), len_u64(failed), paths)
        }
    };
    HistoryDetail {
        id: row_id(h.id),
        summary: copy::history_summary(when, h, files.unwrap_or_default()).into(),
        paths: ModelRc::new(VecModel::from(paths)),
        total: i32::try_from(total).unwrap_or(i32::MAX),
        failed: i32::try_from(failed).unwrap_or(i32::MAX),
        purge_failed: matches!(h.purge, PurgeResult::Failed(_)),
        all_shown: all && files.is_some(),
    }
}

fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Change Folder… on the page: the profile publishes the folder picked from now on.
fn change_folder(p: &Arc<Publisher>, _ui: &AppWindow) {
    let Some(id) = lock(&p.profile).shown.clone() else {
        return;
    };
    pick_folder(p, move |p, _ui, folder| {
        let record = lock(&p.profile)
            .page
            .as_ref()
            .map(|facts| facts.record.clone())
            .filter(|record| record.id == id);
        let Some(mut record) = record else {
            return;
        };
        record.local_dir = folder;
        let p2 = p.clone();
        p.link.rt().spawn(async move {
            match p2.session.save_profile(record).await {
                Ok(()) => {
                    let ui = p2.link.ui().clone();
                    let _ = ui.upgrade_in_event_loop(move |ui| load(&p2, &ui));
                }
                Err(e) => p2.link.status(copy::profile_not_saved(e)),
            }
        });
    });
}

/// Asks for a folder, off the UI thread, then hands it to `then` on the UI thread. One picker at
/// a time; a cancelled one does nothing.
fn pick_folder(
    p: &Arc<Publisher>,
    then: impl FnOnce(&Arc<Publisher>, &AppWindow, PathBuf) + Send + 'static,
) {
    {
        let mut state = lock(&p.profile);
        if state.picking {
            return;
        }
        state.picking = true;
    }
    let open = Picking(p.clone());
    p.link.rt().spawn_blocking(move || {
        let picked = win::pick_folder(copy::PICK_PROFILE_FOLDER);
        let p = open.0.clone();
        drop(open);
        let Some(folder) = picked else {
            return;
        };
        let ui = p.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| then(&p, &ui, folder));
    });
}

/// Marks the folder picker closed when dropped, also if the thread holding it panicked.
struct Picking(Arc<Publisher>);

impl Drop for Picking {
    fn drop(&mut self) {
        lock(&self.0.profile).picking = false;
    }
}

// ---- The profile sheet ----

/// Edit Profile (the toolbar's and the page's): the sheet on the profile the page shows.
fn edit_profile(p: &Arc<Publisher>, ui: &AppWindow) {
    let facts = lock(&p.profile).page.clone();
    let Some(PageFacts {
        record,
        site: Some(site),
        cdn,
    }) = facts
    else {
        return;
    };
    let target = SheetTarget {
        record,
        site,
        new: false,
    };
    open_profile_sheet(p, ui, target, Some(cdn));
}

/// Opens the profile sheet for a new profile of `site`, publishing `local` to `remote`.
pub fn open_new(ui: &AppWindow, site: &SiteRecord, local: PathBuf, remote: RemotePath) {
    let Some(p) = PUBLISHER.with(|slot| slot.borrow().upgrade()) else {
        return;
    };
    let name = copy::new_profile_name(&site.name, &remote);
    let target = SheetTarget {
        record: new_record(site, name, local, remote),
        site: site.clone(),
        new: true,
    };
    let cdn = (site.provider != ProviderKind::Bunny).then_some(Cdn::S3);
    open_profile_sheet(&p, ui, target, cdn);
}

/// A new profile of `site`, with the default skip rules, purge and delete guard (no purge on S3).
fn new_record(
    site: &SiteRecord,
    name: String,
    local_dir: PathBuf,
    remote_dir: RemotePath,
) -> ProfileRecord {
    ProfileRecord {
        id: ProfileId::new(),
        site: site.id.clone(),
        name,
        local_dir,
        remote_dir,
        skip_rules: SkipRules::defaults(),
        remove_extras: false,
        purge: if site.provider == ProviderKind::Bunny {
            PurgeMode::default()
        } else {
            PurgeMode::Off
        },
        guard: DeleteGuard::default(),
        created_at: Utc::now(),
        last_published_at: None,
        cache_control: None,
        extras: ProfileExtras::default(),
    }
}

/// Opens the profile sheet on `target`. `cdn`: what purging would use, or `None` to look it up
/// (the line under the purge choice says so meanwhile).
fn open_profile_sheet(p: &Arc<Publisher>, ui: &AppWindow, target: SheetTarget, cdn: Option<Cdn>) {
    let note = cdn
        .as_ref()
        .map_or_else(|| copy::CDN_NOTE_CHECKING.to_owned(), Cdn::note);
    let draft = draft_of(&target, note);
    let site = target.site.clone();
    lock(&p.profile).sheet = Some(target);
    ui.set_profile_draft(draft);
    ui.set_profile_sheet_open(true);
    if cdn.is_none() {
        refresh_cdn_note(p, site);
    }
}

/// The sheet's fields for `target`.
fn draft_of(target: &SheetTarget, cdn_note: String) -> ProfileDraft {
    let r = &target.record;
    ProfileDraft {
        id: if target.new {
            SharedString::new()
        } else {
            r.id.0.as_str().into()
        },
        name: r.name.as_str().into(),
        site_name: target.site.name.as_str().into(),
        folder: r.local_dir.display().to_string().into(),
        remote_dir: r.remote_dir.as_str().into(),
        skip_rules: r.skip_rules.join("\n").into(),
        remove_extras: r.remove_extras,
        purge: purge_index(r.purge),
        guard_files: r.guard.files.to_string().into(),
        guard_percent: r.guard.percent.to_string().into(),
        cdn_note: cdn_note.into(),
        error: SharedString::new(),
        cache_control: r.cache_control.as_deref().unwrap_or_default().into(),
        cache_control_enabled: target.site.provider == ProviderKind::S3,
        cache_control_note: if target.site.provider == ProviderKind::S3 {
            SharedString::new()
        } else {
            copy::CACHE_CONTROL_BUNNY.into()
        },
        watch: r.extras.watch,
        unattended_deletes: r.extras.unattended_deletes,
    }
}

/// The purge choice's position in the sheet (Off, Changed files, Whole zone), and back.
fn purge_index(mode: PurgeMode) -> i32 {
    match mode {
        PurgeMode::Off => 0,
        PurgeMode::Changed => 1,
        PurgeMode::Zone => 2,
    }
}

fn purge_mode(index: i32) -> PurgeMode {
    match index {
        0 => PurgeMode::Off,
        2 => PurgeMode::Zone,
        _ => PurgeMode::Changed,
    }
}

/// The sheet's fields checked and turned into the profile, or what to say about the first that
/// is wrong. Whether the folder exists is checked when saving, off the UI thread.
fn checked(target: &SheetTarget, d: &ProfileDraft) -> Result<ProfileRecord, String> {
    let name = d.name.trim();
    if name.is_empty() {
        return Err(copy::PROFILE_NAME_MISSING.to_owned());
    }
    let folder = d.folder.trim();
    if folder.is_empty() {
        return Err(copy::PROFILE_FOLDER_MISSING.to_owned());
    }
    let remote_dir = RemotePath::parse(d.remote_dir.trim())
        .map_err(|_| copy::PROFILE_REMOTE_INVALID.to_owned())?;
    let files: u64 = d
        .guard_files
        .trim()
        .parse()
        .map_err(|_| copy::PROFILE_GUARD_FILES_INVALID.to_owned())?;
    let percent: u8 = d
        .guard_percent
        .trim()
        .trim_end_matches('%')
        .trim_end()
        .parse()
        .ok()
        .filter(|percent| *percent <= 100)
        .ok_or_else(|| copy::PROFILE_GUARD_PERCENT_INVALID.to_owned())?;
    let skip_rules = d
        .skip_rules
        .lines()
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(str::to_owned)
        .collect();
    let purge = if target.site.provider == ProviderKind::Bunny {
        purge_mode(d.purge)
    } else {
        PurgeMode::Off
    };
    // S3 only: Bunny sets caching in the pull zone.
    let cache_control = if target.site.provider == ProviderKind::S3 {
        let value = d.cache_control.trim();
        if value.chars().any(|c| c.is_control() || !c.is_ascii()) {
            return Err(copy::CACHE_CONTROL_INVALID.to_owned());
        }
        (!value.is_empty()).then(|| value.to_owned())
    } else {
        target.record.cache_control.clone()
    };
    Ok(ProfileRecord {
        name: name.to_owned(),
        local_dir: PathBuf::from(folder),
        remote_dir,
        skip_rules,
        remove_extras: d.remove_extras,
        purge,
        guard: DeleteGuard { files, percent },
        cache_control,
        // I6: the watch folder's two options (T4 starts and stops the watching); the schedule is
        // the Schedule sheet's.
        extras: ProfileExtras {
            watch: d.watch,
            unattended_deletes: d.unattended_deletes,
            ..target.record.extras.clone()
        },
        ..target.record.clone()
    })
}

/// Shows `message` under the sheet's fields, keeping them as they were handed back.
fn show_sheet_error(ui: &AppWindow, mut draft: ProfileDraft, message: String) {
    draft.error = message.into();
    ui.set_profile_draft(draft);
}

/// Save (Create Profile for a new one): checks the fields, saves the profile and closes the
/// sheet; a new profile then goes on to the Publish sheet.
fn save(p: &Arc<Publisher>, ui: &AppWindow, draft: ProfileDraft) {
    let Some(target) = lock(&p.profile).sheet.clone() else {
        return;
    };
    let record = match checked(&target, &draft) {
        Ok(record) => record,
        Err(message) => return show_sheet_error(ui, draft, message),
    };
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let dir = record.local_dir.clone();
        let saved = if blocking(move || dir.is_dir()).await {
            p2.session
                .save_profile(record.clone())
                .await
                .map_err(copy::profile_not_saved)
        } else {
            Err(copy::PROFILE_FOLDER_NOT_FOUND.to_owned())
        };
        if saved.is_ok() && target.site.provider == ProviderKind::S3 {
            // The engine's Cache-Control rule follows the profile (the old folder's goes).
            let engine = p2.transfers.engine();
            let old = &target.record;
            if !target.new && old.remote_dir != record.remote_dir && old.cache_control.is_some() {
                engine.set_cache_control(&record.site, &old.remote_dir, None);
            }
            if !target.new || record.cache_control.is_some() {
                engine.set_cache_control(
                    &record.site,
                    &record.remote_dir,
                    record.cache_control.clone(),
                );
            }
        }
        let ui = p2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            sheet_saved(&p2, &ui, target.new, &record, saved, draft);
        });
    });
}

/// What became of Save. A sheet that has moved on (closed, or opened on another profile) is left
/// alone, but a profile saved meanwhile is still listed.
fn sheet_saved(
    p: &Arc<Publisher>,
    ui: &AppWindow,
    new: bool,
    record: &ProfileRecord,
    saved: Result<(), String>,
    draft: ProfileDraft,
) {
    let current = ui.get_profile_sheet_open()
        && lock(&p.profile)
            .sheet
            .as_ref()
            .is_some_and(|target| target.record.id == record.id);
    match saved {
        Err(message) if current => show_sheet_error(ui, draft, message),
        Err(_) => {}
        Ok(()) => {
            refresh_sidebar(p);
            if !current {
                return;
            }
            let shown = {
                let mut state = lock(&p.profile);
                state.sheet = None;
                state.shown.as_ref() == Some(&record.id)
            };
            ui.set_profile_sheet_open(false);
            if shown {
                load(p, ui);
            }
            if new {
                p.open_sheet(record.id.clone());
            }
        }
    }
}

/// Delete Profile, confirmed: the profile and its history go, the sheet closes, and the page
/// leaves for the panes if it showed that profile.
fn delete(p: &Arc<Publisher>, _ui: &AppWindow, id: ProfileId) {
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let removed = p2.session.remove_profile(id.clone()).await;
        let ui = p2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| match removed {
            Err(e) => {
                if ui.get_profile_sheet_open() {
                    show_sheet_error(&ui, ui.get_profile_draft(), copy::profile_not_deleted(e));
                }
            }
            Ok(()) => {
                let shown = {
                    let mut state = lock(&p2.profile);
                    state.sheet = None;
                    let shown = state.shown.as_ref() == Some(&id);
                    if shown {
                        state.shown = None;
                        state.page = None;
                        state.history.clear();
                        state.detail = None;
                    }
                    shown
                };
                ui.set_profile_sheet_open(false);
                if shown {
                    ui.set_selected_profile(-1);
                }
                refresh_sidebar(&p2);
            }
        });
    });
}

/// Change Folder… on the sheet: the folder picked goes into the sheet's folder field.
fn pick_sheet_folder(p: &Arc<Publisher>, _ui: &AppWindow) {
    pick_folder(p, |_p, ui, folder| {
        if !ui.get_profile_sheet_open() {
            return;
        }
        let mut draft = ui.get_profile_draft();
        draft.folder = folder.display().to_string().into();
        draft.error = SharedString::new();
        ui.set_profile_draft(draft);
    });
}

/// Add Account Key… under the purge choice: the CDN key sheet, then the line looked up again
/// once that sheet has closed.
fn add_key(p: &Arc<Publisher>, ui: &AppWindow) {
    let Some(site) = lock(&p.profile)
        .sheet
        .as_ref()
        .map(|target| target.site.clone())
    else {
        return;
    };
    p.ask_cdn_key(site.clone());
    watch_key_sheet(p, ui, site, false, 0);
}

/// Waits for the CDN key sheet to close (it has no callback for the profile sheet), then looks up
/// the line under the purge choice again. A key sheet that never opened is waited for a few
/// seconds only; a profile sheet that closed stops the wait.
fn watch_key_sheet(p: &Arc<Publisher>, ui: &AppWindow, site: SiteRecord, seen: bool, looks: u32) {
    let open = ui.get_cdn_key_open();
    if !open && (seen || looks >= KEY_SHEET_OPEN_LOOKS) {
        refresh_cdn_note(p, site);
        return;
    }
    let (p, weak) = (p.clone(), ui.as_weak());
    Timer::single_shot(KEY_SHEET_POLL, move || {
        if let Some(ui) = weak.upgrade()
            && ui.get_profile_sheet_open()
        {
            watch_key_sheet(&p, &ui, site, seen || open, looks + 1);
        }
    });
}

/// Looks up what purging `site` would use, then puts it under the sheet's purge choice while the
/// sheet is still on that site.
fn refresh_cdn_note(p: &Arc<Publisher>, site: SiteRecord) {
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let cdn = read_cdn(&p2, &site).await;
        let ui = p2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            let same = lock(&p2.profile)
                .sheet
                .as_ref()
                .is_some_and(|target| target.site.id == site.id);
            if same && ui.get_profile_sheet_open() {
                let mut draft = ui.get_profile_draft();
                draft.cdn_note = cdn.note().into();
                ui.set_profile_draft(draft);
            }
        });
    });
}

// ---- The sidebar's list ----

/// Reads the profiles again and lists them in the sidebar, as after a profile is saved or deleted.
pub(super) fn refresh_sidebar(p: &Arc<Publisher>) {
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let now = Utc::now();
        let items: Vec<ProfileItem> = p2
            .session
            .profiles()
            .await
            .iter()
            .map(|r| ProfileItem {
                id: r.id.0.as_str().into(),
                name: r.name.as_str().into(),
                meta: copy::profile_meta(r.last_published_at, now).into(),
                // The watch folder sets it (T4).
                watching: false,
            })
            .collect();
        let _ = p2
            .link
            .ui()
            .upgrade_in_event_loop(move |ui| show_profiles(&ui, items));
    });
}

/// Lists `items` in the sidebar, keeping the profile selected (by id) selected. One that is gone
/// leaves the page for the panes.
fn show_profiles(ui: &AppWindow, items: Vec<ProfileItem>) {
    let selected = usize::try_from(ui.get_selected_profile())
        .ok()
        .and_then(|i| ui.get_profiles().row_data(i))
        .map(|item| item.id);
    let index = selected
        .and_then(|id| items.iter().position(|item| item.id == id))
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    ui.set_profiles(ModelRc::new(VecModel::from(items)));
    ui.set_selected_profile(index);
}

/// Reads the sidebar's list and the profile page again (after a publish). From any thread.
pub(super) fn refresh(p: &Arc<Publisher>) {
    refresh_sidebar(p);
    let p2 = p.clone();
    let _ = p.link.ui().upgrade_in_event_loop(move |ui| load(&p2, &ui));
}

// ---- Settings › Publish, and the `.env` profile ----

/// What the Settings window's Publish tab does with the kept Bunny account keys: forgets one when
/// asked (its key, account record and CDN links), then lists them. Only names and ids are read.
fn key_keeper(p: &Arc<Publisher>) -> settings_view::KeyKeeper {
    let p = p.clone();
    Rc::new(move |window: &SettingsWindow, forget: Option<String>| {
        let (p, weak) = (p.clone(), window.as_weak());
        let rt = p.link.rt().clone();
        rt.spawn(async move {
            let forgot = forget.is_some();
            let error = match forget {
                Some(id) => p
                    .session
                    .forget_cdn_key(AccountId(id))
                    .await
                    .err()
                    .map(copy::key_not_forgotten)
                    .unwrap_or_default(),
                None => String::new(),
            };
            let keys: Vec<ProfileItem> = p
                .session
                .bunny_accounts()
                .await
                .iter()
                .map(|account| ProfileItem {
                    id: account.id.0.as_str().into(),
                    name: account.name.as_str().into(),
                    meta: copy::key_meta(account.created_at).into(),
                    watching: false,
                })
                .collect();
            let _ = weak.upgrade_in_event_loop(move |window| {
                settings_view::show_keys(&window, keys, &error);
            });
            // The page's purge chip may name a pull zone whose key is now gone.
            if forgot {
                let ui = p.link.ui().clone();
                let _ = ui.upgrade_in_event_loop(move |ui| load(&p, &ui));
            }
        });
    })
}

/// Makes the publish profile a `.env` import asked for, now that its site is saved: named after
/// the zone, publishing the file's folder to its prefix, with the defaults otherwise. Nothing is
/// made when the folder can't be found or the prefix is no folder name.
fn make_imported(p: &Arc<Publisher>, site: &SiteRecord, imported: ImportedPublish) {
    let Ok(remote) = RemotePath::parse(&imported.prefix) else {
        return;
    };
    let record = new_record(site, imported.zone, imported.local_dir, remote);
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let dir = record.local_dir.clone();
        if !blocking(move || dir.is_dir()).await {
            return;
        }
        match p2.session.save_profile(record).await {
            Ok(()) => refresh_sidebar(&p2),
            Err(e) => p2.link.status(copy::imported_profile_not_saved(e)),
        }
    });
}
