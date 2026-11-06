// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests of the transfers controller (Task 23): the whole app wired to a real engine, a mock
//! storage factory, a journal in memory and a headless window.
//!
//! The testing backend has no event loop here (it can have one per process only), so what the
//! runtime's tasks hand to the window with `upgrade_in_event_loop` does not arrive. The tests look
//! at what the controller, the session, the engine and the storage did instead, and play the
//! window's part themselves where a flow needs it: they put the conflict sheet up from the plan
//! the controller holds, and they call `tick` (the engine's feed) on the UI thread.

use std::path::Path;
use std::sync::Mutex as StdMutex;

use httporter_core::{
    AuthHint, Capabilities, ChecksumKind, Conflict, ConflictPolicy, EmptyFolders, JobRecord,
    JobState, MemoryJournal, MockOp, MockProvider,
};
use httporter_platform::{MemoryStore, SETTINGS_TRANSFERS, Store};
use i_slint_backend_testing as testing;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::WindowEvent;
use slint::{ModelRc, VecModel};
use tokio::runtime::Runtime;

use super::*;
use crate::add_site::ValidSite;
use crate::binding::{Engines, install_app};
use crate::drop_hook::{DropBatch, PaneSide};
use crate::session::fakes::{FakeFactory, fixed_now};
use crate::sheets_view::{ACTION_KEEP_BOTH, ACTION_REPLACE, MODE_REPLACE_ALL, MODE_SKIP_ALL};
use crate::{ConflictRow, ProviderTile, SiteItem};

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

fn rp(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

fn valid(zone: &str) -> ValidSite {
    ValidSite {
        name: zone.into(),
        zone: zone.into(),
        region_code: "sg".into(),
        password: "pw".into(),
    }
}

/// Polls `f` until it has an answer: the work runs on the runtime's threads.
fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The app with one or more Bunny sites, a real engine and a window.
struct Bed<J: Journal + 'static = MemoryJournal> {
    ui: AppWindow,
    t: Arc<Transfers>,
    /// The storage of the first site.
    mock: Arc<MockProvider>,
    factory: Arc<FakeFactory>,
    creds: Arc<MemoryStore>,
    journal: Arc<J>,
    sites: Vec<SiteRecord>,
    /// Where the site database lives, to look at what was saved.
    db: PathBuf,
    /// Files on "this PC".
    local: tempfile::TempDir,
    _data: tempfile::TempDir,
    rt: Option<Runtime>,
}

impl<J: Journal + 'static> Drop for Bed<J> {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// What a test may do before the window and the engine exist: the saved sites, the folder of
/// local files, and the journal a crash left.
type BeforeInstall<'a, J = MemoryJournal> = &'a dyn Fn(&[SiteRecord], &Path, &J);

fn bed(mock: MockProvider) -> Bed {
    bed_with(mock, &["testzone"], &|_, _, _| {})
}

fn bed_with(mock: MockProvider, zones: &[&str], before: BeforeInstall<'_>) -> Bed {
    bed_on(mock, zones, before, Arc::new(MemoryJournal::new()))
}

