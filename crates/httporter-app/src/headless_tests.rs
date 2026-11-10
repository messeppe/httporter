// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests of the headless publish (`publish::headless::run_headless`, I6): what the CLI, the
//! scheduled publish and the watch folder run, on a mock storage with a real engine, a journal in
//! memory and a site database in a temporary folder. The delete guard, the dry run, the
//! no-delete run, the profile's lock, read-only sites and refused sign-ins.
//!
//! The [`Bed`] is shared with the CLI's tests (`cli/commands.rs`), which run the commands over it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use httporter_core::{
    AuthHint, Capabilities, ChecksumKind, DeleteGuard, EmptyFolders, HeadlessOptions,
    HeadlessOutcome, HistoryRecord, HistorySource, MemoryJournal, MockOp, MockProvider,
    ProfileExtras, ProfileId, ProfileRecord, ProviderError, PurgeMode, RemotePath, SiteExtras,
    SiteRecord, exit,
};
use httporter_platform::{CredentialKey, CredentialStore, MemoryStore, Store};
use httporter_transfer::Engine;
use tokio::runtime::Runtime;

use crate::add_site::ValidSite;
use crate::publish::flow;
use crate::publish::headless::{HeadlessCtx, HeadlessError, run_headless};
use crate::session::Session;
use crate::session::fakes::{FakeFactory, fixed_now};

/// Bunny Storage as the mock plays it: SHA-256 in listings, deletes that take a folder whole,
/// and no empty folders.
fn bunny_like() -> Capabilities {
    Capabilities {
        checksum_in_listing: Some(ChecksumKind::Sha256),
        recursive_delete: true,
        empty_folders: EmptyFolders::Unsupported,
        ..Capabilities::default()
    }
}

