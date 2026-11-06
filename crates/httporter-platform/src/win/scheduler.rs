// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduled publish (§8.8.2): one Windows Task Scheduler task per profile, in the folder
//! `\HTTPorter\`, named `Publish <profile id>`. It runs `httporter.exe --cli publish --profile
//! <id> --scheduled` as the signed-in user with an interactive token, so no password is stored.
//!
//! [`TaskSchedulerCom`] talks to the real Task Scheduler through its COM API (`ITaskService`),
//! which doesn't depend on the Windows display language the way parsing `schtasks` output would.
//! [`FakeScheduler`] keeps its tasks in memory, for tests: nothing here may create a real task
//! while HTTPorter is being built.
//!
//! **The task as written** ([`task_xml`], registered with `ITaskFolder::RegisterTask`):
//! - principal: `InteractiveToken`, least privilege, no user named (the user who saves it);
//! - settings: start when available (a run missed while the PC was off or the user signed out
//!   runs at the next chance), runs on battery and keeps running when the PC goes on battery,
//!   a 4 hour limit, and a start while one still runs is ignored (`IgnoreNew`);
//! - triggers, at local time (they follow daylight saving): daily; weekly on the days chosen;
//!   hourly = a daily trigger at the start time repeating every N hours for the rest of that day,
//!   plus a one-time trigger for what is left of the current day's runs (so saving at 15:00 an
//!   "every 3 hours from 06:00" still runs at 18:00 and 21:00 today). Every trigger starts after
//!   the moment of saving, so saving never starts a run by itself;
//! - one action: the program and its arguments, in the program's own folder.
//!
//! **Threads:** every COM call runs on one thread of its own, started at the first call, with
//! COM initialised there once (multi-threaded apartment). The methods hand their work to it and
//! wait for the answer, so they block their caller: call them off the UI thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone};
use httporter_core::{ProfileId, Schedule, ScheduleRepeat};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::TaskScheduler::{
    IRegisteredTask, ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_ENUM_HIDDEN,
    TASK_LOGON_INTERACTIVE_TOKEN, TaskScheduler,
};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::core::{BSTR, HRESULT};

/// The Task Scheduler folder HTTPorter's tasks live in.
pub const TASK_FOLDER: &str = "\\HTTPorter";

/// `SCHED_S_TASK_RUNNING`: what [`Scheduler::last_run`] says of a run still going.
pub const TASK_STILL_RUNNING: i32 = 0x0004_1301;
/// `SCHED_S_TASK_HAS_NOT_RUN`: the task has never run ([`Scheduler::last_run`] answers `None`).
const TASK_NEVER_RAN: i32 = 0x0004_1303;

/// The name of `profile`'s scheduled task (inside [`TASK_FOLDER`]).
pub fn task_name(profile: &ProfileId) -> String {
    format!("Publish {profile}")
}

/// A Task Scheduler task as HTTPorter writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTask {
    /// `"Publish <profile id>"` ([`task_name`]).
    pub name: String,
    pub description: String,
    /// The program the task runs (`httporter.exe`).
    pub exe: PathBuf,
    /// Its arguments (`--cli publish --profile <id> --scheduled`). Never a secret.
    pub args: Vec<String>,
    pub schedule: Schedule,
}

/// Where scheduled tasks are kept. Every name is a task's name inside [`TASK_FOLDER`]; errors are
/// Windows' own words, for the sheet and the activity log.
pub trait Scheduler: Send + Sync {
    /// Creates the task, or replaces the one of that name.
    fn upsert(&self, task: &ScheduledTask) -> Result<(), String>;
    /// Deletes the task; one that isn't there is not an error.
    fn remove(&self, name: &str) -> Result<(), String>;
    /// When the task runs next; `None` when it won't (disabled, or no run left).
    fn next_run(&self, name: &str) -> Result<Option<DateTime<Local>>, String>;
    /// Whether the task exists (the user may have deleted it in Task Scheduler).
    fn exists(&self, name: &str) -> Result<bool, String>;
    /// When the task last started and what it ended with (the program's exit code, or Task
    /// Scheduler's own code, such as [`TASK_STILL_RUNNING`]); `None` when it has never run or
    /// isn't there. Not in the plan's §0: a scheduled run with nothing to publish leaves no
    /// history row, so the card reads the last run here (controller, after T2).
    fn last_run(&self, name: &str) -> Result<Option<(DateTime<Local>, i32)>, String>;
    /// The names of every task in [`TASK_FOLDER`] (none if the folder doesn't exist). Not in the
    /// plan's §0: T3 needs it to remove the tasks of profiles that no longer exist.
    fn list(&self) -> Result<Vec<String>, String>;
}