/// As [`bed_with`], over `journal` (a real SQLite one, say).
fn bed_on<J: Journal + 'static>(
    mock: MockProvider,
    zones: &[&str],
    before: BeforeInstall<'_, J>,
    journal: Arc<J>,
) -> Bed<J> {
    thread_local! {
        static PLATFORM_SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !PLATFORM_SET.replace(true) {
        testing::init_no_event_loop();
    }
    let rt = Runtime::new().unwrap();
    let mock = Arc::new(mock);
    let creds = Arc::new(MemoryStore::default());
    let factory = FakeFactory::new(mock.clone());
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join("sites.db");
    let session = Session::new(
        Store::open(&db).unwrap(),
        creds.clone(),
        factory.clone(),
        fixed_now,
    );
    for zone in zones {
        rt.block_on(session.save_site(&valid(zone))).unwrap();
    }
    let sites = session.sites();
    let local = tempfile::tempdir().unwrap();
    before(&sites, local.path(), &journal);
    let engine = Engine::new(
        rt.handle().clone(),
        journal.clone(),
        session.settings(),
        copy::describe_transfer_error,
    );
    let ui = AppWindow::new().unwrap();
    let t = install_app(
        &ui,
        rt.handle().clone(),
        session,
        Engines {
            engine,
            journal: journal.clone(),
        },
    );
    // The first site opens at the start.
    wait_for("the first site to open", || t.session.remote_shows());
    Bed {
        ui,
        t,
        mock,
        factory,
        creds,
        journal,
        sites,
        db,
        local,
        _data: data,
        rt: Some(rt),
    }
}

impl<J: Journal + 'static> Bed<J> {
    fn rt(&self) -> &Runtime {
        self.rt.as_ref().unwrap()
    }

    /// Puts a file on "this PC".
    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.local.path().join(name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn stored(&self, path: &str) -> Option<String> {
        self.mock
            .contents(&rp(path))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Waits until the storage holds `contents` at `path`.
    fn wait_stored(&self, path: &str, contents: &str) {
        wait_for(&format!("{path} to be uploaded"), || {
            (self.stored(path).as_deref() == Some(contents)).then_some(())
        });
    }

    /// Waits until the engine has nothing left to do.
    fn wait_idle(&self) {
        wait_for("the engine to finish", || {
            (!self.t.engine.is_busy()).then_some(())
        });
    }

    /// The conflict sheet's plan, once the controller asks.
    /// The plan the conflict sheet asks about, and where it goes as the sheet says it.
    fn wait_for_conflicts(&self) -> (Direction, Vec<Conflict>, u64, String) {
        wait_for("the conflict sheet", || {
            let state = lock(&self.t.state);
            state.conflict.as_ref().map(|pending| {
                let site = &pending.target.site.name;
                match &pending.planned {
                    Planned::Upload(plan, dir) => (
                        Direction::Upload,
                        plan.conflicts.clone(),
                        plan.files,
                        copy::remote_folder_label(site, dir),
                    ),
                    Planned::Download(plan, dir) => (
                        Direction::Download,
                        plan.conflicts.clone(),
                        plan.files,
                        copy::local_folder_label(dir),
                    ),
                }
            })
        })
    }

    /// Puts the conflict sheet up the way the controller's message to the window does.
    fn show_conflicts(&self) -> sheets_view::ConflictSheetView {
        let (direction, conflicts, files, target) = self.wait_for_conflicts();
        let view = sheets_view::conflict_sheet(
            direction,
            &conflicts,
            files,
            &target,
            "testzone",
            copy::BUNNY,
        );
        sheets_view::apply_conflicts(&self.ui, &view);
        view
    }

    /// Sets the conflict row of `name` to `action`, as its menu does.
    fn choose(&self, name: &str, action: i32) {
        let rows = self.ui.get_conflicts_rows();
        let i = rows
            .iter()
            .position(|row| row.name == name)
            .unwrap_or_else(|| panic!("no conflict row {name}"));
        rows.set_row_data(
            i,
            ConflictRow {
                action,
                ..rows.row_data(i).unwrap()
            },
        );
    }

    fn list_calls(&self) -> usize {
        self.mock.calls(MockOp::List)
    }
}

// ---- Uploads ----

#[test]
fn drop_on_folder_row_uploads_into_that_folder() {
    let b = bed(MockProvider::new(bunny_like())
        .with_file("wp-content/x.txt", b"x")
        .with_file("index.html", b"<html></html>"));
    // Row 0 is the folder (folders first), row 1 the file.
    assert_eq!(b.t.session.remote_folder_at(0), Some(rp("wp-content")));
    assert_eq!(b.t.session.remote_folder_at(1), None);

    let a = b.file("a.txt", "hello");
    b.t.upload_dropped(vec![a], Some(0));
    b.wait_stored("wp-content/a.txt", "hello");
    assert_eq!(b.stored("a.txt"), None);

    // Dropped on a file row, the files go to the folder the pane shows.
    let c = b.file("c.txt", "see");
    b.t.upload_dropped(vec![c], Some(1));
    b.wait_stored("c.txt", "see");

    // The way Explorer's drops come in (`main.rs`): to the cloud pane once its site is open.
    b.ui.set_can_upload(true);
    let d = b.file("d.txt", "dee");
    crate::dropped(
        &b.ui,
        &b.t,
        DropBatch {
            side: PaneSide::Remote,
            row: Some(0),
            paths: vec![d],
        },
    );
    b.wait_stored("wp-content/d.txt", "dee");
    b.wait_idle();

    // While the site is not open yet, the drop only says to wait.
    b.ui.set_can_upload(false);
    b.ui.set_sites(ModelRc::new(VecModel::from(vec![SiteItem {
        name: "testzone".into(),
        provider: ProviderTile::Bunny,
        ..SiteItem::default()
    }])));
    b.ui.set_selected_site(0);
    let e = b.file("e.txt", "e");
    crate::dropped(
        &b.ui,
        &b.t,
        DropBatch {
            side: PaneSide::Remote,
            row: None,
            paths: vec![e.clone()],
        },
    );
    assert_eq!(
        b.ui.get_status_left(),
        "Wait until testzone has opened, then drop the files again."
    );
    // And files dropped on This PC point to the site's pane.
    crate::dropped(
        &b.ui,
        &b.t,
        DropBatch {
            side: PaneSide::Local,
            row: None,
            paths: vec![e],
        },
    );
    assert_eq!(
        b.ui.get_status_left(),
        "Drop files on the testzone pane to upload them."
    );
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(b.stored("e.txt"), None);
}

#[test]
fn upload_with_conflicts_shows_sheet_then_submits_resolved() {
    let b = bed(MockProvider::new(bunny_like())
        .with_file("index.html", b"<html>old</html>")
        .with_file("keep.txt", b"old")
        .with_file("same.txt", b"same"));
    let files = vec![
        b.file("index.html", "<html>new</html>"),
        b.file("keep.txt", "new!"),
        b.file("same.txt", "same"),
        b.file("fresh.txt", "fresh"),
    ];
    b.t.upload(files, None);

    // The policy is Ask (the default) and three files exist: the sheet asks, nothing is sent.
    let view = b.show_conflicts();
    assert_eq!(view.title, "3 files already exist on testzone");
    assert_eq!(view.summary, "2 differ · 1 identical (same SHA-256)");
    assert!(
        view.body
            .starts_with("You're uploading 4 files to testzone."),
        "{}",
        view.body
    );
    assert_eq!(b.mock.calls(MockOp::Upload), 0);
    let rows: Vec<(String, i32)> =
        b.ui.get_conflicts_rows()
            .iter()
            .map(|row| (row.name.to_string(), row.action))
            .collect();
    assert!(rows.contains(&("same.txt".to_owned(), 1)), "{rows:?}");

    // Each row's own choice wins over the default action.
    b.choose("index.html", ACTION_REPLACE);
    b.choose("keep.txt", ACTION_KEEP_BOTH);
    b.ui.invoke_conflicts_continue(MODE_SKIP_ALL, false);
    assert!(lock(&b.t.state).conflict.is_none());
    b.wait_stored("index.html", "<html>new</html>");
    b.wait_stored("keep (1).txt", "new!");
    b.wait_stored("fresh.txt", "fresh");
    b.wait_idle();
    assert_eq!(b.stored("keep.txt").as_deref(), Some("old"));
    assert_eq!(b.stored("same.txt").as_deref(), Some("same"));
    // The identical file was left alone: three uploads in all.
    assert_eq!(b.mock.calls(MockOp::Upload), 3);

    // Cancel Upload sends nothing.
    let again = b.file("index.html", "<html>newer</html>");
    b.t.upload(vec![again], None);
    b.wait_for_conflicts();
    b.ui.invoke_conflicts_cancel();
    assert!(lock(&b.t.state).conflict.is_none());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(b.mock.calls(MockOp::Upload), 3);
}

#[test]
fn ask_policy_remembered_per_site() {
    let b = bed_with(
        MockProvider::new(bunny_like()).with_file("index.html", b"old"),
        &["testzone", "zone-b"],
        &|_, _, _| {},
    );
    let (a, other) = (b.sites[0].id.clone(), b.sites[1].id.clone());
    let policy =
        |site: &SiteId, upload: bool| b.rt().block_on(b.t.session.conflict_policy(site, upload));
    assert_eq!(policy(&a, true), ConflictPolicy::Ask);

    let file = b.file("index.html", "new");
    b.t.upload(vec![file.clone()], None);
    b.show_conflicts();
    // "Replace all", remembered for uploads to this site.
    b.ui.invoke_conflicts_continue(MODE_REPLACE_ALL, true);
    b.wait_stored("index.html", "new");
    wait_for("the choice to be remembered", || {
        (policy(&a, true) == ConflictPolicy::Replace).then_some(())
    });
    // Only for uploads, and only to this site.
    assert_eq!(policy(&a, false), ConflictPolicy::Ask);
    assert_eq!(policy(&other, true), ConflictPolicy::Ask);
    b.wait_idle();

    // The next upload with a conflict goes ahead without asking.
    std::fs::write(&file, "newer").unwrap();
    b.t.upload(vec![file], None);
    b.wait_stored("index.html", "newer");
    assert!(lock(&b.t.state).conflict.is_none());
}

// ---- Deletes and folders ----

#[test]
fn delete_shows_counts_then_deletes() {
    let b = bed(MockProvider::new(bunny_like())
        .with_file("old-promo-2025/a.jpg", &[0; 1500])
        .with_file("old-promo-2025/b/c.jpg", &[0; 500])
        .with_file("index.html", b"<html></html>")
        .with_file("keep.txt", b"k"));
    let entries =
        b.t.session
            .remote_entries(&["old-promo-2025".into(), "index.html".into()]);
    assert_eq!(entries.len(), 2);

    // What the alert says for these: the counts come from the plan the controller makes.
    let plan = b.rt().block_on(plan_delete(&*b.mock, &entries)).unwrap();
    assert_eq!(
        sheets_view::delete_alert(&plan.summary, "testzone", ProviderKind::Bunny),
        (
            "Delete 1 folder and 1 file from testzone?".to_owned(),
            "That's 3 files (2 KB) in all. Bunny Storage has no trash, so this can't be undone."
                .to_owned(),
            "Delete 3 Files".to_owned()
        )
    );

    // Cancel: nothing goes.
    b.t.delete(entries.clone());
    wait_for("the delete alert", || {
        lock(&b.t.state).delete.as_ref().map(|_| ())
    });
    b.ui.invoke_delete_cancel();
    assert!(lock(&b.t.state).delete.is_none());
    std::thread::sleep(Duration::from_millis(100));
    assert!(b.stored("index.html").is_some());
    assert_eq!(b.mock.calls(MockOp::DeleteFile), 0);
    assert_eq!(b.mock.calls(MockOp::DeleteFolder), 0);

    // Delete: each file alone, the folder's own job last (ruling Q54), the rest stays.
    b.t.delete(entries);
    let jobs = wait_for("the delete alert", || {
        lock(&b.t.state).delete.as_ref().map(|d| d.jobs.clone())
    });
    assert_eq!(jobs.len(), 4, "{jobs:?}");
    assert!(
        matches!(jobs.last(), Some(Job::Delete { folder: true, .. })),
        "{jobs:?}"
    );
    b.ui.invoke_delete_confirm();
    wait_for("the files to go", || {
        (b.stored("index.html").is_none() && b.stored("old-promo-2025/a.jpg").is_none())
            .then_some(())
    });
    b.wait_idle();
    assert_eq!(b.stored("old-promo-2025/b/c.jpg"), None);
    assert_eq!(b.stored("keep.txt").as_deref(), Some("k"));
}

/// The name of each row of the cloud pane, and what its date column says.
fn remote_rows(t: &Transfers) -> Vec<(String, String)> {
    t.session
        .remote_view()
        .rows
        .iter()
        .map(|row| (row.name.clone(), row.modified.clone()))
        .collect()
}

#[test]
fn new_folder_on_bunny_is_virtual_until_refresh() {
    let b = bed(MockProvider::new(bunny_like()).with_file("assets/x.css", b"x"));
    b.t.new_folder(&b.ui, true);
    assert!(b.ui.get_new_folder_open());
    assert_eq!(b.ui.get_new_folder_name(), "untitled folder");

    // A name the pane already has is refused under the field; the sheet stays.
    b.ui.set_remote_rows(crate::viewmodel::to_slint_rows(
        &b.t.session.remote_view().rows,
    ));
    b.ui.invoke_new_folder_create("assets".into());
    assert_eq!(b.ui.get_new_folder_error(), "“assets” already exists here.");
    assert!(b.ui.get_new_folder_open());

    // Bunny keeps no empty folder: the folder shows, with a note, though the storage refused it.
    b.ui.invoke_new_folder_create(" drafts ".into());
    assert!(!b.ui.get_new_folder_open());
    let note = "Appears on Bunny Storage when you add its first file.";
    wait_for("the virtual folder", || {
        remote_rows(&b.t)
            .contains(&("drafts".to_owned(), note.to_owned()))
            .then_some(())
    });
    assert_eq!(b.mock.calls(MockOp::CreateFolder), 1);
    // A refresh that does not list it keeps it.
    let view = b.rt().block_on(
        b.t.session
            .remote_refresh(b.t.session.remote_ticket(), |_| {}),
    );
    assert!(
        view.rows
            .iter()
            .any(|row| row.name == "drafts" && row.modified == note)
    );

    // Uploading into it makes it real: the next listing has it, and the note goes.
    let first = b.file("first.txt", "1");
    b.t.upload(vec![first], Some(rp("drafts")));
    b.wait_stored("drafts/first.txt", "1");
    b.wait_idle();
    let view = b.rt().block_on(
        b.t.session
            .remote_refresh(b.t.session.remote_ticket(), |_| {}),
    );
    let drafts = view.rows.iter().find(|row| row.name == "drafts").unwrap();
    assert_ne!(drafts.modified, note);
    assert_eq!(
        view.rows.iter().filter(|row| row.name == "drafts").count(),
        1
    );
}

#[test]
fn new_folder_on_storage_that_keeps_folders_is_made_there() {
    let caps = Capabilities {
        empty_folders: EmptyFolders::Supported,
        ..bunny_like()
    };
    let b = bed(MockProvider::new(caps));
    let before = b.list_calls();
    b.t.new_folder(&b.ui, true);
    b.ui.invoke_new_folder_create("made".into());
    // Made, and the pane is listed again to show it.
    wait_for("the pane to be listed again", || {
        (b.list_calls() > before).then_some(())
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = remote_rows(&b.t);
        if rows.iter().any(|(name, _)| name == "made") {
            let note = "Appears on Bunny Storage when you add its first file.";
            assert!(!rows.iter().any(|(_, said)| said == note), "{rows:?}");
            break;
        }
        assert!(Instant::now() < deadline, "no folder in the pane: {rows:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(b.mock.calls(MockOp::CreateFolder), 1);
}

// ---- One client per site ----

#[test]
fn browse_and_transfers_share_one_client() {
    let b = bed_with(
        MockProvider::new(bunny_like()).with_file("index.html", b"<html></html>"),
        &["testzone", "zone-b"],
        &|_, _, _| {},
    );
    let zones = || -> Vec<String> { b.factory.configs().into_iter().map(|c| c.0).collect() };
    assert_eq!(zones(), ["testzone"], "opened");

    // Upload, download and delete all use the pane's client.
    let up = b.file("up.txt", "up");
    b.t.upload(vec![up], None);
    b.wait_stored("up.txt", "up");
    b.wait_idle();
    let into = b.local.path().join("downloads");
    std::fs::create_dir(&into).unwrap();
    let entries = b.t.session.remote_entries(&["index.html".into()]);
    b.t.download(entries, Some(into.clone()));
    wait_for("the download", || {
        std::fs::read_to_string(into.join("index.html")).ok()
    });
    b.wait_idle();
    assert_eq!(
        std::fs::read_to_string(into.join("index.html")).unwrap(),
        "<html></html>"
    );
    b.t.delete(b.t.session.remote_entries(&["index.html".into()]));
    wait_for("the delete alert", || {
        lock(&b.t.state).delete.as_ref().map(|_| ())
    });
    b.ui.invoke_delete_confirm();
    wait_for("the delete", || {
        b.stored("index.html").is_none().then_some(())
    });
    assert_eq!(zones(), ["testzone"], "still the one client");

    // The other site gets its own; going back reuses the first.
    b.ui.set_selected_site(1);
    b.ui.invoke_site_selected(1);
    wait_for("the second site", || {
        b.t.session
            .remote_shows()
            .filter(|(site, _)| *site == b.sites[1].id)
    });
    assert_eq!(zones(), ["testzone", "zone-b"]);
    b.ui.set_selected_site(0);
    b.ui.invoke_site_selected(0);
    wait_for("the first site again", || {
        b.t.session
            .remote_shows()
            .filter(|(site, _)| *site == b.sites[0].id)
    });
    assert_eq!(zones(), ["testzone", "zone-b"]);
}

#[test]
fn settings_change_rebuilds_clients_lazily() {
    let b = bed(MockProvider::new(bunny_like()));
    let site = b.sites[0].id.clone();
    let opened = b.t.session.remote_place().unwrap().provider;
    assert_eq!(b.factory.configs().len(), 1);

    let changed = TransferSettings {
        simultaneous: 8,
        ..TransferSettings::default()
    };
    b.t.apply_settings(changed.clone());
    // In use at once by the hub, but no client is built until one is needed.
    assert_eq!(b.t.session.settings(), changed);
    assert!(!b.t.session.hub().is_current(&site, &opened));
    assert_eq!(b.factory.configs().len(), 1);

    // The next transfer builds the site's client with the new settings.
    let file = b.file("a.txt", "a");
    b.t.upload(vec![file], None);
    b.wait_stored("a.txt", "a");
    let configs = b.factory.full_configs();
    assert_eq!(configs.len(), 2);
    assert_eq!(configs[1].transfer_concurrency, 8);
    assert_eq!(b.t.engine.site_transfer_limit(&site), Some(8));

    // And they are saved.
    let saved = || {
        Store::open(&b.db)
            .unwrap()
            .settings::<TransferSettings>(SETTINGS_TRANSFERS)
    };
    wait_for("the settings to be saved", || {
        (saved() == changed).then_some(())
    });
    // The same settings again change nothing, not even what is stored.
    Store::open(&b.db)
        .unwrap()
        .put_settings(SETTINGS_TRANSFERS, &TransferSettings::default())
        .unwrap();
    b.t.apply_settings(changed);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(saved(), TransferSettings::default());
    assert_eq!(b.factory.configs().len(), 2);
}

// ---- Starting and closing ----

#[test]
fn resume_banner_resumes_and_discard_clears() {
    // A crash left two uploads in the journal.
    let crashed: BeforeInstall<'_> = &|sites, local, journal| {
        let mut records: Vec<JobRecord> = ["r1.txt", "r2.txt"]
            .iter()
            .map(|name| {
                let path = local.join(name);
                std::fs::write(&path, *name).unwrap();
                JobRecord {
                    id: 0,
                    batch: "b-1".into(),
                    site: sites[0].id.clone(),
                    job: Job::Upload {
                        local: path,
                        remote: rp(name),
                        size: name.len() as u64,
                    },
                    state: JobState::Queued,
                    error: None,
                    multipart: None,
                }
            })
            .collect();
        journal.insert(&mut records).unwrap();
    };

    let b = bed_with(MockProvider::new(bunny_like()), &["testzone"], crashed);
    assert_eq!(
        b.ui.get_resume_banner_text(),
        "2 transfers didn't finish last time."
    );
    b.ui.invoke_resume_banner_resume();
    assert_eq!(b.ui.get_resume_banner_text(), "");
    b.wait_stored("r1.txt", "r1.txt");
    b.wait_stored("r2.txt", "r2.txt");
    b.wait_idle();
    wait_for("the journal to be done", || {
        b.journal.unfinished().unwrap().is_empty().then_some(())
    });

    let b = bed_with(MockProvider::new(bunny_like()), &["testzone"], crashed);
    assert_eq!(
        b.ui.get_resume_banner_text(),
        "2 transfers didn't finish last time."
    );
    b.ui.invoke_resume_banner_discard();
    assert_eq!(b.ui.get_resume_banner_text(), "");
    wait_for("the journal to be cleared", || {
        b.journal.unfinished().unwrap().is_empty().then_some(())
    });
    assert_eq!(b.t.engine.unfinished_count(), 0);
    assert_eq!(b.mock.calls(MockOp::Upload), 0);

    // With nothing left over there is no banner.
    let b = bed(MockProvider::new(bunny_like()));
    assert_eq!(b.ui.get_resume_banner_text(), "");
}

/// A bed whose upload hangs, so the engine stays busy.
fn busy_bed() -> Bed {
    let mock = MockProvider::new(bunny_like());
    mock.set_latency(MockOp::Upload, Duration::from_secs(60));
    let b = bed(mock);
    let file = b.file("big.bin", "slow");
    b.t.upload(vec![file], None);
    wait_for("the upload to start", || {
        (b.t.engine.snapshot().active_count == 1).then_some(())
    });
    b
}

/// Ruling Q23 through the window: Remove… on the site's menu, then Remove on the alert, cancels
/// the site's transfers and takes its records out of the journal before the site goes.
#[test]
fn removing_a_site_cancels_its_transfers() {
    let b = busy_bed();
    let site = b.sites[0].id.clone();
    b.ui.invoke_site_remove(0);
    assert!(b.ui.get_remove_site_open());
    assert_eq!(b.ui.get_remove_site_title(), "Remove “testzone”?");
    b.ui.set_remove_site_open(false);
    b.ui.invoke_remove_site_confirm();
    wait_for("the site to go", || {
        b.t.session.sites().is_empty().then_some(())
    });
    wait_for("its transfers to stop", || {
        (!b.t.engine.is_busy()).then_some(())
    });
    // They are cancelled through the journal writer, then cleared (review N2).
    wait_for("its journal records to go", || {
        b.journal
            .records()
            .iter()
            .all(|record| record.site != site)
            .then_some(())
    });
    assert!(b.journal.unfinished().unwrap().is_empty());
    assert_eq!(b.t.engine.unfinished_count(), 0);
    assert_eq!(b.stored("big.bin"), None, "nothing reached the storage");
    let secret = httporter_platform::CredentialStore::get(
        &*b.creds,
        &httporter_platform::CredentialKey::site(&site),
    );
    assert_eq!(secret.unwrap(), None);
}

fn request_close(ui: &AppWindow) {
    ui.window().dispatch_event(WindowEvent::CloseRequested);
}

#[test]
fn quit_while_busy_asks() {
    // Nothing running: the window just closes.
    let b = bed(MockProvider::new(bunny_like()));
    request_close(&b.ui);
    assert!(!b.ui.get_quit_open());

    // Busy: the quit alert asks, and Keep Running stays.
    let b = busy_bed();
    request_close(&b.ui);
    assert!(b.ui.get_quit_open());
    assert_eq!(b.ui.get_quit_title(), "Transfers are still running");
    assert_eq!(b.ui.get_quit_body(), "1 file hasn't finished.");
    b.ui.set_quit_open(false);
    b.ui.invoke_quit_keep_running();
    assert!(b.t.engine.is_busy());

    // Pause and Quit: the journal keeps the upload for next time.
    b.ui.invoke_quit_pause();
    assert_eq!(lock(&b.t.state).quitting, Some(Quit::Pause));
    // Closing again does not ask twice.
    b.ui.set_quit_open(false);
    request_close(&b.ui);
    assert!(!b.ui.get_quit_open());
    b.t.finish();
    assert_eq!(b.t.engine.snapshot().active_count, 0);
    assert_eq!(b.journal.unfinished().unwrap().len(), 1);

    // Cancel and Quit: nothing is left.
    let b = busy_bed();
    request_close(&b.ui);
    assert!(b.ui.get_quit_open());
    b.ui.invoke_quit_cancel_all();
    assert_eq!(lock(&b.t.state).quitting, Some(Quit::Cancel));
    b.t.finish();
    assert!(!b.t.engine.is_busy());
    assert!(b.journal.unfinished().unwrap().is_empty());
    assert_eq!(b.stored("big.bin"), None);
}

#[test]
fn auth_stop_banner_opens_edit() {
    let mock = MockProvider::new(bunny_like());
    // A refusal that names the secret: one with no hint is first checked against a listing of
    // the file's folder (review I5), which this mock would answer.
    mock.fail_next(
        MockOp::Upload,
        ProviderError::Auth {
            hint: AuthHint::Secret,
        },
        1,
    );
    let b = bed(mock);
    let site = b.sites[0].clone();
    let file = b.file("a.txt", "a");
    b.t.upload(vec![file], None);
    wait_for("the sign-in to fail", || {
        matches!(b.t.engine.snapshot().state, EngineState::AuthStopped { .. }).then_some(())
    });

    // The feed shows the banner and the drawer says why.
    b.t.tick(&b.ui);
    assert_eq!(
        b.ui.get_auth_banner_text(),
        "Couldn't sign in to testzone. Its transfers are paused."
    );
    assert_eq!(b.ui.get_transfers_title(), "Sign-in failed · testzone");
    assert_eq!(b.ui.get_offline_banner_text(), "");
    // Edit Site… is on (every Add Site control is wired since Task 24), for the stopped site.
    let edit = ElementHandle::find_by_accessible_label(&b.ui, "Edit Site…")
        .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .expect("the banner's Edit Site…");
    assert_eq!(edit.accessible_enabled(), Some(true));
    assert_eq!(b.t.auth_stopped_site(), Some(site.id.clone()));
    edit.invoke_accessible_default_action();

    // Saving the edited site lets its transfers go on with the new client.
    b.rt().block_on(b.t.site_edited(&site));
    b.wait_stored("a.txt", "a");
    b.wait_idle();
    b.t.tick(&b.ui);
    assert_eq!(b.ui.get_auth_banner_text(), "");
    assert_eq!(b.t.auth_stopped_site(), None);
}

#[test]
fn finished_batch_refreshes_visible_folder() {
    let b = bed(MockProvider::new(bunny_like()).with_file("wp-content/x.txt", b"x"));
    // A batch into a folder the pane does not show refreshes nothing.
    let deep = b.file("deep.txt", "d");
    b.t.upload(vec![deep], Some(rp("a/b/c")));
    b.wait_stored("a/b/c/deep.txt", "d");
    b.wait_idle();
    let before = b.list_calls();
    b.t.tick(&b.ui);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(b.list_calls(), before);

    // One into the folder it shows lists it again, with the new file.
    let here = b.file("here.txt", "h");
    b.t.upload(vec![here], None);
    b.wait_stored("here.txt", "h");
    b.wait_idle();
    let before = b.list_calls();
    b.t.tick(&b.ui);
    wait_for("the pane to be listed again", || {
        (b.list_calls() > before).then_some(())
    });
    wait_for("the new file in the pane", || {
        remote_rows(&b.t)
            .iter()
            .any(|(name, _)| name == "here.txt")
            .then_some(())
    });
    // The drawer says what happened last.
    assert_eq!(
        b.ui.get_transfers_collapsed_text(),
        "No transfers running · Last transfer just now · 1 file · 1 B"
    );
    // Once listed, the next tick does not list it again.
    let after = b.list_calls();
    b.t.tick(&b.ui);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(b.list_calls(), after);
}

#[test]
fn a_download_into_a_folder_with_the_file_asks_first() {
    let b = bed(MockProvider::new(bunny_like()).with_file("index.html", b"remote"));
    let into = b.local.path().join("site");
    std::fs::create_dir(&into).unwrap();
    std::fs::write(into.join("index.html"), "local").unwrap();
    b.t.download(
        b.t.session.remote_entries(&["index.html".into()]),
        Some(into.clone()),
    );
    let view = b.show_conflicts();
    assert_eq!(view.title, "1 file already exists in site");
    assert_eq!(view.col_source, "On Bunny");
    assert_eq!(view.cancel_label, "Cancel Download");
    b.ui.invoke_conflicts_continue(MODE_REPLACE_ALL, false);
    wait_for("the download", || {
        (std::fs::read_to_string(into.join("index.html")).ok()? == "remote").then_some(())
    });
    b.wait_idle();
    assert_eq!(b.journal.unfinished().unwrap().len(), 0);
}

// ---- The drawer's feed ----

#[test]
fn the_feed_puts_the_engines_state_in_the_drawer() {
    let b = busy_bed();
    b.t.tick(&b.ui);
    assert!(b.ui.get_transfers_running());
    assert_eq!(b.ui.get_transfers_title(), "Uploading to testzone");
    assert_eq!(
        b.ui.get_transfers_line(),
        "0 of 1 files · 0 of 4 B · over HTTPS to sg.storage.bunnycdn.com"
    );
    let titles: Vec<String> =
        b.ui.get_transfers_tab_titles()
            .iter()
            .map(|t| t.to_string())
            .collect();
    assert_eq!(titles, ["Active 1", "Queued 0", "Failed 0", "Done 0"]);
    // The drawer's buttons reach the engine.
    b.ui.invoke_transfers_pause();
    wait_for("the pause", || {
        (b.t.engine.snapshot().state == EngineState::Paused).then_some(())
    });
    b.t.tick(&b.ui);
    assert!(b.ui.get_transfers_paused());
    assert_eq!(b.ui.get_transfers_title(), "Paused · Uploading to testzone");
    b.ui.invoke_transfers_cancel_all();
    b.wait_idle();
    b.t.tick(&b.ui);
    assert!(!b.ui.get_transfers_running());
}

#[tokio::test(flavor = "multi_thread")]
async fn drawer_updates_at_most_10_hz() {
    let (tx, rx) = watch::channel(0u64);
    let seen = rx.clone();
    let draws: Arc<StdMutex<Vec<(Instant, u64)>>> = Arc::default();
    let feed = tokio::spawn(paced(rx, {
        let draws = draws.clone();
        move || {
            draws.lock().unwrap().push((Instant::now(), *seen.borrow()));
            true
        }
    }));
    // A change every millisecond for a second.
    let start = Instant::now();
    let mut sent = 0;
    while start.elapsed() < Duration::from_secs(1) {
        sent += 1;
        tx.send_replace(sent);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // The last change is drawn too, once the wait is over.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let drawn = draws.lock().unwrap().clone();
    assert!((5..=12).contains(&drawn.len()), "{} draws", drawn.len());
    for pair in drawn.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        assert!(
            gap >= Duration::from_millis(95),
            "{gap:?} between two draws"
        );
    }
    assert_eq!(drawn.last().unwrap().1, sent, "the last change is drawn");

    // The feed ends with the engine …
    drop(tx);
    tokio::time::timeout(Duration::from_secs(1), feed)
        .await
        .expect("the feed ends")
        .unwrap();
    // … or when the window is gone.
    let (tx, rx) = watch::channel(0u64);
    let feed = tokio::spawn(paced(rx, || false));
    tx.send_replace(1);
    tokio::time::timeout(Duration::from_secs(1), feed)
        .await
        .expect("the feed ends")
        .unwrap();
}

#[test]
fn the_taskbar_follows_the_engines_state() {
    let s = |state: EngineState, bytes: (u64, u64), files: (u64, u64)| EngineSnapshot {
        state,
        bytes_done: bytes.0,
        bytes_total: bytes.1,
        files_done: files.0,
        files_total: files.1,
        ..EngineSnapshot::default()
    };
    assert_eq!(
        taskbar_state(&s(EngineState::Idle, (0, 0), (0, 0))),
        TaskbarState::None
    );
    // By bytes when there are any …
    assert_eq!(
        taskbar_state(&s(EngineState::Running, (5, 10), (1, 4))),
        TaskbarState::Normal { done: 5, total: 10 }
    );
    // … by files for deletes.
    assert_eq!(
        taskbar_state(&s(EngineState::Running, (0, 0), (1, 4))),
        TaskbarState::Normal { done: 1, total: 4 }
    );
    let offline = EngineState::Offline {
        site: "testzone".into(),
    };
    assert_eq!(
        taskbar_state(&s(offline, (5, 10), (1, 4))),
        TaskbarState::Paused { done: 5, total: 10 }
    );
    assert_eq!(
        taskbar_state(&s(EngineState::Paused, (5, 10), (1, 4))),
        TaskbarState::Paused { done: 5, total: 10 }
    );
    let auth = EngineState::AuthStopped {
        site: "testzone".into(),
    };
    assert_eq!(
        taskbar_state(&s(auth, (5, 10), (1, 4))),
        TaskbarState::Error { done: 5, total: 10 }
    );
}

#[test]
fn skipped_items_are_said_with_the_first_and_why() {
    assert_eq!(skipped_note(&[]), None);
    assert_eq!(
        skipped_note(&[(
            PathBuf::from(r"C:\site\locked.db"),
            "Access is denied.".into()
        )])
        .as_deref(),
        Some("1 item was skipped: locked.db: Access is denied.")
    );
    assert_eq!(
        skipped_note(&[
            (PathBuf::from(r"C:\site\a"), "gone".into()),
            (PathBuf::from(r"C:\site\b"), "gone".into()),
        ])
        .as_deref(),
        Some("2 items were skipped. a: gone")
    );
}

#[test]
fn the_engines_words_become_sentences_and_notifications_name_the_run() {
    use httporter_transfer::messages;
    for (said, sentence) in [
        (
            messages::STOPPED_UNEXPECTEDLY,
            "The transfer stopped unexpectedly.",
        ),
        (
            messages::HASHING_STOPPED,
            "Checking the file stopped before it finished.",
        ),
        (
            messages::FILE_WORK_STOPPED,
            "Work on the file on this PC stopped before it finished.",
        ),
        (
            messages::NO_ANSWER_FOR_FILE,
            "The server didn't answer for this file.",
        ),
        (
            messages::DAMAGED_TWICE,
            "The file arrived damaged twice (checksum mismatch).",
        ),
        (
            messages::LOST_AFTER_UPLOAD,
            "The site accepted the file but didn't keep it, three times. Retry to send it again.",
        ),
        ("No connection", "No connection"),
    ] {
        assert_eq!(copy::transfer_message(said), sentence);
    }
    // Ruling Q55: the note once cut-off uploads were checked and some sent again.
    assert_eq!(
        copy::rechecked(3, 2),
        "Checked 3 files that were cut off mid-upload; re-sent 2."
    );
    assert_eq!(
        copy::rechecked(1, 1),
        "Checked 1 file that was cut off mid-upload; re-sent it."
    );
    assert_eq!(
        copy::rechecked(1_204, 1),
        "Checked 1,204 files that were cut off mid-upload; re-sent 1."
    );
    let note = |direction, done, failed| {
        copy::finished_notification(direction, done, failed, "example-shop.com")
    };
    assert_eq!(
        note(Direction::Upload, 1_204, 0),
        (
            "Upload finished".to_owned(),
            "Uploaded 1,204 files to example-shop.com.".to_owned()
        )
    );
    assert_eq!(
        note(Direction::Download, 1, 0),
        (
            "Download finished".to_owned(),
            "Downloaded 1 file from example-shop.com.".to_owned()
        )
    );
    assert_eq!(
        note(Direction::Delete, 3, 0).1,
        "Deleted 3 files from example-shop.com."
    );
    assert_eq!(
        note(Direction::Mixed, 3, 0),
        (
            "Transfer finished".to_owned(),
            "Finished 3 files with example-shop.com.".to_owned()
        )
    );
    assert_eq!(
        note(Direction::Upload, 10, 2),
        (
            "Upload finished with problems".to_owned(),
            "Uploaded 10 files to example-shop.com. 2 files failed. Open HTTPorter to retry.".to_owned()
        )
    );
}

#[test]
fn a_site_whose_secret_is_gone_says_so_in_the_f5_words() {
    let b = bed(MockProvider::new(bunny_like()));
    let site = b.sites[0].clone();
    httporter_platform::CredentialStore::delete(
        &*b.creds,
        &httporter_platform::CredentialKey::site(&site.id),
    )
    .unwrap();
    b.t.session.hub().invalidate(&site.id);
    let Err(error) = b.rt().block_on(b.t.session.connect(&site)) else {
        panic!("a site with no password has no client");
    };
    assert_eq!(
        error,
        "No password saved for testzone. Edit the site to enter it again."
    );
    assert_eq!(
        hub_error_text(&crate::site_hub::HubError::MissingSecret, &site),
        error
    );
    assert_eq!(
        hub_error_text(
            &crate::site_hub::HubError::Credential("locked".into()),
            &site
        ),
        "Couldn't read the password for testzone from Windows Credential Manager: locked"
    );
}

// ---- The final fix wave (review C1, I3, I6, I7, M2) ----

/// `n` file deletes for the bed's first site, through `provider`.
fn deletes<J: Journal + 'static>(
    b: &Bed<J>,
    provider: Arc<dyn StorageProvider>,
    n: usize,
) -> Batch {
    let site = &b.sites[0];
    Batch {
        site: site.id.clone(),
        site_name: site.name.clone(),
        host: site.host.clone(),
        provider,
        jobs: (0..n)
            .map(|i| Job::Delete {
                remote: rp(&format!("f{i}.txt")),
                folder: false,
            })
            .collect(),
        checksums: HashMap::new(),
    }
}

/// Review C1: Cancel and Quit with a big queue on the real journal (SQLite in a file, a disk
/// sync per commit) leaves nothing for the next start to offer again.
#[test]
fn cancel_and_quit_leaves_nothing_in_a_real_journal() {
    let data = tempfile::tempdir().unwrap();
    let journal =
        Arc::new(httporter_platform::SqliteJournal::open(&data.path().join("journal.db")).unwrap());
    let b = bed_on(
        MockProvider::new(bunny_like()),
        &["testzone"],
        &|_, _, _| {},
        journal.clone(),
    );
    // 10,000 deletes (an S3 folder of that many files), held in the queue.
    b.t.engine.pause_all();
    let storage: Arc<dyn StorageProvider> = b.mock.clone();
    b.t.engine.submit(deletes(&b, storage, 10_000)).unwrap();
    assert_eq!(b.t.engine.snapshot().queued_count, 10_000);
    assert_eq!(journal.unfinished().unwrap().len(), 10_000);

    b.t.quit(Quit::Cancel);
    b.t.finish();
    assert!(!b.t.engine.is_busy());
    assert_eq!(journal.unfinished().unwrap().len(), 0, "nothing to resume");
    assert_eq!(
        b.mock.calls(MockOp::DeleteBatch) + b.mock.calls(MockOp::DeleteFile),
        0
    );
}

/// Review N2: removing a site that a crash left 10,000 transfers for, on the real journal,
/// returns at once, so the site leaves the sidebar at once: its records are cancelled through
/// the engine's writer, in chunks of one transaction each (which closing waits for), not with a
/// disk sync each (about 9 s for 13,793). They all reach the journal; another site's stay.
#[test]
fn forgetting_a_site_with_a_big_journal_returns_at_once() {
    let data = tempfile::tempdir().unwrap();
    let journal =
        Arc::new(httporter_platform::SqliteJournal::open(&data.path().join("journal.db")).unwrap());
    let crashed: BeforeInstall<'_, httporter_platform::SqliteJournal> = &|sites, _, journal| {
        let record = |site: &SiteRecord, name: String| JobRecord {
            id: 0,
            batch: "left-by-a-crash".into(),
            site: site.id.clone(),
            job: Job::Delete {
                remote: rp(&name),
                folder: false,
            },
            state: JobState::Queued,
            error: None,
            multipart: None,
        };
        let mut records: Vec<JobRecord> = (0..10_000)
            .map(|i| record(&sites[0], format!("f{i}.txt")))
            .collect();
        records.push(record(&sites[1], "kept.txt".into()));
        journal.insert(&mut records).unwrap();
    };
    let b = bed_on(
        MockProvider::new(bunny_like()),
        &["testzone", "zone-b"],
        crashed,
        journal.clone(),
    );
    assert_eq!(b.t.engine.unfinished_count(), 10_001);

    let started = Instant::now();
    b.t.forget_site(&b.sites[0].id);
    let took = started.elapsed();
    assert!(took < Duration::from_secs(1), "took {took:?}");

    wait_for("the cancels to reach the journal", || {
        (b.t.engine.pending_writes() == 0).then_some(())
    });
    let left = journal.unfinished().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].site, b.sites[1].id);
    assert_eq!(b.t.engine.unfinished_count(), 1);
    assert_eq!(
        b.mock.calls(MockOp::DeleteBatch) + b.mock.calls(MockOp::DeleteFile),
        0
    );
}

