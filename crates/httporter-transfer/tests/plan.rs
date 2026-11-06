// SPDX-License-Identifier: MIT OR Apache-2.0

//! Planning uploads, downloads and deletes against `MockProvider` and real local files in a
//! temporary folder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use httporter_core::{
    Capabilities, Checksum, ChecksumKind, CompareBy, ConflictPolicy, DeleteSummary, EmptyFolders,
    Entry, Job, JobState, MemoryJournal, MockOp, MockProvider, ProviderError, RemotePath,
    Resolution, SiteId, StorageProvider, TransferSettings,
};
use httporter_transfer::{
    Batch, Engine, keep_both_name, plan_delete, plan_download, plan_upload, policy_to_resolution,
    reason, resolve_downloads, resolve_uploads, safe_local_name,
};
use md5::{Digest, Md5};

fn p(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

/// Writes `data` to `path`, making its folders first.
fn write(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

fn listing(kind: Option<ChecksumKind>) -> Capabilities {
    Capabilities {
        checksum_in_listing: kind,
        ..Capabilities::default()
    }
}

/// The entry the mock lists for `path`, as a pane would hold it.
async fn listed(mock: &MockProvider, path: &str) -> Entry {
    let path = p(path);
    let parent = path.parent().unwrap();
    mock.list(&parent)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("{} is not listed", path.as_str()))
}

fn upload(local: &Path, remote: &str, size: u64) -> Job {
    Job::Upload {
        local: local.to_owned(),
        remote: p(remote),
        size,
    }
}

fn download(remote: &str, local: &Path, size: u64) -> Job {
    Job::Download {
        remote: p(remote),
        local: local.to_owned(),
        size,
    }
}

fn delete(remote: &str, folder: bool) -> Job {
    Job::Delete {
        remote: p(remote),
        folder,
    }
}

fn names(list: &[&str]) -> HashSet<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

fn none() -> HashMap<usize, Resolution> {
    HashMap::new()
}

#[tokio::test]
async fn upload_of_a_folder_recurses_and_maps_paths() {
    let dir = tempfile::tempdir().unwrap();
    let x = dir.path().join("x");
    let site = x.join("site");
    write(&site.join("a").join("b.txt"), b"bee");
    write(&site.join("index.html"), b"<html>");
    write(&x.join("top.txt"), b"top");
    let mock = MockProvider::new(Capabilities::default());

    let plan = plan_upload(&mock, &[site.clone(), x.join("top.txt")], &p("/"))
        .await
        .unwrap();
    assert_eq!(
        plan.jobs,
        [
            upload(&site.join("a").join("b.txt"), "site/a/b.txt", 3),
            upload(&site.join("index.html"), "site/index.html", 6),
            upload(&x.join("top.txt"), "top.txt", 3),
        ]
    );
    assert_eq!((plan.files, plan.bytes), (3, 12));
    assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    assert_eq!(plan.folders_to_create, [p("site"), p("site/a")]);

    // Into a folder below the root, which doesn't exist yet either.
    let plan = plan_upload(&mock, std::slice::from_ref(&site), &p("www/v2"))
        .await
        .unwrap();
    assert_eq!(
        plan.jobs,
        [
            upload(&site.join("a").join("b.txt"), "www/v2/site/a/b.txt", 3),
            upload(&site.join("index.html"), "www/v2/site/index.html", 6),
        ]
    );
    assert_eq!(
        plan.folders_to_create,
        [p("www/v2/site"), p("www/v2/site/a")]
    );
}

#[tokio::test]
async fn only_existing_remote_folders_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    write(&site.join("a").join("b").join("c.txt"), b"c");
    write(&site.join("d").join("e.txt"), b"e");

    // Nothing of `site` is on the storage: the root is listed, nothing below it.
    let mock = MockProvider::new(Capabilities::default()).with_file("other/x.txt", b"x");
    let plan = plan_upload(&mock, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(mock.calls(MockOp::List), 1);
    assert_eq!(plan.files, 2);
    assert_eq!(
        plan.folders_to_create,
        [p("site"), p("site/a"), p("site/a/b"), p("site/d")]
    );

    // `site` and `site/d` exist, `site/a` doesn't: three listings, none below `site/a`.
    let mock = MockProvider::new(Capabilities::default()).with_file("site/d/e.txt", b"old");
    let plan = plan_upload(&mock, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(mock.calls(MockOp::List), 3);
    assert_eq!(
        plan.jobs,
        [upload(&site.join("a/b/c.txt"), "site/a/b/c.txt", 1)]
    );
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].remote, p("site/d/e.txt"));
    assert_eq!(plan.folders_to_create, [p("site/a"), p("site/a/b")]);

    // A target that isn't there is listed once and nothing below it.
    let mock = MockProvider::new(Capabilities::default());
    let plan = plan_upload(&mock, std::slice::from_ref(&site), &p("new/place"))
        .await
        .unwrap();
    assert_eq!(mock.calls(MockOp::List), 1);
    assert_eq!(plan.files, 2);
}

