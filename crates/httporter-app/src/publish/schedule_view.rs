// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduled publish (§8.8.2, I6, T3): the profile page's Schedule card, the Schedule sheet, and
//! each profile's Task Scheduler task (`httporter_platform::win::Scheduler`, in `\HTTPorter\`).
//!
//! - **The card** follows the page ([`page_shown`], called by the profile page each time it
//!   shows a profile): what the schedule is, its next run as Task Scheduler says it, and the
//!   newest `HistorySource::Scheduled` publish. A task deleted in Task Scheduler while the
//!   schedule is on shows "The scheduled task is missing", with Recreate.
//! - **The sheet:** Save checks the fields, then puts the task in place first (created or
//!   replaced while the schedule is on, removed when it is off), then stores
//!   `ProfileExtras::schedule`. If the profile can't be stored, the task goes back to what the
//!   profile still says.
//! - **The task** runs this program with `--cli publish --profile <id> --scheduled` (and
//!   `--purge …` when the schedule purges otherwise than the profile). Never a secret.
//! - **Deleting a profile** removes its task; so does removing its site
//!   ([`remove_orphan_tasks`]), and at start every `\HTTPorter\Publish *` task whose profile no
//!   longer exists is removed. Nothing else in Task Scheduler is ever touched. That clean-up
//!   doesn't run with `HTTPORTER_DATA_DIR` set: a development run's database isn't the one the
//!   PC's tasks belong to.
//!
//! Every Task Scheduler call runs off the UI thread. The app's own tests get a `FakeScheduler`
//! (the window's tests install Publish too): building HTTPorter never touches a real task.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::{DateTime, Local};
use httporter_core::{
    ActivityKind, HistoryRecord, HistorySource, ProfileId, ProfileRecord, ProviderKind, PurgeMode,
    Schedule, ScheduleRepeat,
};
use httporter_platform::paths::DATA_DIR_ENV;
use httporter_platform::win::{self, ScheduledTask, Scheduler};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{Publisher, copy, profile_view};
use crate::manage::activity_view;
use crate::session::{Session, blocking};
use crate::{AppWindow, ScheduleDraft};

/// A new schedule's start time (06:00), its hours apart and its weekdays (Monday).
const DEFAULT_AT: u16 = 6 * 60;
const DEFAULT_EVERY: u8 = 1;
const DEFAULT_DAYS: [bool; 7] = [true, false, false, false, false, false, false];

/// What the card and the sheet remember between callbacks. The UI thread's, behind a lock so the
/// helpers can be called from anywhere.
struct Card {
    /// The profile the page shows, as last read.
    shown: Option<ProfileRecord>,
    /// Whether its site has a CDN to purge (Bunny).
    purge: bool,
    /// Counts the card's lookups: only the latest one's answer is shown.
    serial: u64,
    /// A Save is on its way: another press waits for it.
    saving: bool,
}

static CARD: Mutex<Card> = Mutex::new(Card {
    shown: None,
    purge: false,
    serial: 0,
    saving: false,
});

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The Task Scheduler the app uses: the PC's own, except in the app's tests, which must never
/// touch it.
fn scheduler() -> Arc<dyn Scheduler> {
    static SCHEDULER: OnceLock<Arc<dyn Scheduler>> = OnceLock::new();
    SCHEDULER
        .get_or_init(|| {
            #[cfg(test)]
            let scheduler: Arc<dyn Scheduler> = Arc::new(win::FakeScheduler::new());
            #[cfg(not(test))]
            let scheduler: Arc<dyn Scheduler> = Arc::new(win::TaskSchedulerCom);
            scheduler
        })
        .clone()
}