/// A journal in memory whose state changes can be held back, as a slow disk would.
#[derive(Default)]
struct SlowJournal {
    inner: MemoryJournal,
    held: StdMutex<bool>,
    turned: std::sync::Condvar,
}

impl SlowJournal {
    fn hold(&self, held: bool) {
        *self.held.lock().unwrap() = held;
        self.turned.notify_all();
    }

    fn wait_while_held(&self) {
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.turned.wait(held).unwrap();
        }
    }
}

impl Journal for SlowJournal {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), httporter_core::JournalError> {
        self.inner.insert(records)
    }
    fn set_state(
        &self,
        id: i64,
        state: JobState,
        error: Option<&str>,
    ) -> Result<(), httporter_core::JournalError> {
        self.wait_while_held();
        self.inner.set_state(id, state, error)
    }
    fn set_multipart(
        &self,
        id: i64,
        mp: Option<&httporter_core::MultipartProgress>,
    ) -> Result<(), httporter_core::JournalError> {
        self.wait_while_held();
        self.inner.set_multipart(id, mp)
    }
    fn unfinished(&self) -> Result<Vec<JobRecord>, httporter_core::JournalError> {
        self.inner.unfinished()
    }
    fn clear_finished(&self) -> Result<(), httporter_core::JournalError> {
        self.inner.clear_finished()
    }
    fn discard_unfinished(&self) -> Result<(), httporter_core::JournalError> {
        self.inner.discard_unfinished()
    }
}