#[tokio::test]
async fn conflicts_compare_by_sha256_on_bunny_and_md5_on_s3() {
    for (kind, by) in [
        (ChecksumKind::Sha256, CompareBy::Sha256),
        (ChecksumKind::Md5, CompareBy::Md5),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let up = dir.path().join("up");
        write(&up.join("same.txt"), b"same bytes");
        write(&up.join("diff.txt"), b"differ!!!!");
        write(&up.join("size.txt"), b"much longer now");
        write(&up.join("new.txt"), b"new");
        let mock = MockProvider::new(listing(Some(kind)))
            .with_file("up/same.txt", b"same bytes")
            .with_file("up/diff.txt", b"other byte")
            .with_file("up/size.txt", b"short");

        let plan = plan_upload(&mock, std::slice::from_ref(&up), &RemotePath::root())
            .await
            .unwrap();
        assert_eq!(plan.jobs, [upload(&up.join("new.txt"), "up/new.txt", 3)]);
        assert_eq!((plan.files, plan.bytes), (4, 10 + 10 + 15 + 3));
        let found: Vec<_> = plan
            .conflicts
            .iter()
            .map(|c| (c.index, c.name.as_str(), c.same, c.compared_by))
            .collect();
        assert_eq!(
            found,
            [
                (0, "diff.txt", Some(false), by),
                (1, "same.txt", Some(true), by),
                // Sizes that differ settle it without reading the file.
                (2, "size.txt", Some(false), CompareBy::Size),
            ],
            "{kind:?}"
        );
        let same = &plan.conflicts[1];
        assert_eq!(same.local, up.join("same.txt"));
        assert_eq!(same.remote, p("up/same.txt"));
        assert_eq!((same.local_size, same.remote_size), (10, 10));
        assert!(same.local_modified.is_some() && same.remote_modified.is_some());
        let size = &plan.conflicts[2];
        assert_eq!((size.local_size, size.remote_size), (15, 5));
    }
}

