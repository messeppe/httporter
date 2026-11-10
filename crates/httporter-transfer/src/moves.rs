// SPDX-License-Identifier: MIT OR Apache-2.0

//! Moving and renaming files and folders on a site: a server-side copy where the provider has
//! one, else download and upload again through the engine; the originals are deleted only once
//! every copy is made.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use futures_util::{StreamExt, stream};
use httporter_core::{
    EmptyFolders, Entry, Job, JournalError, ProviderError, RemotePath, StorageProvider, walk,
};

use crate::compare::rel_of;
use crate::messages;
use crate::publish::run_batch;
use crate::{BatchTarget, Engine};

/// The largest file one server-side copy request may take (S3 `CopyObject`: 5 GiB).
const COPY_MAX: u64 = 5 << 30;

/// Server-side copies in flight at once.
const COPIES_AT_ONCE: usize = 8;

/// One file to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveItem {
    pub from: RemotePath,
    pub to: RemotePath,
    pub size: u64,
}

/// What a move does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MovePlan {
    /// The files.
    pub items: Vec<MoveItem>,
    /// The folders, from and to.
    pub folders: Vec<(RemotePath, RemotePath)>,
    pub bytes: u64,
    /// `caps.server_side_copy` and every file ≤ 5 GiB.
    pub server_side: bool,
}

/// Plans moving `sources` into `to_dir`. Folders expand via `walk()`; `rename_to` is the new
/// last segment (one source). An existing target is `Conflict`; a folder into itself is refused.
///
/// Both are [`ProviderError::Refused`] with the engine's words: [`messages::MOVE_EXISTS`] (the
/// name filled in), [`messages::MOVE_INTO_ITSELF`], [`messages::MOVE_SAME_PLACE`],
/// [`messages::MOVE_BAD_NAME`] or [`messages::MOVE_RENAME_ONE`].
pub async fn plan_move(
    provider: &dyn StorageProvider,
    sources: &[Entry],
    to_dir: &RemotePath,
    rename_to: Option<&str>,
) -> Result<MovePlan, ProviderError> {
    let refused = |text: &str| ProviderError::Refused(text.to_owned());
    if rename_to.is_some() && sources.len() != 1 {
        return Err(refused(messages::MOVE_RENAME_ONE));
    }
    // What the destination holds now; a destination that isn't there yet holds nothing.
    let existing: HashSet<String> = match provider.list(to_dir).await {
        Ok(listing) => listing
            .entries
            .iter()
            .filter(|entry| entry.path.parent().as_ref() == Some(to_dir))
            .map(|entry| entry.name().to_owned())
            .collect(),
        Err(ProviderError::NotFound) => HashSet::new(),
        Err(error) => return Err(error),
    };

    let mut plan = MovePlan::default();
    let mut targets: HashSet<RemotePath> = HashSet::new();
    for source in sources {
        if source.path.is_root() {
            return Err(refused(messages::MOVE_INTO_ITSELF));
        }
        let name = rename_to.map(str::trim).unwrap_or(source.name());
        let target = to_dir
            .join(name)
            .map_err(|_| refused(messages::MOVE_BAD_NAME))?;
        if target == source.path {
            return Err(refused(messages::MOVE_SAME_PLACE));
        }
        if source.is_folder && to_dir.is_within(&source.path) {
            return Err(refused(messages::MOVE_INTO_ITSELF));
        }
        // Names are case-sensitive on both services, so "a" renamed "A" beside it is no clash.
        if existing.contains(name) || !targets.insert(target.clone()) {
            return Err(ProviderError::Refused(messages::move_exists(name)));
        }
        if !source.is_folder {
            plan.bytes += source.size;
            plan.items.push(MoveItem {
                from: source.path.clone(),
                to: target,
                size: source.size,
            });
            continue;
        }
        plan.folders.push((source.path.clone(), target.clone()));
        for entry in walk(provider, &source.path).await? {
            let Some(to) = rel_of(&entry.path, &source.path)
                .and_then(|rel| RemotePath::parse(&format!("{}/{rel}", target.as_str())).ok())
            else {
                continue;
            };
            if entry.is_folder {
                plan.folders.push((entry.path, to));
            } else {
                plan.bytes += entry.size;
                plan.items.push(MoveItem {
                    from: entry.path,
                    to,
                    size: entry.size,
                });
            }
        }
    }
    // Parents before their children.
    plan.folders.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()));
    plan.items
        .sort_by(|a, b| a.from.as_str().cmp(b.from.as_str()));
    plan.server_side = provider.capabilities().server_side_copy
        && plan.items.iter().all(|item| item.size <= COPY_MAX);
    Ok(plan)
}