/// Wires the Schedule card and sheet of `ui` to `p`, and starts the clean-up of the tasks whose
/// profile is gone (off the UI thread).
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    ui.on_open_schedule(with0(ui, p, open_sheet));
    ui.on_schedule_save(with(ui, p, save));
    ui.on_schedule_recreate(with0(ui, p, recreate));
    let session = p.session.clone();
    p.link.rt().spawn(async move {
        blocking(move || remove_orphan_tasks(&session)).await;
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

/// The profile's schedule while it is on.
fn active(record: &ProfileRecord) -> Option<Schedule> {
    record.extras.schedule.clone().filter(|s| s.enabled)
}

// ---- The card ----

/// What Task Scheduler says of a profile's task.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskState {
    /// Not there: deleted in Task Scheduler.
    Missing,
    /// There, and when it runs next (`None`: no run left, or not known).
    Present(Option<DateTime<Local>>),
    /// Task Scheduler couldn't be asked.
    Unknown,
}

fn task_state(s: &dyn Scheduler, name: &str) -> TaskState {
    match s.exists(name) {
        Ok(false) => TaskState::Missing,
        Ok(true) => TaskState::Present(s.next_run(name).ok().flatten()),
        Err(_) => TaskState::Unknown,
    }
}

/// The card's value for a schedule that is on, and whether to say its task is missing.
fn card_text(schedule: &Schedule, state: &TaskState, now: DateTime<Local>) -> (String, bool) {
    match state {
        TaskState::Missing => (copy::schedule_text(schedule, None, now), true),
        TaskState::Present(next) => (copy::schedule_text(schedule, *next, now), false),
        TaskState::Unknown => (copy::schedule_text(schedule, None, now), false),
    }
}

/// The profile page shows `page` (a profile and its publishes, newest first), or none (`None`,
/// while another profile loads or once it is gone): the card follows. Task Scheduler is asked
/// off the UI thread; meanwhile a profile seen before keeps what the card said.
pub(super) fn page_shown(
    p: &Arc<Publisher>,
    ui: &AppWindow,
    page: Option<(&ProfileRecord, &[HistoryRecord])>,
) {
    let Some((record, history)) = page else {
        let mut card = lock(&CARD);
        card.shown = None;
        card.serial += 1;
        drop(card);
        ui.set_schedule_text(SharedString::new());
        ui.set_schedule_last(SharedString::new());
        ui.set_schedule_missing(false);
        return;
    };
    let purge = p
        .session
        .site(&record.site)
        .is_some_and(|site| site.provider == ProviderKind::Bunny);
    let (serial, seen) = {
        let mut card = lock(&CARD);
        let seen = card.shown.as_ref().is_some_and(|old| {
            old.id == record.id && old.extras.schedule == record.extras.schedule
        });
        card.shown = Some(record.clone());
        card.purge = purge;
        card.serial += 1;
        (card.serial, seen)
    };
    let now = Local::now();
    let last = history
        .iter()
        .find(|h| h.source == HistorySource::Scheduled)
        .map(|h| copy::schedule_last(h, now))
        .unwrap_or_default();
    ui.set_schedule_last(last.into());
    let Some(schedule) = active(record) else {
        ui.set_schedule_text(copy::SCHEDULE_OFF.into());
        ui.set_schedule_missing(false);
        return;
    };
    if !seen {
        let (text, missing) = card_text(&schedule, &TaskState::Unknown, now);
        ui.set_schedule_text(text.into());
        ui.set_schedule_missing(missing);
    }
    let name = win::task_name(&record.id);
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        let s = scheduler();
        let state = blocking(move || task_state(&*s, &name)).await;
        let _ = p2.link.ui().upgrade_in_event_loop(move |ui| {
            if lock(&CARD).serial == serial {
                let (text, missing) = card_text(&schedule, &state, Local::now());
                ui.set_schedule_text(text.into());
                ui.set_schedule_missing(missing);
            }
        });
    });
}