/// The real Task Scheduler, through `ITaskService`: the folder [`TASK_FOLDER`] (created at the
/// first task, removed again with the last one), the current user, an interactive token. Every
/// method blocks until Task Scheduler has answered: call it off the UI thread.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskSchedulerCom;

impl Scheduler for TaskSchedulerCom {
    fn upsert(&self, task: &ScheduledTask) -> Result<(), String> {
        // A BSTR can't cross threads: the texts go as Strings and become BSTRs over there.
        let (name, xml) = (
            task.name.clone(),
            task_xml(task, Local::now().naive_local()),
        );
        on_com_thread(move |service| {
            let (name, xml) = (BSTR::from(name.as_str()), BSTR::from(xml.as_str()));
            let folder = our_folder(service)?;
            let empty = VARIANT::default();
            // SAFETY: COM calls on interfaces this thread holds, with arguments that outlive them.
            unsafe {
                folder.RegisterTask(
                    &name,
                    &xml,
                    TASK_CREATE_OR_UPDATE.0,
                    &empty,
                    &empty,
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &empty,
                )
            }
            .map(drop)
        })
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        let name = name.to_owned();
        on_com_thread(move |service| {
            let name = BSTR::from(name.as_str());
            let Some(folder) = existing_folder(service)? else {
                return Ok(());
            };
            // SAFETY: as in `upsert`.
            match unsafe { folder.DeleteTask(&name, 0) } {
                Err(e) if not_found(e.code()) => {}
                other => other?,
            }
            remove_folder_if_empty(service, &folder);
            Ok(())
        })
    }

    fn next_run(&self, name: &str) -> Result<Option<DateTime<Local>>, String> {
        let name = name.to_owned();
        on_com_thread(move |service| {
            let Some(task) = existing_task(service, &BSTR::from(name.as_str()))? else {
                return Ok(None);
            };
            // SAFETY: as in `upsert`.
            let enabled = unsafe { task.Enabled() }?.as_bool();
            if !enabled {
                return Ok(None);
            }
            // SAFETY: as in `upsert`.
            let next = unsafe { task.NextRunTime() }?;
            Ok(from_ole_date(next).and_then(|at| Local.from_local_datetime(&at).earliest()))
        })
    }

    fn exists(&self, name: &str) -> Result<bool, String> {
        let name = name.to_owned();
        on_com_thread(move |service| {
            Ok(existing_task(service, &BSTR::from(name.as_str()))?.is_some())
        })
    }

    fn last_run(&self, name: &str) -> Result<Option<(DateTime<Local>, i32)>, String> {
        let name = name.to_owned();
        on_com_thread(move |service| {
            let Some(task) = existing_task(service, &BSTR::from(name.as_str()))? else {
                return Ok(None);
            };
            // SAFETY: as in `upsert`.
            let (at, code) = unsafe { (task.LastRunTime()?, task.LastTaskResult()?) };
            Ok(last_run_of(at, code))
        })
    }

    fn list(&self) -> Result<Vec<String>, String> {
        on_com_thread(|service| {
            let Some(folder) = existing_folder(service)? else {
                return Ok(Vec::new());
            };
            // SAFETY: as in `upsert`.
            let tasks = unsafe { folder.GetTasks(TASK_ENUM_HIDDEN.0) }?;
            // SAFETY: as in `upsert`.
            let count = unsafe { tasks.Count() }?;
            let mut names = Vec::new();
            // The collection counts from 1.
            for index in 1..=count {
                // SAFETY: as in `upsert`; the index is within the count just read.
                let task = unsafe { tasks.get_Item(&variant_i32(index)) }?;
                // SAFETY: as in `upsert`.
                names.push(unsafe { task.Name() }?.to_string());
            }
            Ok(names)
        })
    }
}

// ---- The COM thread ----

/// Work for the COM thread.
type Job = Box<dyn FnOnce() + Send>;

/// How long a caller waits for Task Scheduler to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(30);

/// The thread every Task Scheduler call runs on, started at the first call. `None` if it couldn't
/// be started.
fn com_thread() -> Option<&'static mpsc::Sender<Job>> {
    static JOBS: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
    JOBS.get_or_init(|| {
        let (jobs, inbox) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("httporter-task-scheduler".to_owned())
            .spawn(move || {
                // Once, for the life of the thread (and of the process: it is never balanced, as
                // the thread only ends with the process). A failure shows as the calls' errors.
                // SAFETY: plain call on this new thread.
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                for job in inbox {
                    // A job that panics loses only its own answer; the thread goes on.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                }
            })
            .ok()
            .map(|_| jobs)
    })
    .as_ref()
}