pub(crate) fn rp(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

/// A Bunny site `testzone` on a mock storage, and a publish profile of a local folder to its
/// root that removes extras and doesn't purge: the CLI's context over them, as `HeadlessCtx::open`
/// makes it, with an engine and a journal in memory.
pub(crate) struct Bed {
    pub ctx: HeadlessCtx,
    pub mock: Arc<MockProvider>,
    pub creds: Arc<MemoryStore>,
    pub site: SiteRecord,
    pub profile: ProfileRecord,
    pub local: tempfile::TempDir,
    _data: tempfile::TempDir,
    rt: Option<Runtime>,
}

impl Drop for Bed {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// A bed whose storage holds `remote` (path, contents) and whose profile is named `name`.
pub(crate) fn bed(remote: &[(&str, &str)], name: &str) -> Bed {
    let rt = Runtime::new().unwrap();
    let mut mock = MockProvider::new(bunny_like());
    for (path, contents) in remote {
        mock = mock.with_file(path, contents.as_bytes());
    }
    let mock = Arc::new(mock);
    let creds = Arc::new(MemoryStore::default());
    let data = tempfile::tempdir().unwrap();
    let session = Session::new(
        Store::open(&data.path().join("sites.db")).unwrap(),
        creds.clone(),
        FakeFactory::new(mock.clone()),
        fixed_now,
    );
    let site = rt
        .block_on(session.save_site(&ValidSite {
            name: "testzone".into(),
            zone: "testzone".into(),
            region_code: "sg".into(),
            password: "pw".into(),
        }))
        .unwrap();
    let local = tempfile::tempdir().unwrap();
    let profile = ProfileRecord {
        id: ProfileId::new(),
        site: site.id.clone(),
        name: name.to_owned(),
        local_dir: local.path().to_path_buf(),
        remote_dir: RemotePath::root(),
        skip_rules: Vec::new(),
        remove_extras: true,
        purge: PurgeMode::Off,
        guard: DeleteGuard::default(),
        created_at: chrono::Utc::now(),
        last_published_at: None,
        cache_control: None,
        extras: ProfileExtras::default(),
    };
    rt.block_on(session.save_profile(profile.clone())).unwrap();
    let journal = Arc::new(MemoryJournal::new());
    let engine = Engine::new(
        rt.handle().clone(),
        journal.clone(),
        session.settings(),
        crate::copy::describe_transfer_error,
    );
    let ctx = HeadlessCtx::for_tests(Arc::new(session), engine, journal);
    Bed {
        ctx,
        mock,
        creds,
        site,
        profile,
        local,
        _data: data,
        rt: Some(rt),
    }
}

impl Bed {
    pub(crate) fn rt(&self) -> &Runtime {
        self.rt.as_ref().unwrap()
    }

    /// Puts a file in the profile's folder.
    pub(crate) fn file(&self, name: &str, contents: &str) -> std::path::PathBuf {
        let path = self.local.path().join(name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    pub(crate) fn stored(&self, path: &str) -> Option<String> {
        self.mock
            .contents(&rp(path))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The profile's publishes, newest first.
    pub(crate) fn history(&self) -> Vec<HistoryRecord> {
        self.rt().block_on(
            self.ctx
                .session
                .history(self.profile.id.clone(), String::new()),
        )
    }

    /// Takes the site's password away, as a wiped Credential Manager would.
    pub(crate) fn forget_password(&self) {
        self.creds
            .delete(&CredentialKey::site(&self.site.id))
            .unwrap();
        self.ctx.session.hub().invalidate(&self.site.id);
    }

    /// Publishes the profile headlessly as `opts` says.
    fn run(&self, opts: &HeadlessOptions) -> Result<HeadlessOutcome, HeadlessError> {
        self.rt()
            .block_on(run_headless(&self.ctx, &self.profile, opts, &|_| {}))
    }
}

/// How many delete calls the storage had.
pub(crate) fn deletes(mock: &MockProvider) -> usize {
    [
        MockOp::DeleteFile,
        MockOp::DeleteFolder,
        MockOp::DeleteBatch,
    ]
    .into_iter()
    .map(|op| mock.calls(op))
    .sum()
}

fn opts(source: HistorySource) -> HeadlessOptions {
    HeadlessOptions {
        dry_run: false,
        yes: false,
        no_delete: false,
        purge: None,
        source,
    }
}

fn cli() -> HeadlessOptions {
    opts(HistorySource::Cli)
}

/// Two local files (`a.txt` changed, `b.txt` new) and one remote file only (`old.txt`).
fn small_bed() -> Bed {
    let mut b = bed(&[("a.txt", "x"), ("old.txt", "old")], "Blog");
    // One delete of two remote files is past the default guard (10 %): let it through.
    b.profile.guard = DeleteGuard {
        files: 100,
        percent: 100,
    };
    b.file("a.txt", "aaa");
    b.file("b.txt", "bbbb");
    b
}

#[test]
fn a_dry_run_counts_and_changes_nothing() {
    let b = small_bed();
    let outcome = b.run(&HeadlessOptions {
        dry_run: true,
        ..cli()
    });
    assert_eq!(
        outcome,
        Ok(HeadlessOutcome::DryRun {
            new: 1,
            changed: 1,
            delete: 1,
            bytes: 7,
        })
    );
    assert_eq!(b.mock.calls(MockOp::Upload), 0);
    assert_eq!(deletes(&b.mock), 0);
    assert_eq!(b.stored("a.txt").as_deref(), Some("x"));
    assert!(b.history().is_empty(), "a dry run leaves no history");
    let plan = b.ctx.last_plan().expect("the dry run's plan");
    assert_eq!(plan.items.len(), 3);
}

#[test]
fn a_publish_uploads_deletes_and_records_who_started_it() {
    let b = small_bed();
    let outcome = b.run(&cli()).unwrap();
    let HeadlessOutcome::Done {
        uploaded,
        deleted,
        failed,
        bytes,
        ..
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!((uploaded, deleted, failed, bytes), (2, 1, 0, 7));
    assert_eq!(b.stored("a.txt").as_deref(), Some("aaa"));
    assert_eq!(b.stored("b.txt").as_deref(), Some("bbbb"));
    assert_eq!(b.stored("old.txt"), None);
    let history = b.history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].source, HistorySource::Cli);
    assert_eq!((history[0].uploaded, history[0].deleted), (2, 1));

    // Again: nothing to do.
    let again = b.run(&cli()).unwrap();
    assert!(
        matches!(
            again,
            HeadlessOutcome::Done {
                uploaded: 0,
                deleted: 0,
                failed: 0,
                ..
            }
        ),
        "{again:?}"
    );
}

#[test]
fn no_delete_uploads_and_keeps_what_only_the_site_has() {
    let b = small_bed();
    let outcome = b.run(&HeadlessOptions {
        no_delete: true,
        ..cli()
    });
    assert!(
        matches!(
            outcome,
            Ok(HeadlessOutcome::Done {
                uploaded: 2,
                deleted: 0,
                failed: 0,
                ..
            })
        ),
        "{outcome:?}"
    );
    assert_eq!(b.stored("old.txt").as_deref(), Some("old"));
    assert_eq!(deletes(&b.mock), 0);
}

/// Ten files only on the site and one new local file: deleting 10 of 10 trips the guard.
fn guarded_bed() -> Bed {
    let old: Vec<(String, &str)> = (0..10).map(|i| (format!("old{i}.txt"), "old")).collect();
    let remote: Vec<(&str, &str)> = old.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    let b = bed(&remote, "Blog");
    b.file("new.txt", "new");
    b
}

#[test]
fn the_guard_holds_the_deletes_back_and_the_uploads_still_run() {
    let b = guarded_bed();
    assert_eq!(
        b.run(&cli()),
        Ok(HeadlessOutcome::GuardTripped {
            would_delete: 10,
            uploaded: 1,
        })
    );
    assert_eq!(b.stored("new.txt").as_deref(), Some("new"));
    assert_eq!(deletes(&b.mock), 0);
    assert!((0..10).all(|i| b.stored(&format!("old{i}.txt")).is_some()));
    // A dry run says the same would happen, and how many deletes wait.
    let dry = b.run(&HeadlessOptions {
        dry_run: true,
        ..cli()
    });
    assert!(
        matches!(dry, Ok(HeadlessOutcome::DryRun { delete: 10, .. })),
        "{dry:?}"
    );
    // Past the guard with --yes.
    let yes = b.run(&HeadlessOptions { yes: true, ..cli() });
    assert!(
        matches!(
            yes,
            Ok(HeadlessOutcome::Done {
                deleted: 10,
                failed: 0,
                ..
            })
        ),
        "{yes:?}"
    );
    assert!((0..10).all(|i| b.stored(&format!("old{i}.txt")).is_none()));
}

#[test]
fn unattended_runs_never_pass_the_guard() {
    for source in [HistorySource::Scheduled, HistorySource::Watch] {
        let b = guarded_bed();
        assert_eq!(
            b.run(&opts(source)),
            Ok(HeadlessOutcome::GuardTripped {
                would_delete: 10,
                uploaded: 1,
            }),
            "{source:?}"
        );
        assert_eq!(b.stored("new.txt").as_deref(), Some("new"));
        assert_eq!(deletes(&b.mock), 0);
        let history = b.history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].source, source);
        assert_eq!(history[0].deleted, 0);
    }
}

#[test]
fn a_profile_another_publish_holds_is_busy() {
    let b = small_bed();
    let held = b
        .rt()
        .block_on(flow::lock_profile(&b.profile.id, Duration::ZERO))
        .expect("the lock is free");
    assert_eq!(b.run(&cli()), Ok(HeadlessOutcome::Busy));
    assert_eq!(b.mock.calls(MockOp::Upload), 0);
    assert!(b.history().is_empty());
    // A dry run changes nothing, so it needs no lock.
    let dry = b.run(&HeadlessOptions {
        dry_run: true,
        ..cli()
    });
    assert!(matches!(dry, Ok(HeadlessOutcome::DryRun { .. })), "{dry:?}");
    drop(held);
    assert!(matches!(
        b.run(&cli()),
        Ok(HeadlessOutcome::Done { uploaded: 2, .. })
    ));
    // The run let go of the lock when it ended.
    assert!(
        b.rt()
            .block_on(flow::lock_profile(&b.profile.id, Duration::ZERO))
            .is_some()
    );
}

#[test]
fn a_read_only_site_refuses_a_publish_but_not_a_dry_run() {
    let b = small_bed();
    b.rt()
        .block_on(b.ctx.session.save_site_extras(
            b.site.id.clone(),
            SiteExtras {
                read_only: true,
                ..SiteExtras::default()
            },
        ))
        .unwrap();
    let refused = b.run(&cli()).unwrap_err();
    assert_eq!(refused.exit_code(), exit::CONFIG);
    assert_eq!(b.mock.calls(MockOp::Upload), 0);
    let dry = b.run(&HeadlessOptions {
        dry_run: true,
        ..cli()
    });
    assert!(matches!(dry, Ok(HeadlessOutcome::DryRun { .. })), "{dry:?}");
}

#[test]
fn a_missing_or_refused_password_ends_at_once_with_exit_3() {
    let b = small_bed();
    b.forget_password();
    let started = Instant::now();
    let error = b.run(&cli()).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(error, HeadlessError::Auth(_)), "{error:?}");
    assert_eq!(error.exit_code(), exit::AUTH);
    assert_eq!(
        error.text(),
        "No password saved for testzone. Edit the site to enter it again."
    );

    // The site refuses the password it has.
    let b = small_bed();
    b.mock.fail_next(
        MockOp::List,
        ProviderError::Auth {
            hint: AuthHint::Unknown,
        },
        10,
    );
    let started = Instant::now();
    let error = b.run(&cli()).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(error.exit_code(), exit::AUTH);
    assert_eq!(b.mock.calls(MockOp::Upload), 0);
}

#[test]
fn a_profile_whose_folder_or_site_is_gone_is_a_config_error() {
    let b = small_bed();
    let gone = ProfileRecord {
        local_dir: b.local.path().join("nope"),
        ..b.profile.clone()
    };
    let error = b
        .rt()
        .block_on(run_headless(&b.ctx, &gone, &cli(), &|_| {}))
        .unwrap_err();
    assert_eq!(error.exit_code(), exit::CONFIG);
    let orphan = ProfileRecord {
        site: httporter_core::SiteId::new(),
        ..b.profile.clone()
    };
    let error = b
        .rt()
        .block_on(run_headless(&b.ctx, &orphan, &cli(), &|_| {}))
        .unwrap_err();
    assert_eq!(error.exit_code(), exit::CONFIG);
    assert_eq!(b.mock.calls(MockOp::Upload), 0);
}

#[test]
fn every_headless_error_has_its_exit_code() {
    assert_eq!(HeadlessError::Config(String::new()).exit_code(), 2);
    assert_eq!(HeadlessError::Auth(String::new()).exit_code(), 3);
    assert_eq!(HeadlessError::Io(String::new()).exit_code(), 1);
    assert_eq!(
        (
            exit::OK,
            exit::FAILURES,
            exit::CONFIG,
            exit::AUTH,
            exit::GUARD
        ),
        (0, 1, 2, 3, 4)
    );
}