/// Recreate on the card: the task again from the schedule stored, then the card again. A
/// failure goes to the status line and the activity log.
fn recreate(p: &Arc<Publisher>, _ui: &AppWindow) {
    let Some(id) = lock(&CARD).shown.as_ref().map(|r| r.id.clone()) else {
        return;
    };
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        // The profile as stored now: it may have changed since the page read it.
        let fresh = p2.session.profiles().await.into_iter().find(|r| r.id == id);
        let done = match fresh {
            None => Err(copy::SCHEDULE_PROFILE_GONE.to_owned()),
            Some(fresh) => {
                let (s, record) = (scheduler(), fresh.clone());
                blocking(move || put_task(&*s, &record))
                    .await
                    .map(|()| fresh)
            }
        };
        match done {
            Ok(record) => log(&p2, &record, true, copy::schedule_recreated(&record.name)),
            Err(message) => {
                activity_view::event(ActivityKind::Publish, "", "", false, message.clone(), 0);
                p2.link.status(message);
            }
        }
        profile_view::refresh(&p2);
    });
}

// ---- The sheet ----

/// Publish on a Schedule… (the window has opened the sheet): its fields from the profile shown.
fn open_sheet(_p: &Arc<Publisher>, ui: &AppWindow) {
    let (record, purge) = {
        let card = lock(&CARD);
        (card.shown.clone(), card.purge)
    };
    match record {
        Some(record) => ui.set_schedule_draft(draft_of(&record, purge)),
        // The page hasn't read the profile yet: nothing to edit.
        None => ui.set_schedule_sheet_open(false),
    }
}

/// The sheet's fields for `record`; a profile never scheduled starts daily at 06:00, off.
fn draft_of(record: &ProfileRecord, purge_enabled: bool) -> ScheduleDraft {
    let schedule = record.extras.schedule.as_ref();
    let (repeat, every, days) = match schedule.map(|s| &s.repeat) {
        Some(ScheduleRepeat::Hourly { every }) => (0, *every, DEFAULT_DAYS),
        Some(ScheduleRepeat::Weekly { days }) => (2, DEFAULT_EVERY, *days),
        Some(ScheduleRepeat::Daily) | None => (1, DEFAULT_EVERY, DEFAULT_DAYS),
    };
    ScheduleDraft {
        profile_name: record.name.as_str().into(),
        enabled: schedule.is_some_and(|s| s.enabled),
        repeat,
        every_hours: every.to_string().into(),
        time: copy::clock(schedule.map_or(DEFAULT_AT, |s| s.at_minutes)).into(),
        days: ModelRc::new(VecModel::from(days.to_vec())),
        purge: purge_index(schedule.and_then(|s| s.purge)),
        purge_enabled,
        error: SharedString::new(),
    }
}

/// The sheet's purge choice: the profile's, Off, Changed files, Whole zone.
fn purge_index(purge: Option<PurgeMode>) -> i32 {
    match purge {
        None => 0,
        Some(PurgeMode::Off) => 1,
        Some(PurgeMode::Changed) => 2,
        Some(PurgeMode::Zone) => 3,
    }
}

fn purge_mode(index: i32) -> Option<PurgeMode> {
    match index {
        1 => Some(PurgeMode::Off),
        2 => Some(PurgeMode::Changed),
        3 => Some(PurgeMode::Zone),
        _ => None,
    }
}

/// A time of day as typed ("06:00", "6:00", "6.30", "0630", "6") in minutes after midnight.
fn parse_time(text: &str) -> Option<u16> {
    let text = text.trim();
    if !text.is_ascii() {
        return None;
    }
    let (hours, minutes) = match text.split_once([':', '.']) {
        Some(parts) => parts,
        None if text.len() > 2 => text.split_at(text.len() - 2),
        None => (text, "00"),
    };
    let number = |part: &str, most: u16| {
        let part = part.trim();
        let digits =
            !part.is_empty() && part.len() <= 2 && part.bytes().all(|b| b.is_ascii_digit());
        digits
            .then(|| part.parse::<u16>().ok())
            .flatten()
            .filter(|n| *n <= most)
    };
    Some(number(hours, 23)? * 60 + number(minutes, 59)?)
}