/// Review C1: after Cancel All in the drawer, closing the window doesn't ask about "0 files":
/// the quit alert counts transfers, not journal writes still on their way. Those are waited
/// for, so nothing comes back at the next start.
#[test]
fn the_quit_alert_counts_transfers_not_journal_writes() {
    let journal = Arc::new(SlowJournal::default());
    let b = bed_on(
        MockProvider::new(bunny_like()),
        &["testzone"],
        &|_, _, _| {},
        journal.clone(),
    );
    b.t.engine.pause_all();
    let storage: Arc<dyn StorageProvider> = b.mock.clone();
    b.t.engine.submit(deletes(&b, storage, 3)).unwrap();
    journal.hold(true);
    b.ui.invoke_transfers_cancel_all();
    let s = b.t.engine.snapshot();
    assert_eq!((s.queued_count, s.active_count), (0, 0));
    assert!(b.t.engine.is_busy(), "the cancels are not written yet");

    request_close(&b.ui);
    assert!(!b.ui.get_quit_open(), "no transfer is left to ask about");
    // The window waits for the writes instead, then closes.
    assert_eq!(lock(&b.t.state).quitting, Some(Quit::Pause));
    journal.hold(false);
    b.t.finish();
    assert!(journal.inner.unfinished().unwrap().is_empty());
}

