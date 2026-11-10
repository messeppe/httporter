// SPDX-License-Identifier: MIT OR Apache-2.0

//! The activity log's rotation and reading, the folder watcher, where a saved window may open,
//! and the journal's owners (I4 to I6).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use httporter_core::{
    ActivityEvent, ActivityFilter, ActivityKind, Job, JobRecord, JobState, Journal, RemotePath,
    SiteId, WindowPlacement,
};
use httporter_platform::{
    ActivityLog, LOG_FILE_BYTES, LOG_FILES, SqliteJournal, owner_alive, process_owner, win,
};

// --- Activity log -----------------------------------------------------------------------------

fn event(i: u32, ok: bool, detail: &str) -> ActivityEvent {
    ActivityEvent {
        at: Utc.with_ymd_and_hms(2026, 11, 6, 10, 0, 0).unwrap()
            + chrono::Duration::seconds(i.into()),
        kind: if ok {
            ActivityKind::Upload
        } else {
            ActivityKind::Delete
        },
        site: "Main".into(),
        path: format!("f{i}.txt"),
        ok,
        detail: detail.into(),
        bytes: u64::from(i),
    }
}

fn log_file(dir: &Path, index: usize) -> PathBuf {
    if index == 0 {
        dir.join("activity.log")
    } else {
        dir.join(format!("activity.{index}.log"))
    }
}

#[test]
fn the_activity_log_reads_newest_first_filtered_and_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let log = ActivityLog::open(&dir.path().join("logs")).unwrap();
    for i in 0..5 {
        log.append(&event(i, i % 2 == 0, "ok"));
    }
    log.append(&event(
        5,
        false,
        "PUT ?X-Amz-Signature=abc123&key=sekrit failed",
    ));
    let all = log.read(&ActivityFilter::default(), 100);
    let paths: Vec<&str> = all.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        ["f5.txt", "f4.txt", "f3.txt", "f2.txt", "f1.txt", "f0.txt"]
    );
    assert_eq!(all[0].detail, "PUT ?X-Amz-Signature=***&key=*** failed");
    let text = std::fs::read_to_string(log_file(log.dir(), 0)).unwrap();
    assert!(!text.contains("abc123") && !text.contains("sekrit"));

    assert_eq!(log.read(&ActivityFilter::default(), 2).len(), 2);
    assert!(log.read(&ActivityFilter::default(), 0).is_empty());
    let failed = ActivityFilter {
        failed_only: true,
        ..ActivityFilter::default()
    };
    let failed: Vec<String> = log.read(&failed, 100).into_iter().map(|e| e.path).collect();
    assert_eq!(failed, ["f5.txt", "f3.txt", "f1.txt"]);

    let out = dir.path().join("export.txt");
    assert_eq!(
        log.export_text(&ActivityFilter::default(), &out).unwrap(),
        6
    );
    let exported = std::fs::read_to_string(out).unwrap();
    assert!(
        exported
            .lines()
            .next()
            .unwrap()
            .contains("\tupload\tMain\tok\t0\tf0.txt\t")
    );
}

#[test]
fn the_activity_log_rotates_when_full_and_keeps_ten_files() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    // A current file just short of full, and nine older ones, each holding one event.
    let line = |i: u32| serde_json::to_string(&event(i, true, "")).unwrap() + "\n";
    let first = line(100);
    let filler = " ".repeat(LOG_FILE_BYTES as usize - first.len() - 10) + "\n";
    std::fs::write(log_file(&logs, 0), first + &filler).unwrap();
    for index in 1..LOG_FILES {
        std::fs::write(log_file(&logs, index), line(100 - index as u32)).unwrap();
    }

    let log = ActivityLog::open(&logs).unwrap();
    log.append(&event(200, true, ""));
    let current = std::fs::read_to_string(log_file(&logs, 0)).unwrap();
    assert_eq!(current, line(200), "a new current file");
    let older = std::fs::read_to_string(log_file(&logs, 1)).unwrap();
    assert!(older.starts_with(&line(100)), "the full file moved up");
    assert_eq!(
        std::fs::read_to_string(log_file(&logs, LOG_FILES - 1)).unwrap(),
        line(100 - (LOG_FILES as u32 - 2)),
        "the oldest went"
    );
    assert!(!log_file(&logs, LOG_FILES).exists());

    // Newest first across the files, the filler line passed over.
    let read: Vec<u64> = log
        .read(&ActivityFilter::default(), 4)
        .iter()
        .map(|e| e.bytes)
        .collect();
    assert_eq!(read, [200, 100, 99, 98]);
}

// --- Folder watcher ---------------------------------------------------------------------------