/// About how long `plan` takes, in seconds, at these rates: a request per file (copy, then
/// delete), plus every byte down and up again when the copy can't be made on the server.
pub fn estimate_secs(plan: &MovePlan, files_per_s: f64, bytes_per_s: f64) -> u64 {
    let files = (plan.items.len() + plan.folders.len()) as f64;
    let per_file = if files_per_s > 0.0 {
        2.0 * files / files_per_s
    } else {
        0.0
    };
    let per_byte = if plan.server_side || bytes_per_s <= 0.0 {
        0.0
    } else {
        2.0 * plan.bytes as f64 / bytes_per_s
    };
    let secs = (per_file + per_byte).ceil();
    if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    }
}

/// Where a running move is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovePhase {
    Copying { done: u64, total: u64 },
    Deleting,
    Finished,
}

/// How a move ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveOutcome {
    pub copied: u64,
    pub deleted: u64,
    pub failed: Vec<(RemotePath, String)>,
    /// The copies made when some failed (the originals were kept).
    pub kept_copies: Vec<RemotePath>,
}

/// Runs `plan`. Server-side: `provider.copy`, 8 at a time. Else a download batch into `temp_dir`,
/// then an upload batch (checksums), each `submit_tracked` + `wait_batch`. One delete batch, only
/// if every copy succeeded; else nothing is deleted and `kept_copies` lists the copies. Bunny:
/// empty folders are created. `temp_dir` is emptied at the end.
///
/// The originals' folders go last, in a batch of their own: once their files are deleted, each
/// moved folder that holds no file any more (one added meanwhile is never deleted unseen) is
/// deleted with what is left in it (S3 markers, Bunny's empty folders).
pub async fn run_move(
    engine: &Engine,
    target: BatchTarget,
    plan: &MovePlan,
    temp_dir: &Path,
    on_phase: &(dyn Fn(MovePhase) + Send + Sync),
) -> Result<MoveOutcome, JournalError> {
    let total = plan.items.len() as u64;
    let mut outcome = MoveOutcome::default();
    on_phase(MovePhase::Copying { done: 0, total });

    let copied: Vec<RemotePath> = if plan.server_side {
        copy_on_server(engine, &target, plan, &mut outcome, on_phase).await
    } else {
        let copied = copy_through(engine, &target, plan, temp_dir, &mut outcome).await;
        empty_dir(temp_dir).await;
        copied?
    };
    outcome.copied = copied.len() as u64;
    on_phase(MovePhase::Copying {
        done: outcome.copied,
        total,
    });
    make_empty_folders(engine, &target, plan, &mut outcome).await;

    if !outcome.failed.is_empty() {
        outcome.kept_copies = copied;
        on_phase(MovePhase::Finished);
        return Ok(outcome);
    }

    on_phase(MovePhase::Deleting);
    let deletes: Vec<Job> = plan
        .items
        .iter()
        .map(|item| Job::Delete {
            remote: item.from.clone(),
            folder: false,
        })
        .collect();
    let down = run_batch(engine, &target, deletes).await?;
    outcome.deleted = down.done.len() as u64;
    // Bunny's listings lag behind its deletes: a file deleted a moment ago may still be listed.
    let just_deleted: HashSet<&RemotePath> = down.done.iter().collect();
    let files_gone = down.failed.is_empty() && down.cancelled.is_empty();
    outcome.failed.extend(down.failed);
    outcome.failed.extend(
        down.cancelled
            .into_iter()
            .map(|path| (path, messages::MOVE_CANCELLED.to_owned())),
    );

    if files_gone {
        let mut folders = Vec::new();
        for from in top_folders(plan) {
            match walk(&*target.provider, &from).await {
                Ok(left)
                    if left
                        .iter()
                        .all(|entry| entry.is_folder || just_deleted.contains(&entry.path)) =>
                {
                    folders.push(Job::Delete {
                        remote: from,
                        folder: true,
                    });
                }
                Ok(_) => outcome
                    .failed
                    .push((from, messages::MOVE_FOLDER_NOT_EMPTY.to_owned())),
                // Gone already: S3 drops a folder with its last file.
                Err(ProviderError::NotFound) => {}
                Err(error) => outcome.failed.push((from, engine.describe(&error))),
            }
        }
        let gone = run_batch(engine, &target, folders).await?;
        outcome.failed.extend(gone.failed);
    }
    on_phase(MovePhase::Finished);
    Ok(outcome)
}