#[tokio::test]
async fn multipart_etag_compares_by_size() {
    let dir = tempfile::tempdir().unwrap();
    let up = dir.path().join("up");
    write(&up.join("big.bin"), &[2; 100]);
    write(&up.join("grown.bin"), &[1; 150]);
    let mock = MockProvider::new(listing(Some(ChecksumKind::Md5)))
        .with_file("up/big.bin", &[1; 100])
        .with_file_etag("up/big.bin", "0123456789abcdef0123456789abcdef-3")
        .with_file("up/grown.bin", &[1; 100])
        .with_file_etag("up/grown.bin", "fedcba9876543210fedcba9876543210-2");
    let plan = plan_upload(&mock, std::slice::from_ref(&up), &RemotePath::root())
        .await
        .unwrap();
    let found: Vec<_> = plan
        .conflicts
        .iter()
        .map(|c| (c.name.as_str(), c.same, c.compared_by))
        .collect();
    assert_eq!(
        found,
        [
            ("big.bin", Some(true), CompareBy::Size),
            ("grown.bin", Some(false), CompareBy::Size),
        ]
    );

    // A listing without checksums compares by size too.
    let mock = MockProvider::new(listing(None)).with_file("up/big.bin", &[1; 100]);
    let plan = plan_upload(&mock, std::slice::from_ref(&up), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(plan.conflicts[0].same, Some(true));
    assert_eq!(plan.conflicts[0].compared_by, CompareBy::Size);
}

#[tokio::test]
async fn resolution_replace_if_different_skips_identical() {
    let dir = tempfile::tempdir().unwrap();
    let up = dir.path().join("up");
    write(&up.join("same.txt"), b"same bytes");
    write(&up.join("diff.txt"), b"differ!!!!");
    write(&up.join("new.txt"), b"new");
    let mock = MockProvider::new(listing(Some(ChecksumKind::Sha256)))
        .with_file("up/same.txt", b"same bytes")
        .with_file("up/diff.txt", b"other byte");
    let plan = plan_upload(&mock, std::slice::from_ref(&up), &RemotePath::root())
        .await
        .unwrap();
    let new = upload(&up.join("new.txt"), "up/new.txt", 3);
    let diff = upload(&up.join("diff.txt"), "up/diff.txt", 10);
    let same = upload(&up.join("same.txt"), "up/same.txt", 10);

    let jobs = resolve_uploads(plan.clone(), Resolution::ReplaceIfDifferent, &none());
    assert_eq!(jobs, [new.clone(), diff.clone()]);
    let jobs = resolve_uploads(plan.clone(), Resolution::Replace, &none());
    assert_eq!(jobs, [new.clone(), diff.clone(), same.clone()]);
    let jobs = resolve_uploads(plan.clone(), Resolution::Skip, &none());
    assert_eq!(jobs, std::slice::from_ref(&new));
    // A row's own choice beats the one for all.
    let rows = HashMap::from([(1, Resolution::Replace)]);
    let jobs = resolve_uploads(plan, Resolution::Skip, &rows);
    assert_eq!(jobs, [new, same]);
}

#[test]
fn policies_map_to_resolutions() {
    assert_eq!(policy_to_resolution(ConflictPolicy::Ask), None);
    assert_eq!(
        policy_to_resolution(ConflictPolicy::Replace),
        Some(Resolution::Replace)
    );
    assert_eq!(
        policy_to_resolution(ConflictPolicy::ReplaceIfDifferent),
        Some(Resolution::ReplaceIfDifferent)
    );
    assert_eq!(
        policy_to_resolution(ConflictPolicy::Skip),
        Some(Resolution::Skip)
    );
    assert_eq!(
        policy_to_resolution(ConflictPolicy::KeepBoth),
        Some(Resolution::KeepBoth)
    );
}

#[tokio::test]
async fn keep_both_avoids_taken_and_planned_names() {
    assert_eq!(
        keep_both_name("photo.jpg", &HashSet::new()),
        "photo (1).jpg"
    );
    assert_eq!(
        keep_both_name(".htaccess", &HashSet::new()),
        ".htaccess (1)"
    );
    assert_eq!(
        keep_both_name("archive.tar.gz", &HashSet::new()),
        "archive.tar (1).gz"
    );
    assert_eq!(keep_both_name("README", &HashSet::new()), "README (1)");
    assert_eq!(
        keep_both_name("photo.jpg", &names(&["photo (1).jpg", "photo (2).jpg"])),
        "photo (3).jpg"
    );

    // Uploads: `photo (1).jpg` is on the site and `photo (2).jpg` is part of this upload.
    let dir = tempfile::tempdir().unwrap();
    let pics = dir.path().join("pics");
    write(&pics.join("photo.jpg"), b"new photo");
    write(&pics.join("photo (2).jpg"), b"another");
    write(&pics.join("notes.txt"), b"notes");
    let mock = MockProvider::new(listing(Some(ChecksumKind::Sha256)))
        .with_file("pics/photo.jpg", b"old photo")
        .with_file("pics/photo (1).jpg", b"older")
        .with_file("pics/notes.txt", b"old notes");
    let plan = plan_upload(&mock, std::slice::from_ref(&pics), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(plan.conflicts.len(), 2);
    let jobs = resolve_uploads(plan, Resolution::KeepBoth, &none());
    assert_eq!(
        jobs,
        [
            upload(&pics.join("photo (2).jpg"), "pics/photo (2).jpg", 7),
            upload(&pics.join("notes.txt"), "pics/notes (1).txt", 5),
            upload(&pics.join("photo.jpg"), "pics/photo (3).jpg", 9),
        ]
    );

    // Downloads: names on the PC are taken whatever their case.
    let out = dir.path().join("out");
    write(&out.join("pics").join("photo.jpg"), b"mine");
    write(&out.join("pics").join("PHOTO (1).JPG"), b"also mine");
    let mock = MockProvider::new(listing(Some(ChecksumKind::Sha256)))
        .with_file("pics/photo.jpg", b"theirs")
        .with_file("pics/photo (2).jpg", b"theirs too");
    let folder = listed(&mock, "pics").await;
    let plan = plan_download(&mock, &[folder], &out).await.unwrap();
    assert_eq!(plan.conflicts.len(), 1);
    let jobs = resolve_downloads(plan, Resolution::KeepBoth, &none());
    assert_eq!(
        jobs,
        [
            download(
                "pics/photo (2).jpg",
                &out.join("pics").join("photo (2).jpg"),
                10
            ),
            download("pics/photo.jpg", &out.join("pics").join("photo (3).jpg"), 6),
        ]
    );
}

#[tokio::test]
async fn file_onto_folder_is_refused_in_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    write(&site.join("a.txt"), b"a file");
    write(&site.join("img").join("one.png"), b"1");
    write(&site.join("img").join("sub").join("two.png"), b"2");
    write(&site.join("ok.txt"), b"ok");
    // On the site, `a.txt` is a folder and `img` a file.
    let mock = MockProvider::new(Capabilities::default())
        .with_file("site/a.txt/inner.txt", b"x")
        .with_file("site/img", b"a file");
    let plan = plan_upload(&mock, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(
        plan.skipped,
        [
            (site.join("a.txt"), reason::FOLDER_THERE.to_owned()),
            (site.join("img"), reason::FILE_THERE.to_owned()),
        ]
    );
    assert_eq!(plan.jobs, [upload(&site.join("ok.txt"), "site/ok.txt", 2)]);
    assert_eq!((plan.files, plan.bytes), (1, 2));
    assert!(plan.conflicts.is_empty());
    assert!(plan.folders_to_create.is_empty());
    assert_eq!(mock.calls(MockOp::List), 2, "the root and site, not img");
}

#[tokio::test]
async fn unreadable_local_file_is_skipped_with_os_reason() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    write(&site.join("fine.txt"), b"fine");

    // A source that is gone by the time it is planned.
    let gone = dir.path().join("gone.txt");
    let mock = MockProvider::new(Capabilities::default());
    let plan = plan_upload(&mock, &[gone.clone(), site.clone()], &RemotePath::root())
        .await
        .unwrap();
    let missing = std::fs::metadata(&gone).unwrap_err().to_string();
    assert_eq!(plan.skipped, [(gone, missing)]);
    assert_eq!(
        plan.jobs,
        [upload(&site.join("fine.txt"), "site/fine.txt", 4)]
    );

    // A file another program holds open without sharing: it is read only to compare it.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        let locked = site.join("locked.txt");
        write(&locked, b"locked!!!!");
        let mock = MockProvider::new(listing(Some(ChecksumKind::Sha256)))
            .with_file("site/locked.txt", b"same size!");
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();
        let why = std::fs::File::open(&locked).unwrap_err().to_string();
        let plan = plan_upload(&mock, std::slice::from_ref(&site), &RemotePath::root())
            .await
            .unwrap();
        assert_eq!(plan.skipped, [(locked, why)]);
        assert!(plan.conflicts.is_empty());
        assert_eq!(
            plan.jobs,
            [upload(&site.join("fine.txt"), "site/fine.txt", 4)]
        );
        assert_eq!((plan.files, plan.bytes), (1, 4));
    }
}