/// Runs `work` on the COM thread with a connected `ITaskService`, and waits for its answer.
fn on_com_thread<T: Send + 'static>(
    work: impl FnOnce(&ITaskService) -> windows::core::Result<T> + Send + 'static,
) -> Result<T, String> {
    let (answer, answered) = mpsc::sync_channel(1);
    let job: Job = Box::new(move || {
        let result = connect().and_then(|service| work(&service));
        let _ = answer.send(result.map_err(|e| e.to_string()));
    });
    com_thread()
        .ok_or_else(|| NOT_STARTED.to_owned())?
        .send(job)
        .map_err(|_| NOT_STARTED.to_owned())?;
    answered
        .recv_timeout(ANSWER_WAIT)
        .map_err(|_| NO_ANSWER.to_owned())?
}

/// Errors of our own (everything else is Windows' words).
const NOT_STARTED: &str = "Task Scheduler couldn't be reached.";
const NO_ANSWER: &str = "Task Scheduler didn't answer.";

/// The Task Scheduler service on this PC, as the current user.
fn connect() -> windows::core::Result<ITaskService> {
    // SAFETY: plain COM creation on the COM thread; the empty variants mean "this PC, this user".
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let empty = VARIANT::default();
        service.Connect(&empty, &empty, &empty, &empty)?;
        Ok(service)
    }
}

/// [`TASK_FOLDER`], if it exists.
fn existing_folder(service: &ITaskService) -> windows::core::Result<Option<ITaskFolder>> {
    // SAFETY: as in `upsert`.
    match unsafe { service.GetFolder(&BSTR::from(TASK_FOLDER)) } {
        Ok(folder) => Ok(Some(folder)),
        Err(e) if not_found(e.code()) => Ok(None),
        Err(e) => Err(e),
    }
}

/// [`TASK_FOLDER`], created if it doesn't exist yet.
fn our_folder(service: &ITaskService) -> windows::core::Result<ITaskFolder> {
    if let Some(folder) = existing_folder(service)? {
        return Ok(folder);
    }
    // SAFETY: as in `upsert`. The empty SDDL gives the folder the root folder's security.
    let created = unsafe {
        let root = service.GetFolder(&BSTR::from("\\"))?;
        root.CreateFolder(
            &BSTR::from(TASK_FOLDER.trim_start_matches('\\')),
            &VARIANT::default(),
        )
    };
    match created {
        Ok(folder) => Ok(folder),
        // Made meanwhile by another HTTPorter.
        Err(e) if e.code() == ALREADY_EXISTS => existing_folder(service)?.ok_or_else(|| e.clone()),
        Err(e) => Err(e),
    }
}

/// The task `name` in [`TASK_FOLDER`], if both exist.
fn existing_task(
    service: &ITaskService,
    name: &BSTR,
) -> windows::core::Result<Option<IRegisteredTask>> {
    let Some(folder) = existing_folder(service)? else {
        return Ok(None);
    };
    // SAFETY: as in `upsert`.
    match unsafe { folder.GetTask(name) } {
        Ok(task) => Ok(Some(task)),
        Err(e) if not_found(e.code()) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Removes [`TASK_FOLDER`] once it holds no task (hidden ones counted) and no folder, so that
/// turning the last schedule off leaves nothing behind. Best effort: a failure keeps the folder.
fn remove_folder_if_empty(service: &ITaskService, folder: &ITaskFolder) {
    // SAFETY: as in `upsert`.
    let empty = unsafe {
        let tasks = folder.GetTasks(TASK_ENUM_HIDDEN.0).and_then(|t| t.Count());
        let folders = folder.GetFolders(0).and_then(|f| f.Count());
        matches!((tasks, folders), (Ok(0), Ok(0)))
    };
    if empty {
        // SAFETY: as in `upsert`.
        let _ = unsafe {
            service.GetFolder(&BSTR::from("\\")).and_then(|root| {
                root.DeleteFolder(&BSTR::from(TASK_FOLDER.trim_start_matches('\\')), 0)
            })
        };
    }
}

/// `HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)` and `(ERROR_PATH_NOT_FOUND)`: what Task Scheduler
/// answers for a folder or task that isn't there.
fn not_found(code: HRESULT) -> bool {
    code == HRESULT(0x8007_0002_u32 as i32) || code == HRESULT(0x8007_0003_u32 as i32)
}

/// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`.
const ALREADY_EXISTS: HRESULT = HRESULT(0x8007_00B7_u32 as i32);

/// A `VT_I4` variant (a collection index).
fn variant_i32(n: i32) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: a zeroed VARIANT is VT_EMPTY; this sets its type and its plain 32-bit value, which
    // own nothing (no drop is needed).
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = n;
    }
    v
}