/// The schedule the sheet's fields describe (on), or what to say about the first that is wrong.
fn read_fields(d: &ScheduleDraft, purge_enabled: bool) -> Result<Schedule, String> {
    let at_minutes = parse_time(&d.time).ok_or_else(|| copy::SCHEDULE_TIME_INVALID.to_owned())?;
    let repeat = match d.repeat {
        0 => {
            let every = d.every_hours.trim();
            let every = every
                .bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| every.parse::<u8>().ok())
                .flatten()
                .filter(|n| (1..=24).contains(n))
                .ok_or_else(|| copy::SCHEDULE_HOURS_INVALID.to_owned())?;
            ScheduleRepeat::Hourly { every }
        }
        2 => {
            let mut days = [false; 7];
            for (day, on) in days.iter_mut().zip(d.days.iter()) {
                *day = on;
            }
            if !days.contains(&true) {
                return Err(copy::SCHEDULE_DAYS_MISSING.to_owned());
            }
            ScheduleRepeat::Weekly { days }
        }
        _ => ScheduleRepeat::Daily,
    };
    Ok(Schedule {
        enabled: true,
        repeat,
        at_minutes,
        purge: if purge_enabled {
            purge_mode(d.purge)
        } else {
            None
        },
    })
}

/// What Save stores: the schedule on, as the fields say; or off, keeping the fields for next time
/// when they read (else what was stored before, if anything).
fn checked(
    d: &ScheduleDraft,
    before: Option<&Schedule>,
    purge_enabled: bool,
) -> Result<Option<Schedule>, String> {
    let fields = read_fields(d, purge_enabled);
    if d.enabled {
        return fields.map(Some);
    }
    Ok(fields.ok().or_else(|| before.cloned()).map(|s| Schedule {
        enabled: false,
        ..s
    }))
}

/// The sheet's fields as Save handed them back, in a form that can cross threads (a draft holds
/// a model, which can't): Save's answer comes back to the UI thread with them.
struct Edited {
    profile_name: SharedString,
    enabled: bool,
    repeat: i32,
    every_hours: SharedString,
    time: SharedString,
    days: Vec<bool>,
    purge: i32,
    purge_enabled: bool,
}

impl Edited {
    fn of(d: &ScheduleDraft) -> Self {
        Self {
            profile_name: d.profile_name.clone(),
            enabled: d.enabled,
            repeat: d.repeat,
            every_hours: d.every_hours.clone(),
            time: d.time.clone(),
            days: d.days.iter().collect(),
            purge: d.purge,
            purge_enabled: d.purge_enabled,
        }
    }

    fn draft(self) -> ScheduleDraft {
        ScheduleDraft {
            profile_name: self.profile_name,
            enabled: self.enabled,
            repeat: self.repeat,
            every_hours: self.every_hours,
            time: self.time,
            days: ModelRc::new(VecModel::from(self.days)),
            purge: self.purge,
            purge_enabled: self.purge_enabled,
            error: SharedString::new(),
        }
    }
}

/// Shows `message` under the sheet's fields, keeping them as they were handed back.
fn show_error(ui: &AppWindow, mut draft: ScheduleDraft, message: String) {
    draft.error = message.into();
    ui.set_schedule_draft(draft);
}

/// Save: checks the fields, then puts the task and the profile's schedule in place (off the UI
/// thread) and closes the sheet; or shows what went wrong in it.
fn save(p: &Arc<Publisher>, ui: &AppWindow, draft: ScheduleDraft) {
    let (record, purge) = {
        let card = lock(&CARD);
        if card.saving {
            return;
        }
        let Some(record) = card.shown.clone() else {
            drop(card);
            ui.set_schedule_sheet_open(false);
            return;
        };
        let purge = card.purge;
        (record, purge)
    };
    let schedule = match checked(&draft, record.extras.schedule.as_ref(), purge) {
        Ok(schedule) => schedule,
        Err(message) => return show_error(ui, draft, message),
    };
    lock(&CARD).saving = true;
    let (p2, edited) = (p.clone(), Edited::of(&draft));
    p.link.rt().spawn(async move {
        let saved = apply(&p2.session, scheduler(), &record.id, schedule).await;
        let ui = p2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            sheet_saved(&p2, &ui, &record.id, saved, edited);
        });
    });
}