/// Makes `link` a link to the folder `target`: a symbolic link where allowed, otherwise a
/// junction, which needs no privileges.
fn link_folder(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return;
        }
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "mklink /J failed: {out:?}");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[tokio::test]
async fn linked_folders_are_skipped_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    write(&site.join("real").join("x.txt"), b"x");
    let outside = dir.path().join("outside");
    write(&outside.join("secret.txt"), b"not for the site");
    let link = site.join("link");
    link_folder(&link, &outside);
    let mock = MockProvider::new(Capabilities::default());

    let plan = plan_upload(&mock, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(
        plan.jobs,
        [upload(&site.join("real/x.txt"), "site/real/x.txt", 1)]
    );
    assert_eq!(
        plan.skipped,
        [(link.clone(), reason::LINKED_FOLDER.to_owned())]
    );

    // Given on its own, it is not followed either.
    let plan = plan_upload(&mock, std::slice::from_ref(&link), &RemotePath::root())
        .await
        .unwrap();
    assert!(plan.jobs.is_empty());
    assert_eq!(plan.skipped, [(link, reason::LINKED_FOLDER.to_owned())]);
}

/// A folder path below `base` longer than Windows' classic 260-character limit.
fn deep_folder(base: &Path) -> PathBuf {
    let mut deep = base.to_owned();
    for i in 0..6 {
        deep.push(format!("{i}-{}", "d".repeat(48)));
    }
    deep
}