/// Waits up to 5 s for a change named `name`.
fn saw(changes: &mpsc::Receiver<PathBuf>, name: &str) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match changes.recv_timeout(left) {
            Ok(path) if path.file_name().is_some_and(|n| n == name) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

#[test]
fn the_watcher_sees_a_write_and_a_rename_replace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("page.html"), b"old").unwrap();
    let (tx, changes) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let watcher = win::DirWatcher::start(
        dir.path(),
        Box::new(move |path| {
            let _ = tx.lock().unwrap().send(path);
        }),
    )
    .unwrap();
    assert!(watcher.is_running());

    std::fs::write(dir.path().join("new.txt"), b"hello").unwrap();
    assert!(saw(&changes, "new.txt"), "a write");

    // An editor's save: a temporary file renamed over the original.
    let temp = dir.path().join("sub").join("page.html.tmp");
    std::fs::write(&temp, b"new").unwrap();
    std::fs::rename(&temp, dir.path().join("sub").join("page.html")).unwrap();
    assert!(saw(&changes, "page.html"), "a rename over a file below");
    drop(watcher);
}

// --- Window placement -------------------------------------------------------------------------

#[test]
fn a_saved_window_opens_only_where_a_monitor_is() {
    let home = win::default_placement(800, 600).expect("Windows says where the monitors are");
    assert_eq!(win::visible_placement(&home), Some(home));
    let maximized = WindowPlacement {
        maximized: true,
        ..home
    };
    assert_eq!(win::visible_placement(&maximized), Some(maximized));
    let unplugged = WindowPlacement {
        x: -100_000,
        y: -100_000,
        ..home
    };
    assert_eq!(win::visible_placement(&unplugged), None);
    let too_big = WindowPlacement {
        width: 100_000,
        height: 100_000,
        ..home
    };
    assert_eq!(win::visible_placement(&too_big), None);
    let title_bar_off = WindowPlacement {
        y: home.y - home.height as i32 - 100_000,
        ..home
    };
    assert_eq!(win::visible_placement(&title_bar_off), None);
}

// --- Journal owners ---------------------------------------------------------------------------

fn record(remote: &str) -> JobRecord {
    JobRecord {
        id: 0,
        batch: "b".into(),
        site: SiteId("s-1".into()),
        job: Job::Delete {
            remote: RemotePath::parse(remote).unwrap(),
            folder: false,
        },
        state: JobState::Queued,
        error: None,
        multipart: None,
    }
}

fn remotes(journal: &SqliteJournal) -> Vec<String> {
    journal
        .unfinished()
        .unwrap()
        .into_iter()
        .map(|r| match r.job {
            Job::Delete { remote, .. } => remote.as_str().to_owned(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn a_journal_leaves_the_rows_of_another_live_owner_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("httporter.db");
    // This test process stands for the window, which is still running.
    let window = SqliteJournal::open(&path)
        .unwrap()
        .with_owner(process_owner());
    window.insert(&mut [record("window.txt")]).unwrap();
    // A run that has ended, and a row from before owners were written.
    SqliteJournal::open(&path)
        .unwrap()
        .with_owner("4294967294:1".into())
        .insert(&mut [record("gone.txt")])
        .unwrap();
    SqliteJournal::open(&path)
        .unwrap()
        .insert(&mut [record("old.txt")])
        .unwrap();

    let cli = SqliteJournal::open(&path)
        .unwrap()
        .with_owner("4294967293:2".into());
    assert_eq!(remotes(&cli), ["gone.txt", "old.txt"], "not the window's");
    assert_eq!(
        remotes(&window),
        ["window.txt", "gone.txt", "old.txt"],
        "its own and the gone ones"
    );
    cli.discard_unfinished().unwrap();
    assert!(remotes(&cli).is_empty());
    assert_eq!(
        remotes(&window),
        ["window.txt"],
        "the discard left it alone"
    );

    assert!(owner_alive(&process_owner()));
    assert!(!owner_alive("4294967294:1"));
    assert!(
        !owner_alive(&format!("{}:1", std::process::id())),
        "another start time"
    );
    assert!(!owner_alive("not an owner"));
}

// --- The profile's publish lock (a busy profile) ----------------------------------------------

#[test]
fn one_publish_per_profile_at_a_time() {
    let profile = httporter_core::ProfileId::new();
    let held = win::ProfileLock::try_acquire(&profile).expect("free at first");
    assert!(win::ProfileLock::try_acquire(&profile).is_none(), "busy");
    assert!(win::ProfileLock::acquire_wait(&profile, Duration::from_millis(50)).is_none());
    let other = httporter_core::ProfileId::new();
    assert!(
        win::ProfileLock::try_acquire(&other).is_some(),
        "per profile"
    );

    // A wait ends once the holder lets go.
    let waiter = std::thread::spawn({
        let profile = profile.clone();
        move || win::ProfileLock::acquire_wait(&profile, Duration::from_secs(10)).is_some()
    });
    std::thread::sleep(Duration::from_millis(100));
    drop(held);
    assert!(waiter.join().unwrap());
    assert!(win::ProfileLock::try_acquire(&profile).is_some());
}