/// Review C1: the resume banner says when deletes are among what didn't finish, since Resume
/// carries them out.
#[test]
fn the_resume_banner_names_the_deletes_among_what_is_left() {
    let crashed: BeforeInstall<'_> = &|sites, local, journal| {
        let site = sites[0].id.clone();
        let mut records: Vec<JobRecord> = Vec::new();
        for name in ["r1.txt", "r2.txt"] {
            let path = local.join(name);
            std::fs::write(&path, name).unwrap();
            records.push(JobRecord {
                id: 0,
                batch: "b-1".into(),
                site: site.clone(),
                job: Job::Upload {
                    local: path,
                    remote: rp(name),
                    size: name.len() as u64,
                },
                state: JobState::Queued,
                error: None,
                multipart: None,
            });
        }
        for name in ["old/a.txt", "old/b.txt", "old"] {
            records.push(JobRecord {
                id: 0,
                batch: "b-2".into(),
                site: site.clone(),
                job: Job::Delete {
                    remote: rp(name),
                    folder: name == "old",
                },
                state: JobState::Queued,
                error: None,
                multipart: None,
            });
        }
        journal.insert(&mut records).unwrap();
    };
    let b = bed_with(MockProvider::new(bunny_like()), &["testzone"], crashed);
    assert_eq!(
        b.ui.get_resume_banner_text(),
        "5 transfers didn't finish last time, including 3 deletes."
    );
}