/// An OLE Automation date (days since 30 Dec 1899, local time; what `NextRunTime` answers) as a
/// local date and time. `None` for 0 and below, which Task Scheduler answers for "no next run".
fn from_ole_date(date: f64) -> Option<NaiveDateTime> {
    if !date.is_finite() || date <= 0.0 {
        return None;
    }
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30)?.and_time(NaiveTime::MIN);
    // Whole seconds: Task Scheduler's times have no finer part.
    let seconds = (date * 86_400.0).round();
    if seconds > i64::MAX as f64 {
        return None;
    }
    epoch.checked_add_signed(TimeDelta::try_seconds(seconds as i64)?)
}

/// What `LastRunTime` and `LastTaskResult` say, as [`Scheduler::last_run`] answers it.
fn last_run_of(at: f64, code: i32) -> Option<(DateTime<Local>, i32)> {
    if code == TASK_NEVER_RAN {
        return None;
    }
    let at = Local.from_local_datetime(&from_ole_date(at)?).earliest()?;
    Some((at, code))
}

// ---- The task's definition (pure; the unit tests pin it) ----

/// When a trigger fires, from its start (local time).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TriggerKind {
    /// Once, at the start.
    Once,
    /// Every day at the start's time.
    Daily,
    /// On these days (Monday first) at the start's time, every week.
    Weekly([bool; 7]),
}

/// One trigger of a task.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Trigger {
    kind: TriggerKind,
    /// The first time it may fire (`StartBoundary`), local; always after the moment of saving.
    start: NaiveDateTime,
    /// After each firing, again every `.0` minutes for `.1` minutes.
    repeat: Option<(u32, u32)>,
}

/// The triggers that run `schedule` from `now` on (local time). See the module's notes.
fn triggers(schedule: &Schedule, now: NaiveDateTime) -> Vec<Trigger> {
    let minutes = u32::from(schedule.at_minutes.min(24 * 60 - 1));
    let at = NaiveTime::from_hms_opt(minutes / 60, minutes % 60, 0).unwrap_or(NaiveTime::MIN);
    let today = now.date().and_time(at);
    // The latest start not after `now`, and the first one after it.
    let base = if today <= now {
        today
    } else {
        today - TimeDelta::days(1)
    };
    let next = base + TimeDelta::days(1);
    match &schedule.repeat {
        ScheduleRepeat::Daily => vec![Trigger {
            kind: TriggerKind::Daily,
            start: next,
            repeat: None,
        }],
        ScheduleRepeat::Weekly { days } if days.iter().any(|day| *day) => vec![Trigger {
            kind: TriggerKind::Weekly(*days),
            start: next,
            repeat: None,
        }],
        // No day chosen (the app doesn't save that): every day rather than never.
        ScheduleRepeat::Weekly { .. } => vec![Trigger {
            kind: TriggerKind::Daily,
            start: next,
            repeat: None,
        }],
        ScheduleRepeat::Hourly { every } => {
            let every = u32::from((*every).clamp(1, 24));
            if every == 24 {
                return vec![Trigger {
                    kind: TriggerKind::Daily,
                    start: next,
                    repeat: None,
                }];
            }
            // The runs of one day: the start, then every `every` hours until the next start.
            let runs = 24_u32.div_ceil(every);
            let interval = every * 60;
            // The duration ends a minute after the day's last run, so that the next day's start
            // is never also a repetition (whether the end counts or not).
            let repeat = |count: u32| (count >= 2).then(|| (interval, (count - 1) * interval + 1));
            let mut out = vec![Trigger {
                kind: TriggerKind::Daily,
                start: next,
                repeat: repeat(runs),
            }];
            // What is left of the current day's runs, from the first one after `now`.
            let left: Vec<NaiveDateTime> = (0..runs)
                .map(|k| base + TimeDelta::minutes(i64::from(k * interval)))
                .filter(|run| *run > now)
                .collect();
            if let Some(first) = left.first() {
                out.push(Trigger {
                    kind: TriggerKind::Once,
                    start: *first,
                    repeat: repeat(u32::try_from(left.len()).unwrap_or(u32::MAX)),
                });
            }
            out
        }
    }
}

/// `minutes` as an XML duration: `PT3H`, `PT20H1M`, `PT1M`.
fn duration(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("PT{m}M"),
        (h, 0) => format!("PT{h}H"),
        (h, m) => format!("PT{h}H{m}M"),
    }
}

/// `text` safe inside an XML element.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control characters can't be in XML 1.0 at all (a name never has them).
            c if c.is_control() && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// One argument as Windows' command-line parsing reads it back (`CommandLineToArgvW`): as it is
/// when it has no space, tab or quote; otherwise quoted, with quotes and the backslashes before
/// them escaped.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// A path as Task Scheduler takes it: without the `\\?\` prefix of a long local path.
fn plain_path(path: &Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix("\\\\?\\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_owned(),
        _ => text,
    }
}