/// `deep_folder`'s segments below `site`, as a remote path.
fn deep_remote() -> String {
    let segments: Vec<String> = (0..6).map(|i| format!("{i}-{}", "d".repeat(48))).collect();
    format!("site/{}", segments.join("/"))
}

#[tokio::test]
async fn long_paths_over_260_chars_work() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    let deep = deep_folder(&site);
    let page = deep.join("page.html");
    write(&page, b"long way down");
    write(&deep.join("new.txt"), b"new");
    assert!(page.as_os_str().len() > 260, "{}", page.display());
    let remote_page = format!("{}/page.html", deep_remote());
    let mock = MockProvider::new(listing(Some(ChecksumKind::Sha256)))
        .with_file(&remote_page, b"long way down");

    let plan = plan_upload(&mock, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    assert_eq!(
        plan.jobs,
        [upload(
            &deep.join("new.txt"),
            &format!("{}/new.txt", deep_remote()),
            3
        )]
    );
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].same, Some(true));
    assert_eq!(plan.conflicts[0].local, page);

    // Downloads into the same place find the long local file and read it to compare.
    let folder = listed(&mock, "site").await;
    let plan = plan_download(&mock, &[folder], dir.path()).await.unwrap();
    assert!(plan.jobs.is_empty(), "{:?}", plan.jobs);
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].local, page);
    assert_eq!(plan.conflicts[0].same, Some(true));
    assert_eq!(plan.conflicts[0].compared_by, CompareBy::Sha256);
}

/// A name that isn't valid Unicode, which the journal can't store (ledger 164).
fn lossy_name(stem: &str) -> std::ffi::OsString {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<u16> = stem.encode_utf16().collect();
        wide.push(0xD800); // half of a surrogate pair, alone
        wide.extend(".txt".encode_utf16());
        std::ffi::OsString::from_wide(&wide)
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = stem.as_bytes().to_vec();
        bytes.extend_from_slice(b"\xff.txt");
        std::ffi::OsString::from_vec(bytes)
    }
}

#[tokio::test]
async fn lossy_names_are_skipped_with_reason() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    write(&site.join("good.txt"), b"good");
    let bad_file = site.join(lossy_name("bad"));
    write(&bad_file, b"bad");
    let bad_folder = site.join(lossy_name("folder"));
    write(&bad_folder.join("inside.txt"), b"inside");
    let mock = MockProvider::new(Capabilities::default());

    let plan = plan_upload(&mock, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(
        plan.jobs,
        [upload(&site.join("good.txt"), "site/good.txt", 4)]
    );
    let mut skipped = plan.skipped.clone();
    skipped.sort();
    let mut expected = vec![
        (bad_file.clone(), reason::NAME.to_owned()),
        (bad_folder, reason::NAME.to_owned()),
    ];
    expected.sort();
    assert_eq!(skipped, expected);

    // Given on its own, too.
    let plan = plan_upload(&mock, std::slice::from_ref(&bad_file), &RemotePath::root())
        .await
        .unwrap();
    assert!(plan.jobs.is_empty());
    assert_eq!(plan.skipped, [(bad_file, reason::NAME.to_owned())]);
}

#[test]
fn safe_local_names_for_windows() {
    let cases = [
        ("a:b", "a_b"),
        ("name.", "name"),
        ("CON.txt", "_CON.txt"),
        ("x?y*", "x_y_"),
        ("aux", "_aux"),
        ("index.html", "index.html"),
        ("a<b>c|d\"e\\f", "a_b_c_d_e_f"),
        ("tab\there", "tab_here"),
        ("trailing. . ", "trailing"),
        ("...", "_"),
        ("nul.tar.gz", "_nul.tar.gz"),
        ("Com1", "_Com1"),
        ("LPT9.log", "_LPT9.log"),
        ("CON .txt", "_CON .txt"),
        ("COM10", "COM10"),
        ("console.txt", "console.txt"),
        ("naïve résumé.pdf", "naïve résumé.pdf"),
    ];
    for (name, safe) in cases {
        assert_eq!(safe_local_name(name), safe, "{name:?}");
    }
}