/// Forwards to a mock through `n` transfer slots made with it, as a real client sizes its own
/// from the settings it was built with.
struct Slotted {
    inner: Arc<MockProvider>,
    slots: tokio::sync::Semaphore,
}

impl Slotted {
    fn new(inner: Arc<MockProvider>, n: usize) -> Arc<Self> {
        Arc::new(Self {
            inner,
            slots: tokio::sync::Semaphore::new(n),
        })
    }
}

#[async_trait::async_trait]
impl StorageProvider for Slotted {
    fn capabilities(&self) -> &httporter_core::Capabilities {
        self.inner.capabilities()
    }
    fn endpoint(&self) -> String {
        self.inner.endpoint()
    }
    async fn list(&self, dir: &RemotePath) -> Result<httporter_core::Listing, ProviderError> {
        self.inner.list(dir).await
    }
    async fn upload(
        &self,
        path: &RemotePath,
        source: httporter_core::UploadSource,
        opts: &httporter_core::UploadOptions,
    ) -> Result<(), ProviderError> {
        let _slot = self.slots.acquire().await.expect("never closed");
        self.inner.upload(path, source, opts).await
    }
    async fn download(
        &self,
        path: &RemotePath,
    ) -> Result<httporter_core::ByteStream, ProviderError> {
        self.inner.download(path).await
    }
    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        self.inner.delete_file(path).await
    }
    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.inner.delete_folder(dir).await
    }
    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.inner.create_folder(dir).await
    }
    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        self.inner.delete_batch(paths).await
    }
    async fn reachable(&self) -> Result<(), ProviderError> {
        self.inner.reachable().await
    }
}