/// The days of the week as Task Scheduler names them, Monday first (as `ScheduleRepeat::Weekly`).
const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

fn trigger_xml(out: &mut String, trigger: &Trigger) {
    let element = match trigger.kind {
        TriggerKind::Once => "TimeTrigger",
        TriggerKind::Daily | TriggerKind::Weekly(_) => "CalendarTrigger",
    };
    out.push_str(&format!("    <{element}>\n"));
    if let Some((interval, length)) = trigger.repeat {
        out.push_str("      <Repetition>\n");
        out.push_str(&format!(
            "        <Interval>{}</Interval>\n",
            duration(interval)
        ));
        out.push_str(&format!(
            "        <Duration>{}</Duration>\n",
            duration(length)
        ));
        out.push_str("        <StopAtDurationEnd>false</StopAtDurationEnd>\n");
        out.push_str("      </Repetition>\n");
    }
    out.push_str(&format!(
        "      <StartBoundary>{}</StartBoundary>\n",
        trigger.start.format("%Y-%m-%dT%H:%M:%S")
    ));
    out.push_str("      <Enabled>true</Enabled>\n");
    match &trigger.kind {
        TriggerKind::Once => {}
        TriggerKind::Daily => {
            out.push_str("      <ScheduleByDay>\n");
            out.push_str("        <DaysInterval>1</DaysInterval>\n");
            out.push_str("      </ScheduleByDay>\n");
        }
        TriggerKind::Weekly(days) => {
            out.push_str("      <ScheduleByWeek>\n");
            out.push_str("        <DaysOfWeek>\n");
            for (day, on) in WEEKDAYS.iter().zip(days) {
                if *on {
                    out.push_str(&format!("          <{day} />\n"));
                }
            }
            out.push_str("        </DaysOfWeek>\n");
            out.push_str("        <WeeksInterval>1</WeeksInterval>\n");
            out.push_str("      </ScheduleByWeek>\n");
        }
    }
    out.push_str(&format!("    </{element}>\n"));
}

/// The task definition Task Scheduler is given for `task`, saved at `now` (local time). Holds the
/// program, its arguments and the description: never a secret.
fn task_xml(task: &ScheduledTask, now: NaiveDateTime) -> String {
    let mut x = String::new();
    // The text reaches Task Scheduler as a BSTR (UTF-16), which is what the declaration says.
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n");
    x.push_str(
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n",
    );
    x.push_str("  <RegistrationInfo>\n");
    x.push_str("    <Author>HTTPorter</Author>\n");
    x.push_str(&format!(
        "    <Description>{}</Description>\n",
        xml_escape(&task.description)
    ));
    x.push_str("  </RegistrationInfo>\n");
    x.push_str("  <Triggers>\n");
    for trigger in triggers(&task.schedule, now) {
        trigger_xml(&mut x, &trigger);
    }
    x.push_str("  </Triggers>\n");
    // No user named: Task Scheduler takes the user who registers it. An interactive token runs
    // only while that user is signed in, and needs no password.
    x.push_str("  <Principals>\n");
    x.push_str("    <Principal id=\"Author\">\n");
    x.push_str("      <LogonType>InteractiveToken</LogonType>\n");
    x.push_str("      <RunLevel>LeastPrivilege</RunLevel>\n");
    x.push_str("    </Principal>\n");
    x.push_str("  </Principals>\n");
    x.push_str("  <Settings>\n");
    x.push_str("    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n");
    x.push_str("    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n");
    x.push_str("    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n");
    x.push_str("    <AllowHardTerminate>true</AllowHardTerminate>\n");
    x.push_str("    <StartWhenAvailable>true</StartWhenAvailable>\n");
    x.push_str("    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n");
    x.push_str("    <AllowStartOnDemand>true</AllowStartOnDemand>\n");
    x.push_str(&format!(
        "    <Enabled>{}</Enabled>\n",
        task.schedule.enabled
    ));
    x.push_str("    <Hidden>false</Hidden>\n");
    x.push_str("    <RunOnlyIfIdle>false</RunOnlyIfIdle>\n");
    x.push_str("    <WakeToRun>false</WakeToRun>\n");
    x.push_str("    <ExecutionTimeLimit>PT4H</ExecutionTimeLimit>\n");
    x.push_str("    <Priority>7</Priority>\n");
    x.push_str("  </Settings>\n");
    x.push_str("  <Actions Context=\"Author\">\n");
    x.push_str("    <Exec>\n");
    x.push_str(&format!(
        "      <Command>{}</Command>\n",
        xml_escape(&plain_path(&task.exe))
    ));
    let args: Vec<String> = task.args.iter().map(|arg| quote_arg(arg)).collect();
    if !args.is_empty() {
        x.push_str(&format!(
            "      <Arguments>{}</Arguments>\n",
            xml_escape(&args.join(" "))
        ));
    }
    if let Some(dir) = task.exe.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        x.push_str(&format!(
            "      <WorkingDirectory>{}</WorkingDirectory>\n",
            xml_escape(&plain_path(dir))
        ));
    }
    x.push_str("    </Exec>\n");
    x.push_str("  </Actions>\n");
    x.push_str("</Task>\n");
    x
}