#[tokio::test]
async fn case_only_duplicates_get_distinct_names() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockProvider::new(Capabilities::default())
        .with_file("docs/A.txt", b"upper")
        .with_file("docs/a.txt", b"lower")
        .with_file("docs/a:b.txt", b"colon")
        .with_file("docs/CON", b"device")
        .with_file("docs/plain.txt", b"plain");
    let folder = listed(&mock, "docs").await;
    let out = dir.path().join("out");
    let plan = plan_download(&mock, &[folder], &out).await.unwrap();
    let docs = out.join("docs");
    assert_eq!(
        plan.jobs,
        [
            download("docs/A.txt", &docs.join("A.txt"), 5),
            download("docs/CON", &docs.join("_CON"), 6),
            download("docs/a.txt", &docs.join("a (2).txt"), 5),
            download("docs/a:b.txt", &docs.join("a_b.txt"), 5),
            download("docs/plain.txt", &docs.join("plain.txt"), 5),
        ]
    );
    let mut renamed = plan.renamed.clone();
    renamed.sort();
    assert_eq!(
        renamed,
        [
            (p("docs/A.txt"), "A.txt".to_owned()),
            (p("docs/CON"), "_CON".to_owned()),
            (p("docs/a.txt"), "a (2).txt".to_owned()),
            (p("docs/a:b.txt"), "a_b.txt".to_owned()),
        ]
    );
    assert_eq!((plan.files, plan.bytes), (5, 26));
    assert!(plan.conflicts.is_empty());
}

#[tokio::test]
async fn download_conflicts_compare_like_uploads_and_carry_checksums() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    write(&out.join("same.txt"), b"same bytes");
    write(&out.join("diff.txt"), b"differ!!!!");
    let mock = MockProvider::new(listing(Some(ChecksumKind::Md5)))
        .with_file("same.txt", b"same bytes")
        .with_file("diff.txt", b"other byte")
        .with_file("new.txt", b"new");
    let mut entries = Vec::new();
    for name in ["same.txt", "diff.txt", "new.txt"] {
        entries.push(listed(&mock, name).await);
    }
    let plan = plan_download(&mock, &entries, &out).await.unwrap();
    assert_eq!(plan.jobs, [download("new.txt", &out.join("new.txt"), 3)]);
    let found: Vec<_> = plan
        .conflicts
        .iter()
        .map(|c| (c.index, c.name.as_str(), c.same, c.compared_by))
        .collect();
    assert_eq!(
        found,
        [
            (0, "diff.txt", Some(false), CompareBy::Md5),
            (1, "same.txt", Some(true), CompareBy::Md5),
        ]
    );
    assert_eq!(plan.conflicts[1].local, out.join("same.txt"));
    assert_eq!((plan.files, plan.bytes), (3, 23));
    // Every file's listed checksum, for the engine to check what arrives (ruling Q27).
    assert_eq!(plan.checksums.len(), 3);
    assert_eq!(
        plan.checksums[&p("new.txt")],
        Checksum {
            kind: ChecksumKind::Md5,
            hex: hex::encode(Md5::digest(b"new")),
        }
    );

    let jobs = resolve_downloads(plan.clone(), Resolution::ReplaceIfDifferent, &none());
    assert_eq!(
        jobs,
        [
            download("new.txt", &out.join("new.txt"), 3),
            download("diff.txt", &out.join("diff.txt"), 10),
        ]
    );
    let jobs = resolve_downloads(
        plan,
        Resolution::Skip,
        &HashMap::from([(1, Resolution::Replace)]),
    );
    assert_eq!(
        jobs,
        [
            download("new.txt", &out.join("new.txt"), 3),
            download("same.txt", &out.join("same.txt"), 10),
        ]
    );
}

/// A file entry as a listing shows it.
fn file_entry(path: &str, size: u64) -> Entry {
    Entry {
        path: p(path),
        is_folder: false,
        size,
        modified: None,
        checksum: None,
        content_type: None,
        replicated_regions: Vec::new(),
    }
}