/// Review I3: new settings reach a site whose transfers are under way. Its lane gets a client
/// built with them for the work still queued (the one in flight finishes on the old one), so a
/// raised Simultaneous runs more at once and a new proxy reaches the queued jobs.
#[test]
fn new_settings_reach_transfers_under_way() {
    let b = bed(MockProvider::new(bunny_like()));
    b.t.apply_settings(TransferSettings {
        simultaneous: 1,
        ..TransferSettings::default()
    });
    let old = Arc::new(MockProvider::new(bunny_like()));
    old.set_latency(MockOp::Upload, Duration::from_millis(400));
    b.factory.serve("testzone", Slotted::new(old.clone(), 1));
    let files: Vec<PathBuf> = (0..6).map(|i| b.file(&format!("f{i}.txt"), "x")).collect();
    b.t.upload(files, None);
    wait_for("the first upload", || {
        (old.calls(MockOp::Upload) == 1).then_some(())
    });

    let new = Arc::new(MockProvider::new(bunny_like()));
    new.set_latency(MockOp::Upload, Duration::from_millis(400));
    b.factory.serve("testzone", Slotted::new(new.clone(), 4));
    let changed = TransferSettings {
        simultaneous: 4,
        proxy: httporter_core::ProxyMode::Custom("http://proxy.example:8080".into()),
        ..TransferSettings::default()
    };
    b.t.apply_settings(changed.clone());
    b.wait_idle();

    assert_eq!(old.calls(MockOp::Upload), 1, "only the one in flight");
    assert_eq!(new.calls(MockOp::Upload), 5);
    assert!(
        new.max_in_flight(MockOp::Upload) >= 3,
        "{} at once",
        new.max_in_flight(MockOp::Upload)
    );
    let built = b.factory.full_configs();
    assert_eq!(built.last().unwrap().proxy, changed.proxy);
}

/// Review M2: saving an edited site hands its new client to the transfers queued for it, not
/// only to a site stopped by a failed sign-in.
#[test]
fn an_edited_sites_queued_transfers_use_its_new_client() {
    let mock = MockProvider::new(bunny_like());
    mock.set_latency(MockOp::Upload, Duration::from_millis(300));
    let b = bed(mock);
    let site = b.sites[0].clone();
    b.t.apply_settings(TransferSettings {
        simultaneous: 1,
        ..TransferSettings::default()
    });
    let files: Vec<PathBuf> = (0..3).map(|i| b.file(&format!("f{i}.txt"), "x")).collect();
    b.t.upload(files, None);
    wait_for("the first upload", || {
        (b.mock.calls(MockOp::Upload) == 1).then_some(())
    });

    let edited = Arc::new(MockProvider::new(bunny_like()));
    b.factory.serve("testzone", edited.clone());
    b.t.session.hub().invalidate(&site.id);
    b.rt().block_on(b.t.site_edited(&site));
    b.wait_idle();
    assert_eq!(b.mock.calls(MockOp::Upload), 1, "only the one in flight");
    assert_eq!(edited.calls(MockOp::Upload), 2);
}

/// Review I6: a second plan that needs the conflict sheet while one is already up is not lost:
/// the first sheet stays, and the second is shown once the first is answered.
#[test]
fn a_second_conflict_sheet_waits_for_the_first() {
    let b = bed(MockProvider::new(bunny_like())
        .with_file("a.txt", b"old a")
        .with_file("b.txt", b"old b"));
    let a = b.file("a.txt", "new a");
    let second = b.file("b.txt", "new b");
    b.t.upload(vec![a], None);
    let (_, conflicts, _, _) = b.wait_for_conflicts();
    assert_eq!(conflicts[0].name, "a.txt");

    // The second plan arrives while the first sheet is up: it waits.
    b.t.upload(vec![second], None);
    wait_for("the second plan to wait", || {
        (lock(&b.t.state).waiting.len() == 1).then_some(())
    });
    let (_, conflicts, _, _) = b.wait_for_conflicts();
    assert_eq!(conflicts[0].name, "a.txt", "the first sheet stays");

    // A delete asked for meanwhile waits behind it.
    let entries = b.t.session.remote_entries(&["a.txt".into()]);
    b.t.delete(entries);
    wait_for("the delete to wait", || {
        (lock(&b.t.state).waiting.len() == 2).then_some(())
    });
    assert!(lock(&b.t.state).delete.is_none());

    // The first sheet is answered: the second comes up, then the delete alert.
    b.ui.invoke_conflicts_cancel();
    let (_, conflicts, _, _) = b.wait_for_conflicts();
    assert_eq!(conflicts[0].name, "b.txt");
    assert_eq!(lock(&b.t.state).waiting.len(), 1);
    b.ui.invoke_conflicts_cancel();
    assert!(lock(&b.t.state).conflict.is_none());
    assert!(lock(&b.t.state).delete.is_some(), "the delete alert is up");
    assert!(lock(&b.t.state).waiting.is_empty());
    b.ui.invoke_delete_cancel();
    assert!(!lock(&b.t.state).asking());
    assert_eq!(b.stored("a.txt").as_deref(), Some("old a"));
    assert_eq!(b.stored("b.txt").as_deref(), Some("old b"));
}