/// Puts `schedule` in place for profile `id`: the task first (what most often fails), then the
/// profile, read again so that nothing else it holds is put back. If the profile can't be saved,
/// the task goes back to what the profile still says. The profile as saved.
async fn apply(
    session: &Session,
    s: Arc<dyn Scheduler>,
    id: &ProfileId,
    schedule: Option<Schedule>,
) -> Result<ProfileRecord, String> {
    let Some(before) = session.profiles().await.into_iter().find(|r| r.id == *id) else {
        return Err(copy::SCHEDULE_PROFILE_GONE.to_owned());
    };
    let mut after = before.clone();
    after.extras.schedule = schedule;
    let (s2, target) = (s.clone(), after.clone());
    blocking(move || put_task(&*s2, &target)).await?;
    if let Err(e) = session.save_profile(after.clone()).await {
        let _ = blocking(move || put_task(&*s, &before)).await;
        return Err(copy::schedule_not_saved(e));
    }
    Ok(after)
}

/// What became of Save. A sheet closed meanwhile (or now on another profile) is left alone; a
/// failure then goes to the status line.
fn sheet_saved(
    p: &Arc<Publisher>,
    ui: &AppWindow,
    id: &ProfileId,
    saved: Result<ProfileRecord, String>,
    edited: Edited,
) {
    let current = {
        let mut card = lock(&CARD);
        card.saving = false;
        let current = card.shown.as_ref().is_some_and(|r| r.id == *id);
        if let (true, Ok(record)) = (current, &saved) {
            card.shown = Some(record.clone());
        }
        current && ui.get_schedule_sheet_open()
    };
    match saved {
        Err(message) => {
            activity_view::event(ActivityKind::Publish, "", "", false, message.clone(), 0);
            if current {
                show_error(ui, edited.draft(), message);
            } else {
                p.link.status(message);
            }
        }
        Ok(record) => {
            if current {
                ui.set_schedule_sheet_open(false);
            }
            let detail = match active(&record) {
                Some(schedule) => {
                    copy::schedule_set(&record.name, &copy::schedule_summary(&schedule))
                }
                None => copy::schedule_off(&record.name),
            };
            log(p, &record, true, detail);
            // The page reads the profile again (Edit Profile then starts from this schedule),
            // and the card asks Task Scheduler for the next run.
            profile_view::refresh(p);
        }
    }
}

// ---- The task ----

/// The task that runs `record`'s schedule, with this program.
fn task_for(record: &ProfileRecord, schedule: Schedule) -> Result<ScheduledTask, String> {
    let exe = std::env::current_exe().map_err(copy::schedule_no_exe)?;
    Ok(task_with(record, schedule, exe))
}

fn task_with(record: &ProfileRecord, schedule: Schedule, exe: PathBuf) -> ScheduledTask {
    let mut args: Vec<String> = ["--cli", "publish", "--profile"]
        .map(str::to_owned)
        .to_vec();
    args.push(record.id.0.clone());
    args.push("--scheduled".to_owned());
    // The CLI's own words for the purges (`--purge changed|all|none`).
    if let Some(purge) = schedule.purge {
        let flag = match purge {
            PurgeMode::Off => "none",
            PurgeMode::Changed => "changed",
            PurgeMode::Zone => "all",
        };
        args.extend(["--purge".to_owned(), flag.to_owned()]);
    }
    ScheduledTask {
        name: win::task_name(&record.id),
        description: copy::schedule_task_description(&record.name),
        exe,
        args,
        schedule,
    }
}