#[tokio::test]
async fn delete_summary_counts_recursively() {
    let mut mock = MockProvider::new(Capabilities {
        recursive_delete: true,
        ..Capabilities::default()
    });
    let data = vec![0u8; 256_250];
    for i in 0..48 {
        let folder = if i % 2 == 0 { "a" } else { "b/c" };
        mock = mock.with_file(&format!("old-promo-2025/{folder}/img{i}.jpg"), &data);
    }
    mock = mock
        .with_file("index.html", b"0123456789")
        .with_file("blog/post.html", b"post")
        .with_file("docs/one.pdf", b"one")
        .with_file("docs/two.pdf", b"two!");
    let folder = listed(&mock, "old-promo-2025").await;

    let plan = plan_delete(&mock, std::slice::from_ref(&folder))
        .await
        .unwrap();
    assert_eq!(
        plan.summary,
        DeleteSummary {
            folders: 1,
            files: 0,
            total_files: 48,
            total_bytes: 12_300_000,
            single: Some(("old-promo-2025".to_owned(), true)),
        }
    );
    // Even where one request could delete the whole folder, each file goes alone and the
    // folder last (ruling Q54): Bunny's recursive delete outlasts any time limit on big ones.
    assert_eq!(plan.jobs.len(), 49);
    assert!(
        plan.jobs[..48]
            .iter()
            .all(|job| matches!(job, Job::Delete { folder: false, .. }))
    );
    assert_eq!(plan.jobs[48], delete("old-promo-2025", true));

    // Several items: counts of what was picked, and of everything that goes.
    let picked = vec![
        folder,
        listed(&mock, "blog").await,
        listed(&mock, "docs").await,
        listed(&mock, "index.html").await,
    ];
    let plan = plan_delete(&mock, &picked).await.unwrap();
    assert_eq!(
        plan.summary,
        DeleteSummary {
            folders: 3,
            files: 1,
            total_files: 48 + 1 + 2 + 1,
            total_bytes: 12_300_000 + 4 + 7 + 10,
            single: None,
        }
    );
    assert_eq!(plan.jobs.len(), 48 + 1 + 2 + 1 + 3);
    assert_eq!(
        plan.jobs[52..],
        [
            delete("blog", true),
            delete("docs", true),
            delete("old-promo-2025", true),
        ]
    );

    // One file.
    let plan = plan_delete(&mock, &[file_entry("index.html", 10)])
        .await
        .unwrap();
    assert_eq!(plan.summary.single, Some(("index.html".to_owned(), false)));
    assert_eq!(
        (plan.summary.total_files, plan.summary.total_bytes),
        (1, 10)
    );
}

#[tokio::test]
async fn deleting_root_is_refused() {
    let mock = MockProvider::new(Capabilities::default()).with_file("a.txt", b"a");
    let root = Entry {
        is_folder: true,
        ..file_entry("", 0)
    };
    let refused = Err(ProviderError::Refused(
        "The top folder of a site can't be deleted.".into(),
    ));
    assert_eq!(
        plan_delete(&mock, std::slice::from_ref(&root)).await,
        refused
    );
    assert_eq!(
        plan_delete(&mock, &[file_entry("a.txt", 1), root]).await,
        refused
    );
    assert_eq!(
        reason::ROOT_DELETE,
        "The top folder of a site can't be deleted."
    );
    assert_eq!(mock.calls(MockOp::List), 0);
}

fn site() -> SiteId {
    SiteId("site-1".into())
}

fn describe(err: &ProviderError) -> String {
    format!("Failed: {err}")
}