/// Review I7: a download says which items get other names on this PC: those Windows can't
/// store, then, in their own sentence, those whose names differ only in capital letters.
#[test]
fn download_renames_are_told() {
    let renamed = [
        (rp("docs/A.txt"), "A.txt".to_owned()),
        (rp("docs/CON"), "_CON".to_owned()),
        (rp("docs/a.txt"), "a (2).txt".to_owned()),
        (rp("docs/a:b"), "a_b".to_owned()),
        (rp("docs/what?.txt"), "what_.txt".to_owned()),
    ];
    assert_eq!(
        renamed_note(&renamed).as_deref(),
        Some(
            "3 items get other names on this PC (Windows can't store their names). CON → _CON. \
             2 items have names that differ only in capital letters, which Windows treats as the \
             same, so they're numbered. a.txt → a (2).txt."
        )
    );
    assert_eq!(
        renamed_note(&[(rp("a:b"), "a_b".to_owned())]).as_deref(),
        Some("1 item gets another name on this PC (Windows can't store its name): a:b → a_b.")
    );
    assert_eq!(renamed_note(&[]), None);
}

/// Review I7, through the app: a download with a name Windows can't store is planned with the
/// renaming, which the status line tells.
#[test]
fn a_download_with_a_name_windows_cant_store_is_renamed() {
    let b = bed(MockProvider::new(bunny_like()).with_file("a:b.txt", b"colon"));
    let out = tempfile::tempdir().unwrap();
    let entries = b.t.session.remote_entries(&["a:b.txt".into()]);
    assert_eq!(entries.len(), 1);
    b.t.download(entries, Some(out.path().to_owned()));
    b.wait_idle();
    wait_for("the download", || {
        out.path().join("a_b.txt").exists().then_some(())
    });
    assert_eq!(std::fs::read(out.path().join("a_b.txt")).unwrap(), b"colon");
}

/// Review I4: a site that finds the PC's clock off stops once, with its own banner (not the
/// sign-in one), and the banner's Resume carries on with the same site once the clock is set.
#[test]
fn a_clock_that_is_off_has_its_banner_and_resume_goes_on() {
    let mock = MockProvider::new(bunny_like());
    mock.fail_next(MockOp::Upload, ProviderError::ClockSkew, 1);
    let b = bed(mock);
    let file = b.file("a.txt", "a");
    b.t.upload(vec![file], None);
    wait_for("the clock stop", || {
        matches!(b.t.engine.snapshot().state, EngineState::ClockSkew { .. }).then_some(())
    });
    b.t.tick(&b.ui);
    assert_eq!(
        b.ui.get_clock_banner_text(),
        "Your PC's clock is off. Turn on “Set time automatically” in Windows Settings. \
         Transfers to testzone are paused until you click Resume."
    );
    assert_eq!(b.ui.get_transfers_title(), "PC clock is off · testzone");
    assert_eq!(b.ui.get_auth_banner_text(), "");
    assert_eq!(b.t.engine.snapshot().files_failed, 0);
    let resume = ElementHandle::find_by_accessible_label(&b.ui, "Resume")
        .find(|e| e.accessible_role() == Some(AccessibleRole::Button))
        .expect("the clock banner's Resume");
    resume.invoke_accessible_default_action();
    b.wait_stored("a.txt", "a");
    b.wait_idle();
    b.t.tick(&b.ui);
    assert_eq!(b.ui.get_clock_banner_text(), "");
    assert_eq!(
        b.mock.calls(MockOp::Upload),
        2,
        "the same client, once more"
    );
}

/// Review I5: a file the site refused alone is said as such in the Failed tab.
#[test]
fn a_file_refused_alone_says_so() {
    let denied = ProviderError::Refused(httporter_s3::ACCESS_DENIED.into());
    assert_eq!(
        copy::describe_transfer_error(&denied),
        "Access to this file was denied."
    );
    assert_eq!(
        copy::transfer_message(httporter_transfer::messages::ACCESS_DENIED),
        "Access to this file was denied."
    );
    // Other refusals keep their own words.
    assert_eq!(
        copy::describe_transfer_error(&ProviderError::Refused("Not here.".into())),
        "Not here."
    );
}

// ---- The follow-up to the fix wave (minors A and B) ----

/// Review minor A: closing waits [`SETTLE_FOR`] at most in all. `quit` sets the deadline, and
/// `finish`, once the event loop has stopped, waits only for what is left of it, not
/// [`SETTLE_FOR`] again.
#[test]
fn closing_waits_for_one_deadline_not_two() {
    let journal = Arc::new(SlowJournal::default());
    let b = bed_on(
        MockProvider::new(bunny_like()),
        &["testzone"],
        &|_, _, _| {},
        journal.clone(),
    );
    b.t.engine.pause_all();
    let storage: Arc<dyn StorageProvider> = b.mock.clone();
    b.t.engine.submit(deletes(&b, storage, 3)).unwrap();
    // A disk that has stopped answering: the cancels never get written.
    journal.hold(true);
    let before = Instant::now();
    b.t.quit(Quit::Cancel);
    let deadline = lock(&b.t.state).closing_by.expect("quit sets the deadline");
    assert!(deadline >= before + SETTLE_FOR && deadline <= Instant::now() + SETTLE_FOR);

    // As if all but 300 ms of it had gone by while the window waited.
    let left = Instant::now() + Duration::from_millis(300);
    lock(&b.t.state).closing_by = Some(left);
    let started = Instant::now();
    b.t.finish();
    let took = started.elapsed();
    assert!(Instant::now() >= left);
    assert!(took < Duration::from_secs(2), "finish waited {took:?}");
    assert!(!b.t.settled(Quit::Cancel), "the writes are still held");
    journal.hold(false);
}

/// Review minor B: the settings go A, B, then back to A while the clients for the first A are
/// still being built. That build ends last, holding a client made while B was in use; it is
/// not handed to the engine, though A is in use again: only the latest change's clients are,
/// with its limits.
#[test]
fn a_build_for_settings_changed_since_is_not_handed_over() {
    let b = bed_with(
        MockProvider::new(bunny_like()),
        &["testzone", "zone-b"],
        &|_, _, _| {},
    );
    let zones = ["testzone", "zone-b"];
    let site_of = |zone: &str| {
        b.sites
            .iter()
            .find(|site| site.name == zone)
            .unwrap()
            .clone()
    };
    // Both sites have a lane, with a transfer held in its queue.
    b.t.engine.pause_all();
    for zone in zones {
        let site = site_of(zone);
        b.t.engine
            .submit(Batch {
                site: site.id.clone(),
                site_name: site.name.clone(),
                host: site.host.clone(),
                provider: b.mock.clone(),
                jobs: vec![Job::Delete {
                    remote: rp("old.txt"),
                    folder: false,
                }],
                checksums: HashMap::new(),
            })
            .unwrap();
    }
    let with = |simultaneous| TransferSettings {
        simultaneous,
        ..TransferSettings::default()
    };
    let (a, b_settings) = (with(2), with(3));
    let limit = || b.t.engine.site_transfer_limit(&site_of("testzone").id);

    // A: its build is held at the first site it makes a client for.
    let holds = [b.factory.hold(zones[0]), b.factory.hold(zones[1])];
    b.t.apply_settings(a.clone());
    let first = wait_for("the first build", || {
        (0..2).find(|&i| holds[i].has_entered())
    });
    let second = 1 - first;

    // B: built (its second site once let through) and handed over.
    b.t.apply_settings(b_settings);
    holds[second].wait_entered();
    holds[second].release();
    wait_for("B's limits", || (limit() == Some(3)).then_some(()));

    // The first build goes on now, while B is in use: the second site's client is B's. It then
    // waits to be handed over while A is chosen again, and its own build is held.
    let applying = lock(&b.t.applying);
    let built = b.factory.configs().len();
    holds[first].release();
    wait_for("the first build to go on", || {
        (b.factory.configs().len() > built).then_some(())
    });
    std::thread::sleep(Duration::from_millis(100));
    let again = b.factory.hold(zones[first]);
    b.t.apply_settings(a.clone());
    again.wait_entered();
    drop(applying);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(limit(), Some(3), "the first build for A was handed over");

    // The latest change's clients, and its limits.
    again.release();
    wait_for("A's limits", || (limit() == Some(2)).then_some(()));
    assert_eq!(b.t.session.settings(), a);
}