/// Makes `record`'s task match its schedule: created or replaced while it is on, removed
/// otherwise. Blocks.
fn put_task(s: &dyn Scheduler, record: &ProfileRecord) -> Result<(), String> {
    match active(record) {
        Some(schedule) => s
            .upsert(&task_for(record, schedule)?)
            .map_err(copy::schedule_task_not_saved),
        None => s
            .remove(&win::task_name(&record.id))
            .map_err(copy::schedule_task_not_removed),
    }
}

/// Logs `detail` about `record`'s schedule in the activity log, under its site and folder.
fn log(p: &Publisher, record: &ProfileRecord, ok: bool, detail: String) {
    let site = p
        .session
        .site(&record.site)
        .map(|site| site.name)
        .unwrap_or_default();
    let folder = format!("/{}", record.remote_dir.as_str());
    activity_view::event(ActivityKind::Publish, &site, &folder, ok, detail, 0);
}

/// Profile `id` was deleted: its task goes too (one that isn't there is no error).
pub(super) fn profile_deleted(p: &Arc<Publisher>, id: &ProfileId) {
    let name = win::task_name(id);
    p.link.rt().spawn(async move {
        let (s, task) = (scheduler(), name.clone());
        if let Err(e) = blocking(move || s.remove(&task)).await {
            let detail = copy::schedule_task_trouble(&name, e);
            activity_view::event(ActivityKind::Publish, "", "", false, detail, 0);
        }
    });
}

/// A profile was saved from its sheet (`before` as it was): a scheduled one that was renamed gets
/// its task again, whose description names it.
pub(super) fn profile_saved(p: &Arc<Publisher>, before: &ProfileRecord, after: &ProfileRecord) {
    if before.name == after.name || active(after).is_none() {
        return;
    }
    let record = after.clone();
    p.link.rt().spawn(async move {
        let (s, task) = (scheduler(), record.clone());
        if let Err(e) = blocking(move || put_task(&*s, &task)).await {
            let detail = copy::schedule_task_trouble(&win::task_name(&record.id), e);
            activity_view::event(ActivityKind::Publish, "", "", false, detail, 0);
        }
    });
}

/// Removes every `\HTTPorter\Publish <id>` task whose profile no longer exists (at start, and
/// after a site and its profiles were removed), and logs each. Nothing when the profiles can't
/// be read, and nothing in a run with `HTTPORTER_DATA_DIR` set (its database isn't the one the
/// PC's tasks belong to). Blocks: call it from a blocking thread.
pub(crate) fn remove_orphan_tasks(session: &Session) {
    if std::env::var_os(DATA_DIR_ENV).is_some() {
        return;
    }
    let Ok(profiles) = session.profile_ids() else {
        return;
    };
    for name in remove_orphans(&*scheduler(), &profiles) {
        let detail = copy::schedule_orphan_removed(&name);
        activity_view::event(ActivityKind::Publish, "", "", true, detail, 0);
    }
}

