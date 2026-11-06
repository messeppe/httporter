// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scheduled publish (§8.8.2): one Windows Task Scheduler task per profile, in the folder
//! `\HTTPorter\`, named `Publish <profile id>`. It runs `httporter.exe --cli publish --profile
//! <id> --scheduled` as the signed-in user with an interactive token, so no password is stored.
//!
//! [`TaskSchedulerCom`] talks to the real Task Scheduler through its COM API (`ITaskService`),
//! which doesn't depend on the Windows display language the way parsing `schtasks` output would.
//! [`FakeScheduler`] keeps its tasks in memory, for tests: nothing here may create a real task
//! while HTTPorter is being built.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Local};
use httporter_core::{ProfileId, Schedule};

/// The Task Scheduler folder HTTPorter's tasks live in.
pub const TASK_FOLDER: &str = "\\HTTPorter";

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
    /// The names of every task in [`TASK_FOLDER`] (none if the folder doesn't exist). Not in the
    /// plan's §0: T3 needs it to remove the tasks of profiles that no longer exist.
    fn list(&self) -> Result<Vec<String>, String>;
}

/// The real Task Scheduler, through `ITaskService` (T3).
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskSchedulerCom;

impl Scheduler for TaskSchedulerCom {
    fn upsert(&self, _task: &ScheduledTask) -> Result<(), String> {
        todo!("T3")
    }

    fn remove(&self, _name: &str) -> Result<(), String> {
        todo!("T3")
    }

    fn next_run(&self, _name: &str) -> Result<Option<DateTime<Local>>, String> {
        todo!("T3")
    }

    fn exists(&self, _name: &str) -> Result<bool, String> {
        todo!("T3")
    }

    fn list(&self) -> Result<Vec<String>, String> {
        todo!("T3")
    }
}

/// Tasks in memory, for tests. `next_run` is whatever [`FakeScheduler::set_next_run`] last said
/// for the task (`None` until then, and for a disabled schedule).
#[derive(Debug, Default)]
pub struct FakeScheduler {
    tasks: Mutex<BTreeMap<String, ScheduledTask>>,
    next: Mutex<BTreeMap<String, DateTime<Local>>>,
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

    fn list(&self) -> Result<Vec<String>, String> {
        Ok(lock(&self.tasks).keys().cloned().collect())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
