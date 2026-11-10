// SPDX-License-Identifier: MIT OR Apache-2.0

//! The I3 to I5 pieces of the core that need no provider: skip rules, the line diff, redaction,
//! the activity filter, the sites file, purge URLs and `run_purge`, the delete guard and the
//! window state kept as JSON.

use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use httporter_core::sites_file::sites_file_text;
use httporter_core::{
    ActivityEvent, ActivityFilter, ActivityKind, CdnPurger, CdnZone, ColumnWidths,
    DEFAULT_SKIP_RULES, DIFF_MAX_LINES, DeleteGuard, DiffOp, ProfileExtras, ProfileId,
    ProfileRecord, ProviderError, ProviderKind, PurgeMode, PurgeResult, RemotePath, S3Preset,
    S3Site, SITES_FORMAT, SITES_VERSION, SiteExtras, SiteId, SiteRecord, SkipRules, TabState,
    UiState, ViewMode, WindowPlacement, diff_lines, export_sites, glob_match, looks_secret,
    parse_sites_file, purge_urls, redact, run_purge,
};

fn p(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

// --- Skip rules -------------------------------------------------------------------------------

#[test]
fn the_twelve_default_rules() {
    assert_eq!(DEFAULT_SKIP_RULES.len(), 12);
    let rules = SkipRules::new(&SkipRules::defaults());
    for skipped in [
        ".env",
        "sub/.env",
        ".env.local",
        "certs/site.key",
        "certs/site.PEM",
        "Thumbs.db",
        "a/desktop.ini",
        ".DS_Store",
        "build/x.tmp",
        "~$report.docx",
        ".git/config",
        "deep/node_modules/pkg/index.js",
        "a/.svn/entries",
    ] {
        assert!(
            rules.is_skipped(skipped, false),
            "{skipped} should be skipped"
        );
    }
    for folder in [".git", "node_modules", "x/.svn"] {
        assert!(rules.is_skipped(folder, true), "folder {folder}");
    }
    for kept in [
        "index.html",
        "env.txt",
        "keys/readme.md",
        "a.keyboard",
        "git/config",
        "node_modules.txt",
        "thumbs.db.bak",
    ] {
        assert!(!rules.is_skipped(kept, false), "{kept} should be kept");
    }
    // A folder-only rule leaves a file of that name alone.
    assert!(!rules.is_skipped("node_modules", false));
}

#[test]
fn skip_rules_anchor_match_whole_paths_and_ignore_blanks() {
    let rules = SkipRules::new(&[
        "/drafts/".into(),
        "assets/*.map".into(),
        "  ".into(),
        String::new(),
    ]);
    assert!(rules.is_skipped("drafts", true));
    assert!(rules.is_skipped("drafts/a.html", false));
    assert!(rules.is_skipped("assets/app.js.map", false));
    assert!(
        !rules.is_skipped("assets/js/app.js.map", false),
        "'*' stops at '/'"
    );
    assert!(!rules.is_skipped("other/assets/app.map", false));
    assert!(!SkipRules::default().is_skipped(".env", false));
    assert!(!rules.is_skipped("", false));
}

#[test]
fn glob_match_stars_questions_and_case() {
    assert!(glob_match("*.TXT", "notes.txt"));
    assert!(glob_match("a?c", "abc"));
    assert!(!glob_match("a?c", "a/c"));
    assert!(!glob_match("*.txt", "dir/notes.txt"));
    assert!(glob_match("*", ""));
    assert!(glob_match("a*b*c", "aXXbYYc"));
    assert!(!glob_match("a*b*c", "aXXbYY"));
    assert!(glob_match("ü*", "Übersicht"));
    assert!(!glob_match("abc", "abcd"));
}

#[test]
fn looks_secret_by_name() {
    for secret in [
        ".env",
        "app/.env.production",
        "id_rsa",
        "keys/id_rsa.pub",
        "cert.PEM",
        "tls.key",
        "store.p12",
        "wp/WP-CONFIG.PHP",
    ] {
        assert!(looks_secret(secret), "{secret}");
    }
    for plain in [
        "index.html",
        "environment.txt",
        "keyboard.png",
        "wp-config.php.bak",
    ] {
        assert!(!looks_secret(plain), "{plain}");
    }
}

// --- Line diff --------------------------------------------------------------------------------

fn ops(old: &str, new: &str) -> Vec<(DiffOp, Option<u32>, Option<u32>, String)> {
    diff_lines(old, new)
        .unwrap()
        .into_iter()
        .map(|l| (l.op, l.old_no, l.new_no, l.text))
        .collect()
}

#[test]
fn diff_insert_delete_and_change() {
    use DiffOp::{Added, Removed, Same};
    let s = |t: &str| t.to_owned();
    assert_eq!(
        ops("a\nc\n", "a\nb\nc\n"),
        [
            (Same, Some(1), Some(1), s("a")),
            (Added, None, Some(2), s("b")),
            (Same, Some(2), Some(3), s("c")),
        ]
    );
    assert_eq!(
        ops("a\nb\nc", "a\nc"),
        [
            (Same, Some(1), Some(1), s("a")),
            (Removed, Some(2), None, s("b")),
            (Same, Some(3), Some(2), s("c")),
        ]
    );
    assert_eq!(
        ops("a\nold\nc", "a\nnew\nc"),
        [
            (Same, Some(1), Some(1), s("a")),
            (Removed, Some(2), None, s("old")),
            (Added, None, Some(2), s("new")),
            (Same, Some(3), Some(3), s("c")),
        ]
    );
}

#[test]
fn diff_of_empty_texts() {
    assert!(diff_lines("", "").unwrap().is_empty());
    let added = diff_lines("", "x\ny").unwrap();
    assert!(added.iter().all(|l| l.op == DiffOp::Added));
    assert_eq!(added.len(), 2);
    let removed = diff_lines("x\ny", "").unwrap();
    assert!(removed.iter().all(|l| l.op == DiffOp::Removed));
}

#[test]
fn diff_is_a_shortest_script_that_rebuilds_both_texts() {
    let old: Vec<String> = (0..200).map(|i| format!("line {}", i % 17)).collect();
    let new: Vec<String> = (0..180).map(|i| format!("line {}", (i * 3) % 13)).collect();
    let (old, new) = (old.join("\n"), new.join("\n"));
    let lines = diff_lines(&old, &new).unwrap();
    let rebuilt_old: Vec<&str> = lines
        .iter()
        .filter(|l| l.op != DiffOp::Added)
        .map(|l| l.text.as_str())
        .collect();
    let rebuilt_new: Vec<&str> = lines
        .iter()
        .filter(|l| l.op != DiffOp::Removed)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(rebuilt_old, old.lines().collect::<Vec<_>>());
    assert_eq!(rebuilt_new, new.lines().collect::<Vec<_>>());
}

#[test]
fn diff_gives_up_above_the_cap() {
    let big = "x\n".repeat(DIFF_MAX_LINES + 1);
    assert!(diff_lines(&big, "x").is_none());
    assert!(diff_lines("x", &big).is_none());
    let at_cap = "x\n".repeat(DIFF_MAX_LINES);
    assert!(diff_lines(&at_cap, &at_cap).is_some());
}

// --- Redaction and the activity filter --------------------------------------------------------

#[test]
fn redact_masks_each_key() {
    let cases = [
        (
            "Authorization: AWS4-HMAC-SHA256 Credential=abc, Signature=def\nnext",
            "Authorization: ***\nnext",
        ),
        ("?X-Amz-Signature=deadbeef&x=1", "?X-Amz-Signature=***&x=1"),
        ("X-Amz-Credential=AKIA/2026/auto", "X-Amz-Credential=***"),
        (
            "x-amz-security-token: tok123 rest",
            "x-amz-security-token: *** rest",
        ),
        ("AccessKey: 1234-abcd", "AccessKey: ***"),
        ("password=hunter2;", "password=***;"),
        ("api?key=sekrit&b=2", "api?key=***&b=2"),
        ("token=\"abc\"", "token=\"***\""),
        ("SECRET=xyz", "SECRET=***"),
    ];
    for (text, want) in cases {
        assert_eq!(redact(text), want, "{text}");
    }
    assert_eq!(redact("nothing to hide"), "nothing to hide");
    assert_eq!(redact("key="), "key=", "an empty value stays empty");
}

fn event(kind: ActivityKind, site: &str, path: &str, ok: bool, detail: &str) -> ActivityEvent {
    ActivityEvent {
        at: Utc.with_ymd_and_hms(2026, 11, 5, 10, 0, 0).unwrap(),
        kind,
        site: site.into(),
        path: path.into(),
        ok,
        detail: detail.into(),
        bytes: 1,
    }
}

#[test]
fn activity_filter_by_kind_failed_site_and_text() {
    let up = event(ActivityKind::Upload, "Main", "blog/a.html", true, "");
    let failed = event(
        ActivityKind::Delete,
        "Backup",
        "x.txt",
        false,
        "Access Denied",
    );
    assert!(ActivityFilter::default().matches(&up));
    let kind = ActivityFilter {
        kind: Some(ActivityKind::Upload),
        ..ActivityFilter::default()
    };
    assert!(kind.matches(&up) && !kind.matches(&failed));
    let failed_only = ActivityFilter {
        failed_only: true,
        ..ActivityFilter::default()
    };
    assert!(!failed_only.matches(&up) && failed_only.matches(&failed));
    let site = ActivityFilter {
        site: Some("Backup".into()),
        ..ActivityFilter::default()
    };
    assert!(!site.matches(&up) && site.matches(&failed));
    let text = |t: &str| ActivityFilter {
        text: t.into(),
        ..ActivityFilter::default()
    };
    assert!(text("BLOG").matches(&up));
    assert!(
        text(" denied ").matches(&failed),
        "detail, trimmed, any case"
    );
    assert!(text("backup").matches(&failed), "site");
    assert!(!text("nowhere").matches(&up));
    assert!(text("   ").matches(&up));
}

// --- Sites file -------------------------------------------------------------------------------

fn bunny_site() -> SiteRecord {
    SiteRecord {
        id: SiteId::new(),
        name: "Main".into(),
        provider: ProviderKind::Bunny,
        container: "main-zone".into(),
        region: "de".into(),
        host: "storage.bunnycdn.com".into(),
        s3: None,
        created_at: Utc::now(),
    }
}

fn profile(site: &SiteId, name: &str) -> ProfileRecord {
    ProfileRecord {
        id: ProfileId::new(),
        site: site.clone(),
        name: name.into(),
        local_dir: PathBuf::from(r"D:\site\public"),
        remote_dir: p("www"),
        skip_rules: SkipRules::defaults(),
        remove_extras: true,
        purge: PurgeMode::Zone,
        guard: DeleteGuard {
            files: 5,
            percent: 50,
        },
        created_at: Utc::now(),
        last_published_at: None,
        cache_control: Some("max-age=60".into()),
        extras: ProfileExtras::default(),
    }
}

#[test]
fn the_sites_file_round_trips_without_secret_fields() {
    let bunny = bunny_site();
    let r2 = SiteRecord {
        provider: ProviderKind::S3,
        s3: Some(S3Site {
            preset: S3Preset::CloudflareR2,
            endpoint: "https://acc.r2.cloudflarestorage.com".into(),
            path_style: true,
        }),
        name: "Media".into(),
        ..bunny_site()
    };
    let extras = SiteExtras {
        read_only: true,
        public_base_url: Some("https://cdn.example/".into()),
        presign_expiry_secs: 600,
    };
    let mine = profile(&bunny.id, "Blog");
    // A profile of another site is never exported under this one.
    let stray = profile(&r2.id, "Stray");
    let file = export_sites(&[
        (bunny.clone(), extras.clone(), vec![mine.clone(), stray]),
        (r2.clone(), SiteExtras::default(), Vec::new()),
    ]);
    assert_eq!(
        (file.format.as_str(), file.version),
        (SITES_FORMAT, SITES_VERSION)
    );
    assert_eq!(file.sites.len(), 2);
    assert_eq!(file.sites[0].profiles.len(), 1);
    assert_eq!(file.sites[0].profiles[0].name, "Blog");
    assert_eq!(file.sites[0].extras, extras);
    assert_eq!(file.sites[1].s3, r2.s3);

    let text = sites_file_text(&file).unwrap();
    let mut names = Vec::new();
    field_names(&serde_json::from_str(&text).unwrap(), &mut names);
    assert!(names.contains(&"skip_rules".to_owned()));
    for name in &names {
        let lower = name.to_ascii_lowercase();
        for word in ["password", "secret", "token", "key"] {
            assert!(!lower.contains(word), "the file has a field {name}");
        }
    }
    assert_eq!(parse_sites_file(&text).unwrap(), file);
}

/// Every field name in `value`, at any depth.
fn field_names(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (name, inner) in map {
                out.push(name.clone());
                field_names(inner, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| field_names(item, out)),
        _ => {}
    }
}

#[test]
fn a_wrong_or_newer_sites_file_is_refused() {
    use httporter_core::copy::{SITES_NEWER_VERSION, SITES_WRONG_FORMAT};
    assert_eq!(
        parse_sites_file("not json").unwrap_err(),
        SITES_WRONG_FORMAT
    );
    assert_eq!(
        parse_sites_file(r#"{"format":"other","version":1}"#).unwrap_err(),
        SITES_WRONG_FORMAT
    );
    assert_eq!(
        parse_sites_file(r#"{"format":"httporter-sites","version":2,"sites":[]}"#).unwrap_err(),
        SITES_NEWER_VERSION
    );
}

// --- Purge URLs and run_purge -----------------------------------------------------------------

fn zone(id: u64, hosts: &[&str]) -> CdnZone {
    CdnZone {
        id,
        name: format!("zone-{id}"),
        hostnames: hosts.iter().map(|h| (*h).to_owned()).collect(),
    }
}

#[test]
fn purge_urls_for_a_file_and_index_pages() {
    let zones = [zone(1, &["cdn.example"])];
    assert_eq!(
        purge_urls(&zones, &[p("css/site.css")]),
        ["https://cdn.example/css/site.css"]
    );
    assert_eq!(
        purge_urls(&zones, &[p("blog/index.html")]),
        [
            "https://cdn.example/blog",
            "https://cdn.example/blog/",
            "https://cdn.example/blog/index.html",
        ]
    );
    assert_eq!(
        purge_urls(&zones, &[p("index.html")]),
        ["https://cdn.example/", "https://cdn.example/index.html"]
    );
    assert!(purge_urls(&zones, &[RemotePath::root()]).is_empty());
}

#[test]
fn purge_urls_encode_spaces_and_non_ascii() {
    let zones = [zone(1, &["cdn.example"])];
    assert_eq!(
        purge_urls(&zones, &[p("a b/ü.html"), p("x~y_z-1.txt")]),
        [
            "https://cdn.example/a%20b/%C3%BC.html",
            "https://cdn.example/x~y_z-1.txt",
        ]
    );
}

#[test]
fn purge_urls_over_two_zones_and_three_hosts_are_deduplicated() {
    let zones = [
        zone(1, &["a.b-cdn.net", "https://www.example.com/"]),
        zone(2, &["http://www.example.com", "  ", "static.example.com"]),
    ];
    let urls = purge_urls(&zones, &[p("a.css"), p("a.css")]);
    assert_eq!(
        urls,
        [
            "https://a.b-cdn.net/a.css",
            "https://static.example.com/a.css",
            "https://www.example.com/a.css",
        ]
    );
}

/// A purger that records what it is asked, failing the URLs and zones it is told to.
#[derive(Default)]
struct Recording {
    zones: Vec<CdnZone>,
    fail: Vec<String>,
    urls: Mutex<Vec<String>>,
    purged_zones: Mutex<Vec<u64>>,
}

#[async_trait]
impl CdnPurger for Recording {
    fn zones(&self) -> &[CdnZone] {
        &self.zones
    }

    async fn purge_url(&self, url: &str) -> Result<(), ProviderError> {
        self.urls.lock().unwrap().push(url.to_owned());
        if self.fail.iter().any(|f| url.ends_with(f.as_str())) {
            return Err(ProviderError::Server {
                status: 500,
                detail: "boom".into(),
            });
        }
        Ok(())
    }

    async fn purge_zone(&self, id: u64) -> Result<(), ProviderError> {
        self.purged_zones.lock().unwrap().push(id);
        if self.fail.contains(&id.to_string()) {
            return Err(ProviderError::Auth {
                hint: httporter_core::AuthHint::Unknown,
            });
        }
        Ok(())
    }
}

#[tokio::test]
async fn run_purge_goes_on_past_a_failure_and_counts() {
    let purger = Recording {
        zones: vec![zone(1, &["cdn.example"])],
        fail: vec!["b.css".into()],
        ..Recording::default()
    };
    let progress = Mutex::new(Vec::new());
    let on = |done: usize, total: usize| progress.lock().unwrap().push((done, total));
    let result = run_purge(
        &purger,
        PurgeMode::Changed,
        &[p("a.css"), p("b.css"), p("c.css")],
        &on,
    )
    .await;
    let PurgeResult::Failed(text) = result else {
        panic!("{result:?}");
    };
    assert!(text.starts_with("1 of 3 failed: "), "{text}");
    assert_eq!(purger.urls.lock().unwrap().len(), 3, "it went on");
    assert_eq!(*progress.lock().unwrap(), [(0, 3), (1, 3), (2, 3), (3, 3)]);

    let ok = Recording {
        zones: vec![zone(1, &["cdn.example"])],
        ..Recording::default()
    };
    assert_eq!(
        run_purge(&ok, PurgeMode::Changed, &[p("a/index.html")], &|_, _| {}).await,
        PurgeResult::Urls(3)
    );
}

#[tokio::test]
async fn run_purge_zone_off_and_no_zones() {
    let two = Recording {
        zones: vec![zone(1, &["a"]), zone(2, &["b"])],
        ..Recording::default()
    };
    assert_eq!(
        run_purge(&two, PurgeMode::Zone, &[], &|_, _| {}).await,
        PurgeResult::Zone
    );
    assert_eq!(*two.purged_zones.lock().unwrap(), [1, 2]);
    assert_eq!(
        run_purge(&two, PurgeMode::Off, &[p("a")], &|_, _| {}).await,
        PurgeResult::Off
    );
    let failing = Recording {
        zones: vec![zone(1, &["a"]), zone(2, &["b"])],
        fail: vec!["2".into()],
        ..Recording::default()
    };
    assert!(matches!(
        run_purge(&failing, PurgeMode::Zone, &[], &|_, _| {}).await,
        PurgeResult::Failed(_)
    ));
    let none = Recording::default();
    assert_eq!(
        run_purge(&none, PurgeMode::Zone, &[p("a")], &|_, _| {}).await,
        PurgeResult::Unavailable(httporter_core::copy::NO_PULL_ZONE.into())
    );
    assert!(matches!(
        run_purge(&none, PurgeMode::Changed, &[p("a")], &|_, _| {}).await,
        PurgeResult::Unavailable(_)
    ));
}

// --- Delete guard -----------------------------------------------------------------------------

#[test]
fn the_delete_guard_trips_above_the_smaller_limit() {
    let guard = DeleteGuard::default();
    assert_eq!((guard.files, guard.percent), (100, 10));
    // 10 % of 2000 is 200: the 100 files limit is the smaller.
    assert!(!guard.trips(100, 2000));
    assert!(guard.trips(101, 2000));
    // 10 % of 500 is 50.
    assert!(!guard.trips(50, 500));
    assert!(guard.trips(51, 500));
    // An empty site: any delete trips, none does not.
    assert!(!guard.trips(0, 0));
    assert!(guard.trips(1, 0));
    assert!(!guard.trips(0, u64::MAX), "no overflow");
    let all = DeleteGuard {
        files: u64::MAX,
        percent: 100,
    };
    assert!(!all.trips(10, 10));
    assert!(all.trips(11, 10));
}

// --- UiState ----------------------------------------------------------------------------------

#[test]
fn ui_state_round_trips_with_the_new_fields() {
    let state = UiState {
        tabs: vec![TabState {
            site: Some(SiteId("s-1".into())),
            remote: Some(p("blog")),
            local: Some(PathBuf::from(r"D:\site")),
        }],
        current: 0,
        sync: true,
        view: ViewMode::Inspector,
        grid: true,
        appearance: 2,
        restore_tabs: false,
        ask_big_preview: false,
        window: Some(WindowPlacement {
            x: -1920,
            y: 10,
            width: 1280,
            height: 800,
            maximized: true,
        }),
        columns: ColumnWidths {
            size: 90,
            modified: 140,
            status: 0,
        },
        tray: false,
        check_updates: true,
        last_update_check: 1_793_000_000,
        renderer: 1,
        gpu_failed: true,
    };
    let text = serde_json::to_string(&state).unwrap();
    assert_eq!(serde_json::from_str::<UiState>(&text).unwrap(), state);
}

#[test]
fn ui_state_from_older_json_takes_the_defaults() {
    // What I4 wrote: no window, columns, tray, update check or renderer yet.
    let old = r#"{"tabs":[{"site":"s-1"}],"current":0,"sync":false,"view":"two_panes",
                  "grid":false,"appearance":1,"restore_tabs":true,"ask_big_preview":true}"#;
    let state: UiState = serde_json::from_str(old).unwrap();
    assert_eq!(state.appearance, 1);
    assert_eq!(state.tabs[0].site, Some(SiteId("s-1".into())));
    assert_eq!(state.tabs[0].remote, None);
    assert_eq!(state.window, None);
    assert_eq!(state.columns, ColumnWidths::default());
    assert!(state.tray, "the tray is on by default");
    assert!(!state.check_updates);
    assert_eq!((state.last_update_check, state.renderer), (0, 0));
    assert!(!state.gpu_failed);
    assert_eq!(
        serde_json::from_str::<UiState>("{}").unwrap(),
        UiState::default()
    );
}