/// Removes the tasks in the folder named as a profile's task (`Publish <id>`) whose profile isn't
/// in `profiles`, and returns their names. Any other task in the folder is left alone.
fn remove_orphans(s: &dyn Scheduler, profiles: &[ProfileId]) -> Vec<String> {
    let prefix = win::task_name(&ProfileId(String::new()));
    let Ok(names) = s.list() else {
        return Vec::new();
    };
    names
        .into_iter()
        .filter(|name| {
            name.strip_prefix(&prefix)
                .is_some_and(|id| !id.is_empty() && !profiles.iter().any(|p| p.0 == id))
        })
        .filter(|name| s.remove(name).is_ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use httporter_core::{DeleteGuard, ProfileExtras, PublishResult, PurgeResult, SiteId};
    use httporter_platform::win::FakeScheduler;

    fn profile(name: &str, schedule: Option<Schedule>) -> ProfileRecord {
        ProfileRecord {
            id: ProfileId::new(),
            site: SiteId::new(),
            name: name.to_owned(),
            local_dir: PathBuf::from("D:\\site"),
            remote_dir: httporter_core::RemotePath::root(),
            skip_rules: Vec::new(),
            remove_extras: false,
            purge: PurgeMode::Changed,
            guard: DeleteGuard::default(),
            created_at: Utc::now(),
            last_published_at: None,
            cache_control: None,
            extras: ProfileExtras {
                schedule,
                ..ProfileExtras::default()
            },
        }
    }

    fn daily(at_minutes: u16) -> Schedule {
        Schedule {
            enabled: true,
            repeat: ScheduleRepeat::Daily,
            at_minutes,
            purge: None,
        }
    }

    fn local(text: &str) -> DateTime<Local> {
        let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        Local.from_local_datetime(&naive).earliest().unwrap()
    }

    #[test]
    fn times_are_read_as_typed() {
        for (text, minutes) in [
            ("06:00", Some(360)),
            ("6:00", Some(360)),
            (" 6.30 ", Some(390)),
            ("0630", Some(390)),
            ("630", Some(390)),
            ("6", Some(360)),
            ("23:59", Some(1439)),
            ("24:00", None),
            ("6:60", None),
            ("+6:00", None),
            ("", None),
            ("６:00", None),
            ("six", None),
        ] {
            assert_eq!(parse_time(text), minutes, "{text:?}");
        }
    }

    #[test]
    fn the_sheet_round_trips_a_schedule() {
        let days = [false, true, false, true, false, false, true];
        let weekly = Schedule {
            enabled: true,
            repeat: ScheduleRepeat::Weekly { days },
            at_minutes: 22 * 60 + 15,
            purge: Some(PurgeMode::Zone),
        };
        let record = profile("Blog", Some(weekly.clone()));
        let draft = draft_of(&record, true);
        assert_eq!(draft.time.as_str(), "22:15");
        assert_eq!(draft.repeat, 2);
        assert_eq!(draft.purge, 3);
        assert_eq!(checked(&draft, None, true), Ok(Some(weekly.clone())));
        // On S3 there is nothing to purge: the schedule keeps the profile's (none).
        assert_eq!(checked(&draft, None, false).unwrap().unwrap().purge, None);
        // A profile never scheduled starts daily at 06:00, off.
        let fresh = draft_of(&profile("New", None), false);
        assert!(!fresh.enabled);
        assert_eq!((fresh.repeat, fresh.time.as_str()), (1, "06:00"));
    }

    #[test]
    fn save_says_what_is_wrong_and_off_keeps_the_fields() {
        let mut draft = draft_of(&profile("Blog", None), true);
        draft.enabled = true;
        draft.time = "25:00".into();
        assert_eq!(
            checked(&draft, None, true),
            Err(copy::SCHEDULE_TIME_INVALID.to_owned())
        );
        draft.time = "07:00".into();
        draft.repeat = 0;
        draft.every_hours = "0".into();
        assert_eq!(
            checked(&draft, None, true),
            Err(copy::SCHEDULE_HOURS_INVALID.to_owned())
        );
        draft.every_hours = "4".into();
        assert_eq!(
            checked(&draft, None, true).unwrap().unwrap().repeat,
            ScheduleRepeat::Hourly { every: 4 }
        );
        draft.repeat = 2;
        draft.days = ModelRc::new(VecModel::from(vec![false; 7]));
        assert_eq!(
            checked(&draft, None, true),
            Err(copy::SCHEDULE_DAYS_MISSING.to_owned())
        );
        // Off: what is wrong doesn't matter; what was stored is kept, off.
        draft.enabled = false;
        let before = daily(360);
        assert_eq!(
            checked(&draft, Some(&before), true),
            Ok(Some(Schedule {
                enabled: false,
                ..before
            }))
        );
        assert_eq!(checked(&draft, None, true), Ok(None));
    }

    #[test]
    fn the_task_runs_this_program_for_the_profile() {
        let mut schedule = daily(360);
        schedule.purge = Some(PurgeMode::Zone);
        let record = profile("Blog & News", Some(schedule.clone()));
        let task = task_with(
            &record,
            schedule,
            PathBuf::from("C:\\HTTPorter\\httporter.exe"),
        );
        assert_eq!(task.name, format!("Publish {}", record.id));
        let id = record.id.0.as_str();
        assert_eq!(
            task.args,
            [
                "--cli",
                "publish",
                "--profile",
                id,
                "--scheduled",
                "--purge",
                "all"
            ]
        );
        assert!(task.description.contains("Blog & News"));
    }

    #[test]
    fn put_task_follows_the_schedule() {
        let fake = FakeScheduler::new();
        let mut record = profile("Blog", Some(daily(360)));
        put_task(&fake, &record).unwrap();
        assert_eq!(fake.tasks().len(), 1);
        let name = win::task_name(&record.id);
        fake.set_next_run(&name, Some(local("2026-11-10 06:00")));
        assert_eq!(
            task_state(&fake, &name),
            TaskState::Present(Some(local("2026-11-10 06:00")))
        );
        record.extras.schedule = Some(Schedule {
            enabled: false,
            ..daily(360)
        });
        put_task(&fake, &record).unwrap();
        assert!(fake.tasks().is_empty());
        assert_eq!(task_state(&fake, &name), TaskState::Missing);
    }

    #[test]
    fn only_the_tasks_of_gone_profiles_are_removed() {
        let fake = FakeScheduler::new();
        let (kept, gone) = (
            profile("Kept", Some(daily(0))),
            profile("Gone", Some(daily(0))),
        );
        put_task(&fake, &kept).unwrap();
        put_task(&fake, &gone).unwrap();
        // A task in the folder that isn't a profile's is never touched.
        let other = ScheduledTask {
            name: "Something else".to_owned(),
            ..task_with(&kept, daily(0), PathBuf::from("x.exe"))
        };
        fake.upsert(&other).unwrap();
        let removed = remove_orphans(&fake, std::slice::from_ref(&kept.id));
        assert_eq!(removed, vec![win::task_name(&gone.id)]);
        let left: Vec<String> = fake.tasks().into_iter().map(|t| t.name).collect();
        assert_eq!(left, vec![win::task_name(&kept.id), other.name]);
    }

    #[test]
    fn the_card_says_the_next_run_and_the_last_scheduled_publish() {
        let now = local("2026-11-06 15:00");
        let s = daily(360);
        let (text, missing) = card_text(
            &s,
            &TaskState::Present(Some(local("2026-11-10 06:00"))),
            now,
        );
        assert_eq!(
            text.replace('\u{a0}', " "),
            "Daily at 06:00 · next Tue 06:00"
        );
        assert!(!missing);
        let (text, missing) = card_text(&s, &TaskState::Missing, now);
        assert_eq!(text.replace('\u{a0}', " "), "Daily at 06:00");
        assert!(missing);
        let (text, _) = card_text(
            &s,
            &TaskState::Present(Some(local("2026-11-06 18:00"))),
            now,
        );
        assert!(text.ends_with("next\u{a0}at\u{a0}18:00"), "{text}");
        let run = HistoryRecord {
            id: 1,
            profile: ProfileId::new(),
            started_at: local("2026-11-06 06:00").with_timezone(&Utc),
            finished_at: local("2026-11-06 06:01").with_timezone(&Utc),
            result: PublishResult::Ok,
            uploaded: 3,
            deleted: 0,
            skipped: 0,
            failed: 0,
            bytes: 0,
            purge: PurgeResult::Off,
            verified: true,
            remote_files: 0,
            remote_folders: 0,
            regions: 0,
            files: Vec::new(),
            source: HistorySource::Scheduled,
        };
        assert_eq!(
            copy::schedule_last(&run, now),
            "Last run: today 06:00 · 3 uploaded"
        );
    }
}