/// `provider.copy` for every item, [`COPIES_AT_ONCE`] at a time. Answers the copies made; the
/// rest go to `outcome.failed`.
async fn copy_on_server(
    engine: &Engine,
    target: &BatchTarget,
    plan: &MovePlan,
    outcome: &mut MoveOutcome,
    on_phase: &(dyn Fn(MovePhase) + Send + Sync),
) -> Vec<RemotePath> {
    let provider = &*target.provider;
    let total = plan.items.len() as u64;
    // Owned items, so the stream's closure has no borrowed argument: a spawned `Send` future
    // that takes one fails rustc's higher-ranked lifetime check.
    let mut copies = stream::iter(plan.items.iter().cloned())
        .map(|item| async move {
            let result = provider.copy(&item.from, &item.to).await;
            (item, result)
        })
        .buffer_unordered(COPIES_AT_ONCE);
    let mut copied = Vec::new();
    while let Some((item, result)) = copies.next().await {
        match result {
            Ok(()) => copied.push(item.to),
            Err(error) => outcome.failed.push((item.from, engine.describe(&error))),
        }
        on_phase(MovePhase::Copying {
            done: copied.len() as u64,
            total,
        });
    }
    copied
}

/// Downloads every item into `temp_dir`, then uploads what arrived to its new place (with
/// checksums, as every upload). Answers the copies made; the rest go to `outcome.failed`. A
/// cancelled download stops the move before anything is uploaded.
async fn copy_through(
    engine: &Engine,
    target: &BatchTarget,
    plan: &MovePlan,
    temp_dir: &Path,
    outcome: &mut MoveOutcome,
) -> Result<Vec<RemotePath>, JournalError> {
    let _ = crate::task::spawn_blocking({
        let dir = temp_dir.to_owned();
        move || std::fs::create_dir_all(dir)
    })
    .await;
    let local = |index: usize| -> PathBuf { temp_dir.join(format!("{index}.part")) };
    let downloads: Vec<Job> = plan
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| Job::Download {
            remote: item.from.clone(),
            local: local(index),
            size: item.size,
        })
        .collect();
    let down = run_batch(engine, target, downloads).await?;
    let arrived: HashSet<&RemotePath> = down.done.iter().collect();
    outcome.failed.extend(down.failed.iter().cloned());
    if !down.cancelled.is_empty() {
        outcome.failed.extend(
            down.cancelled
                .iter()
                .map(|path| (path.clone(), messages::MOVE_CANCELLED.to_owned())),
        );
        return Ok(Vec::new());
    }

    let uploads: Vec<Job> = plan
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| arrived.contains(&item.from))
        .map(|(index, item)| Job::Upload {
            local: local(index),
            remote: item.to.clone(),
            size: item.size,
        })
        .collect();
    let up = run_batch(engine, target, uploads).await?;
    // Upload failures are reported by the original's path, as the download ones are.
    let from_of = |to: &RemotePath| {
        plan.items
            .iter()
            .find(|item| item.to == *to)
            .map_or_else(|| to.clone(), |item| item.from.clone())
    };
    outcome.failed.extend(
        up.failed
            .into_iter()
            .map(|(to, reason)| (from_of(&to), reason)),
    );
    outcome.failed.extend(
        up.cancelled
            .iter()
            .map(|to| (from_of(to), messages::MOVE_CANCELLED.to_owned())),
    );
    Ok(up.done)
}

/// Makes the moved folders that hold no file, where the service keeps empty folders: Bunny, and
/// S3's markers. A failure is noted, and keeps the originals.
async fn make_empty_folders(
    engine: &Engine,
    target: &BatchTarget,
    plan: &MovePlan,
    outcome: &mut MoveOutcome,
) {
    if target.provider.capabilities().empty_folders == EmptyFolders::Unsupported {
        return;
    }
    for (from, to) in &plan.folders {
        let holds_file = plan.items.iter().any(|item| item.to.is_within(to));
        let holds_folder = plan
            .folders
            .iter()
            .any(|(_, other)| other != to && other.is_within(to));
        if holds_file || holds_folder {
            continue;
        }
        if let Err(error) = target.provider.create_folder(to).await {
            outcome.failed.push((from.clone(), engine.describe(&error)));
        }
    }
}

/// The moved folders not inside another moved folder.
fn top_folders(plan: &MovePlan) -> Vec<RemotePath> {
    plan.folders
        .iter()
        .map(|(from, _)| from)
        .filter(|from| {
            !plan
                .folders
                .iter()
                .any(|(other, _)| other != *from && from.is_within(other))
        })
        .cloned()
        .collect()
}

/// Empties `dir` (the downloads a move made), best effort.
async fn empty_dir(dir: &Path) {
    let dir = dir.to_owned();
    let _ = crate::task::spawn_blocking(move || {
        let Ok(children) = std::fs::read_dir(&dir) else {
            return;
        };
        for child in children.flatten() {
            let path = child.path();
            let _ = if child.file_type().is_ok_and(|kind| kind.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    })
    .await;
}