/// Tasks in memory, for tests. `next_run` is whatever [`FakeScheduler::set_next_run`] last said
/// for the task (`None` until then, and for a disabled schedule).
#[derive(Debug, Default)]
pub struct FakeScheduler {
    tasks: Mutex<BTreeMap<String, ScheduledTask>>,
    next: Mutex<BTreeMap<String, DateTime<Local>>>,
    last: Mutex<BTreeMap<String, (DateTime<Local>, i32)>>,
}

impl FakeScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every task, by name.
    pub fn tasks(&self) -> Vec<ScheduledTask> {
        lock(&self.tasks).values().cloned().collect()
    }

    /// What `next_run` reports for `name` from now on.
    pub fn set_next_run(&self, name: &str, at: Option<DateTime<Local>>) {
        let mut next = lock(&self.next);
        match at {
            Some(at) => next.insert(name.to_owned(), at),
            None => next.remove(name),
        };
    }

    /// What `last_run` reports for `name` from now on, while the task exists.
    pub fn set_last_run(&self, name: &str, run: Option<(DateTime<Local>, i32)>) {
        let mut last = lock(&self.last);
        match run {
            Some(run) => last.insert(name.to_owned(), run),
            None => last.remove(name),
        };
    }
}

impl Scheduler for FakeScheduler {
    fn upsert(&self, task: &ScheduledTask) -> Result<(), String> {
        lock(&self.tasks).insert(task.name.clone(), task.clone());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        lock(&self.tasks).remove(name);
        Ok(())
    }

    fn next_run(&self, name: &str) -> Result<Option<DateTime<Local>>, String> {
        let enabled = lock(&self.tasks)
            .get(name)
            .is_some_and(|task| task.schedule.enabled);
        Ok(enabled
            .then(|| lock(&self.next).get(name).copied())
            .flatten())
    }

    fn exists(&self, name: &str) -> Result<bool, String> {
        Ok(lock(&self.tasks).contains_key(name))
    }

    fn last_run(&self, name: &str) -> Result<Option<(DateTime<Local>, i32)>, String> {
        let exists = lock(&self.tasks).contains_key(name);
        Ok(exists
            .then(|| lock(&self.last).get(name).copied())
            .flatten())
    }