#[tokio::test(start_paused = true)]
async fn delete_without_recursive_capability_expands_to_files_and_markers() {
    // Room for every delete at once, so only the order of the jobs and the engine's waiting
    // keep the folder's delete behind the files inside it.
    let mock = MockProvider::new(Capabilities {
        delete_concurrency: 8,
        ..Capabilities::default()
    })
    .with_file("old/a.txt", b"1")
    .with_file("old/sub/b.txt", b"22")
    .with_file("old/sub/deeper/c.txt", b"333")
    .with_file("x.txt", b"x")
    .with_file("keep.txt", b"keep");
    let picked = vec![listed(&mock, "old").await, listed(&mock, "x.txt").await];
    let plan = plan_delete(&mock, &picked).await.unwrap();
    // Every file first, then one delete of the picked folder, last (ruling Q38): it removes
    // what is left of the folder (markers) once the files are gone.
    assert_eq!(
        plan.jobs,
        [
            delete("old/a.txt", false),
            delete("old/sub/b.txt", false),
            delete("old/sub/deeper/c.txt", false),
            delete("x.txt", false),
            delete("old", true),
        ]
    );
    assert_eq!(
        plan.summary,
        DeleteSummary {
            folders: 1,
            files: 1,
            total_files: 4,
            total_bytes: 7,
            single: None,
        }
    );

    // The engine runs it to the end. The folder's delete waits for the file deletes in flight
    // inside it (sent sooner, this mock would refuse a folder that isn't empty); then the
    // folder, which vanished with its last file, is done (ruling Q45).
    let journal = Arc::new(MemoryJournal::new());
    let engine = Engine::new(
        tokio::runtime::Handle::current(),
        journal.clone(),
        TransferSettings::default(),
        describe,
    );
    let mock = Arc::new(mock);
    mock.set_latency(MockOp::DeleteFile, std::time::Duration::from_secs(1));
    let start = tokio::time::Instant::now();
    engine
        .submit(Batch {
            site: site(),
            site_name: "Test site".into(),
            host: "storage.example".into(),
            provider: mock.clone(),
            jobs: plan.jobs,
            checksums: HashMap::new(),
        })
        .unwrap();
    let mut changes = engine.changes();
    while engine.is_busy() {
        tokio::time::timeout(std::time::Duration::from_secs(10), changes.changed())
            .await
            .expect("the deletes finish")
            .unwrap();
    }
    let s = engine.snapshot();
    assert_eq!((s.files_done, s.files_failed), (5, 0), "{:?}", s.failed);
    assert_eq!(
        start.elapsed(),
        std::time::Duration::from_secs(1),
        "the four files at once"
    );
    assert_eq!(mock.max_in_flight(MockOp::DeleteFile), 4);
    assert_eq!(mock.calls(MockOp::DeleteFolder), 1);
    assert!(journal.records().iter().all(|r| r.state == JobState::Done));
    let left: Vec<String> = mock
        .list(&RemotePath::root())
        .await
        .unwrap()
        .entries
        .iter()
        .map(|e| e.name().to_owned())
        .collect();
    assert_eq!(left, ["keep.txt"]);

    // With recursive deletes too, each file alone and the folder last (ruling Q54).
    let recursive = MockProvider::new(Capabilities {
        recursive_delete: true,
        ..Capabilities::default()
    })
    .with_file("old/a.txt", b"1")
    .with_file("old/sub/b.txt", b"22")
    .with_file("x.txt", b"x");
    let picked = vec![
        listed(&recursive, "old").await,
        listed(&recursive, "x.txt").await,
    ];
    let plan = plan_delete(&recursive, &picked).await.unwrap();
    assert_eq!(
        plan.jobs,
        [
            delete("old/a.txt", false),
            delete("old/sub/b.txt", false),
            delete("x.txt", false),
            delete("old", true),
        ]
    );
}

#[tokio::test]
async fn empty_folders_are_made_where_the_storage_keeps_them() {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    std::fs::create_dir_all(site.join("empty")).unwrap();
    std::fs::create_dir_all(site.join("nest").join("inner")).unwrap();
    write(&site.join("full").join("f.txt"), b"f");

    let keeps = MockProvider::new(Capabilities {
        empty_folders: EmptyFolders::Markers,
        ..Capabilities::default()
    });
    let plan = plan_upload(&keeps, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(
        plan.jobs,
        [
            Job::CreateFolder {
                remote: p("site/empty")
            },
            Job::CreateFolder {
                remote: p("site/nest/inner")
            },
            upload(&site.join("full/f.txt"), "site/full/f.txt", 1),
        ]
    );
    assert_eq!(
        plan.folders_to_create,
        [
            p("site"),
            p("site/empty"),
            p("site/full"),
            p("site/nest"),
            p("site/nest/inner"),
        ]
    );
    assert!(plan.skipped.is_empty());

    let loses = MockProvider::new(Capabilities {
        empty_folders: EmptyFolders::Unsupported,
        ..Capabilities::default()
    });
    let plan = plan_upload(&loses, std::slice::from_ref(&site), &RemotePath::root())
        .await
        .unwrap();
    assert_eq!(
        plan.jobs,
        [upload(&site.join("full/f.txt"), "site/full/f.txt", 1)]
    );
    assert_eq!(
        plan.skipped,
        [
            (site.join("empty"), reason::EMPTY_FOLDER.to_owned()),
            (
                site.join("nest").join("inner"),
                reason::EMPTY_FOLDER.to_owned()
            ),
        ]
    );
    assert_eq!(plan.folders_to_create, [p("site"), p("site/full")]);
}

#[tokio::test]
async fn two_sources_with_one_name_upload_once() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("a").join("same.txt");
    let second = dir.path().join("b").join("same.txt");
    write(&first, b"first");
    write(&second, b"second");
    let mock = MockProvider::new(Capabilities::default());
    let plan = plan_upload(
        &mock,
        &[first.clone(), first.clone(), second.clone()],
        &RemotePath::root(),
    )
    .await
    .unwrap();
    assert_eq!(plan.jobs, [upload(&first, "same.txt", 5)]);
    assert_eq!(plan.skipped, [(second, reason::SAME_NAME.to_owned())]);
}