    fn list(&self) -> Result<Vec<String>, String> {
        Ok(lock(&self.tasks).keys().cloned().collect())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httporter_core::PurgeMode;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    fn schedule(repeat: ScheduleRepeat, at_minutes: u16) -> Schedule {
        Schedule {
            enabled: true,
            repeat,
            at_minutes,
            purge: None,
        }
    }

    fn task(schedule: Schedule) -> ScheduledTask {
        ScheduledTask {
            name: "Publish 1f0c".to_owned(),
            description: "Publishes \"Blog & News\" <daily>".to_owned(),
            exe: PathBuf::from("C:\\Program Files\\HTTPorter\\httporter.exe"),
            args: ["--cli", "publish", "--profile", "1f0c", "--scheduled"]
                .map(str::to_owned)
                .to_vec(),
            schedule,
        }
    }

    #[test]
    fn daily_starts_at_the_next_time_after_saving() {
        let daily = schedule(ScheduleRepeat::Daily, 6 * 60);
        // Before 06:00: today; at or after it: tomorrow (saving never starts a run).
        for (now, start) in [
            ("2026-11-06 05:59", "2026-11-06 06:00"),
            ("2026-11-06 06:00", "2026-11-07 06:00"),
            ("2026-11-06 15:00", "2026-11-07 06:00"),
        ] {
            assert_eq!(
                triggers(&daily, at(now)),
                vec![Trigger {
                    kind: TriggerKind::Daily,
                    start: at(start),
                    repeat: None
                }],
                "saved at {now}"
            );
        }
    }

    #[test]
    fn weekly_keeps_the_days_mask() {
        let days = [true, false, true, false, true, false, false];
        let weekly = schedule(ScheduleRepeat::Weekly { days }, 22 * 60 + 30);
        assert_eq!(
            triggers(&weekly, at("2026-11-06 23:00")),
            vec![Trigger {
                kind: TriggerKind::Weekly(days),
                start: at("2026-11-07 22:30"),
                repeat: None
            }]
        );
        let xml = task_xml(&task(weekly), at("2026-11-06 23:00"));
        assert!(xml.contains("<Monday />") && xml.contains("<Wednesday />"));
        assert!(xml.contains("<Friday />") && !xml.contains("<Tuesday />"));
        assert!(xml.contains("<WeeksInterval>1</WeeksInterval>"));
    }

    #[test]
    fn hourly_is_a_daily_trigger_repeating_plus_the_rest_of_today() {
        // Every 3 hours from 06:00, saved at 15:00: 18:00 and 21:00 are left of today's runs.
        let hourly = schedule(ScheduleRepeat::Hourly { every: 3 }, 6 * 60);
        assert_eq!(
            triggers(&hourly, at("2026-11-06 15:00")),
            vec![
                Trigger {
                    kind: TriggerKind::Daily,
                    start: at("2026-11-07 06:00"),
                    // 06:00 … 03:00: 8 runs, the duration a minute past the last.
                    repeat: Some((180, 7 * 180 + 1)),
                },
                Trigger {
                    kind: TriggerKind::Once,
                    start: at("2026-11-06 18:00"),
                    // 18, 21, 00, 03.
                    repeat: Some((180, 3 * 180 + 1)),
                },
            ]
        );
        // Saved before today's start: yesterday's day of runs may still have some left (every 5
        // hours from 06:00: 02:00 is yesterday's last run).
        let five = schedule(ScheduleRepeat::Hourly { every: 5 }, 6 * 60);
        assert_eq!(
            triggers(&five, at("2026-11-06 01:00")),
            vec![
                Trigger {
                    kind: TriggerKind::Daily,
                    start: at("2026-11-06 06:00"),
                    repeat: Some((300, 4 * 300 + 1)),
                },
                Trigger {
                    kind: TriggerKind::Once,
                    start: at("2026-11-06 02:00"),
                    repeat: None,
                },
            ]
        );
        // Every 24 hours is daily; and nothing left of today adds no one-time trigger.
        let day = schedule(ScheduleRepeat::Hourly { every: 24 }, 6 * 60);
        assert_eq!(triggers(&day, at("2026-11-06 15:00")).len(), 1);
        let late = schedule(ScheduleRepeat::Hourly { every: 12 }, 6 * 60);
        assert_eq!(triggers(&late, at("2026-11-06 19:00")).len(), 1);
    }

    #[test]
    fn the_xml_has_the_settings_the_plan_asks_for() {
        let mut daily = schedule(ScheduleRepeat::Daily, 6 * 60);
        daily.purge = Some(PurgeMode::Zone);
        let xml = task_xml(&task(daily.clone()), at("2026-11-06 15:00"));
        for part in [
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<StartWhenAvailable>true</StartWhenAvailable>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<ExecutionTimeLimit>PT4H</ExecutionTimeLimit>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<Enabled>true</Enabled>",
            "<StartBoundary>2026-11-07T06:00:00</StartBoundary>",
            "<Command>C:\\Program Files\\HTTPorter\\httporter.exe</Command>",
            "<Arguments>--cli publish --profile 1f0c --scheduled</Arguments>",
            "<WorkingDirectory>C:\\Program Files\\HTTPorter</WorkingDirectory>",
            "<Description>Publishes &quot;Blog &amp; News&quot; &lt;daily&gt;</Description>",
        ] {
            assert!(xml.contains(part), "{part} missing from\n{xml}");
        }
        // No user and no password are ever written.
        assert!(!xml.contains("<UserId>") && !xml.to_lowercase().contains("password"));
        // A schedule turned off is a disabled task.
        daily.enabled = false;
        let off = task_xml(&task(daily), at("2026-11-06 15:00"));
        assert!(off.contains("    <Enabled>false</Enabled>\n"));
    }

    #[test]
    fn durations_and_arguments_are_written_as_windows_reads_them() {
        assert_eq!(duration(1), "PT1M");
        assert_eq!(duration(180), "PT3H");
        assert_eq!(duration(1201), "PT20H1M");
        assert_eq!(quote_arg("--scheduled"), "--scheduled");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("my site"), "\"my site\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(
            quote_arg("C:\\dir with space\\"),
            "\"C:\\dir with space\\\\\""
        );
        assert_eq!(
            plain_path(Path::new("\\\\?\\C:\\HTTPorter\\httporter.exe")),
            "C:\\HTTPorter\\httporter.exe"
        );
        assert_eq!(
            plain_path(Path::new("\\\\?\\UNC\\server\\share\\h.exe")),
            "\\\\?\\UNC\\server\\share\\h.exe"
        );
    }

    #[test]
    fn ole_dates_read_as_local_times() {
        assert_eq!(from_ole_date(0.0), None);
        assert_eq!(from_ole_date(-1.0), None);
        assert_eq!(from_ole_date(f64::NAN), None);
        // 46_332 days after 30 Dec 1899 is 6 Nov 2026; .25 of a day is 06:00.
        assert_eq!(from_ole_date(46_332.25), Some(at("2026-11-06 06:00")));
        assert_eq!(from_ole_date(46_332.75), Some(at("2026-11-06 18:00")));
        // A task that never ran says so in its result; one that ran keeps the program's code.
        assert_eq!(last_run_of(0.0, TASK_NEVER_RAN), None);
        assert_eq!(last_run_of(46_332.25, TASK_NEVER_RAN), None);
        let (when, code) = last_run_of(46_332.25, 4).expect("a run");
        assert_eq!((when.naive_local(), code), (at("2026-11-06 06:00"), 4));
    }

    #[test]
    fn the_fake_forgets_and_lists_like_the_real_one() {
        let fake = FakeScheduler::new();
        let one = task(schedule(ScheduleRepeat::Daily, 0));
        fake.upsert(&one).unwrap();
        assert_eq!(fake.list().unwrap(), vec![one.name.clone()]);
        assert!(fake.exists(&one.name).unwrap());
        fake.remove(&one.name).unwrap();
        fake.remove(&one.name).unwrap();
        assert!(!fake.exists(&one.name).unwrap());
    }

    /// Reads the real Task Scheduler: lists `\HTTPorter` (which may not exist) and asks about a
    /// task that isn't there. Changes nothing. Ignored only because it needs the real service.
    #[test]
    #[ignore = "reads the real Task Scheduler"]
    fn listing_the_real_folder_changes_nothing() {
        let names = TaskSchedulerCom.list().expect("listed");
        let missing = format!("Publish no-such-profile-{}", std::process::id());
        assert!(!names.contains(&missing));
        assert!(!TaskSchedulerCom.exists(&missing).expect("asked"));
        assert_eq!(TaskSchedulerCom.next_run(&missing).expect("asked"), None);
        assert_eq!(TaskSchedulerCom.last_run(&missing).expect("asked"), None);
    }

    /// The live check of T10: creates `\HTTPorter\Publish test-<unix>`, runs it once (a console
    /// that exits at once), then deletes it. Never part of a normal test run.
    #[test]
    #[ignore = "creates, runs and deletes a real scheduled task (T10 only)"]
    fn a_real_task_is_created_run_and_removed() {
        let unix = chrono::Utc::now().timestamp();
        let name = format!("Publish test-{unix}");
        let system = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from("C:\\Windows"), PathBuf::from);
        let test = ScheduledTask {
            name: name.clone(),
            description: "HTTPorter live test; deleted at once.".to_owned(),
            exe: system.join("System32").join("cmd.exe"),
            args: ["/c", "exit", "0"].map(str::to_owned).to_vec(),
            schedule: schedule(ScheduleRepeat::Daily, 3 * 60),
        };
        TaskSchedulerCom.upsert(&test).expect("created");
        // Whatever happens next, the task is removed again before anything is asserted.
        let checked = (|| {
            let listed =
                TaskSchedulerCom.exists(&name)? && TaskSchedulerCom.list()?.contains(&name);
            let next = TaskSchedulerCom.next_run(&name)?;
            Ok::<_, String>((listed, next, run_and_wait(&name)?))
        })();
        TaskSchedulerCom.remove(&name).expect("removed");
        assert!(!TaskSchedulerCom.exists(&name).expect("asked"));
        let (listed, next, result) = checked.expect("asked Task Scheduler");
        assert!(listed, "the task is listed");
        assert!(
            next.is_some_and(|next| next > Local::now()),
            "next run {next:?}"
        );
        assert_eq!(result, 0, "the run's exit code");
    }

    /// Starts task `name` now and waits (25 s at most, within the COM thread's answer wait) until
    /// it has run; its exit code, or -1 if it didn't finish in time.
    fn run_and_wait(name: &str) -> Result<i32, String> {
        let name = name.to_owned();
        on_com_thread(move |service| {
            let name = BSTR::from(name.as_str());
            let gone = || windows::core::Error::from(HRESULT(0x8007_0002_u32 as i32));
            let task = existing_task(service, &name)?.ok_or_else(gone)?;
            // SAFETY: as in `upsert`.
            unsafe { task.Run(&VARIANT::default()) }?;
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(500));
                // Read again each time: a registered task's object may keep old values.
                let task = existing_task(service, &name)?.ok_or_else(gone)?;
                // SAFETY: as in `upsert`.
                let (ran, result) = unsafe { (task.LastRunTime()?, task.LastTaskResult()?) };
                if ran > 0.0 && result != TASK_STILL_RUNNING && result != TASK_NEVER_RAN {
                    return Ok(result);
                }
            }
            Ok(-1)
        })
    }
}
