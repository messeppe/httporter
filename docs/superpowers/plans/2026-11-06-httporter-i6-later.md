# HTTPorter I6 "Later", implementation plan (lean)

> **For agentic workers:** speed mode (see below). Builders work in parallel worktrees; the controller merges.

**Goal:** the last milestone (spec §8.8, §15 I6):
- **CLI** (§8.8.1): `httporter publish|ls|upload|download`, plus a console companion `httporter.com`.
- **Scheduled publish** (§8.8.2): a Windows Task Scheduler task per profile. The profile page's Schedule card shows
  the next run and the last result.
- **Watch folder** (§8.8.3): auto-publish when the local folder changes, debounced 10 s.
- **Zone and bucket tools** (§8.8.4–8.8.5): create Bunny storage zones (+ a linked pull zone) and S3 buckets; Bunny
  404 handling and replication regions; S3 CORS and static website where the preset supports them.

This also covers the checklist rows "Publish on a schedule…", "CLI (`httporter.com`), scheduled publish, watch
folder", and "Create zones / buckets, zone and bucket settings".

**Spec:** `docs/superpowers/specs/2026-10-29-httporter-design.md` §8.8, §8.6, §10.2, §12, B6, B8.

**Architecture:**
- **One implementation of every command, in `httporter.exe`:**
  - `httporter.exe --cli <args…>` runs the command without a window, then exits with its code.
  - `httporter.com` (new crate `httporter-com`, std only, tiny) starts `httporter.exe --cli <args…>` from its own
    folder, with stdin/stdout/stderr inherited. It forwards Ctrl+C, then exits with the child's code. Typing
    `httporter …` in a terminal finds `.com` first (PATHEXT).
- **A headless publish runner** (`publish::headless`) holds compare → plan → guards → run → purge → history. The CLI,
  the scheduler (through the CLI) and the watch folder all use it, and so does nothing else, so the three behave the
  same.
- **The GUI and the CLI can run at once.** A per-profile lock (named mutex `Local\HTTPorter.Profile.<id>`) means one
  publish per profile.
- **Journal rows carry their owner** (pid + process start time). At start the GUI offers to resume only rows whose
  owner is gone.
- **Scheduled tasks run `httporter.exe --cli publish --profile <id> --scheduled`.** This is GUI subsystem, so no
  console window flashes. It's a deviation from §8.8.1 "Task Scheduler entries call `httporter.com`", for that
  reason.
- **Tasks go in the folder `\HTTPorter\`,** made through the Task Scheduler COM API (`ITaskService`), which is
  locale-proof, unlike parsing `schtasks` output.

**Speed mode (user rule: build everything first, test at the end):**
- No per-task tests or reviews.
- **A task is done** when:
  - `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` pass;
  - `cargo test --workspace --no-run` compiles;
  - the app starts (debug `httporter.exe` still running 5 s after `Start-Process`, then `Stop-Process`);
  - the work is committed.
- A task that breaks an existing test on purpose fixes it. All tests run in T10, after the user has used the build.

**Rules (as I5, plus):**
- **Look:**
  - Reuse `Theme`, `CapsuleButton`, `CheckRow`, `AlertCard`, `Modal`, `Segmented`, `form_controls.slint`, and the
    Settings `Section` / `Group` / `GroupRow`.
  - Use the Apple HIG.
  - Every size and font comes from Theme tokens (text scaling), focus rings use `Theme.focus-ring`, and animations
    follow `Theme.motion`.
- **Strings:** `@tr()` in Slint. Rust words only in `copy.rs` files (`lint_tests`). CLI output words live in
  `src/cli/copy.rs`.
- **No side effects on the user's accounts or PC during building:**
  - never create a real Bunny zone, pull zone, S3 bucket or Windows scheduled task;
  - test against wiremock, MockProvider, or a fake `Scheduler`;
  - the real, live checks are T10, on `httporter-test*` only;
  - never `exampleshop`, never pull zone 7654321.
- **Secrets:**
  - Only in Credential Manager.
  - Never in CLI arguments, output, task XML or logs.
  - The CLI reads keys exactly as the GUI does.
- **Exe size:** no budget (user). Release profile `opt-level "s"` + busy crates at 3. Nothing is dropped for size.
- **Builds:**
  - One worktree per builder, `CARGO_BUILD_JOBS=2`.
  - `TMP` / `TEMP` = `D:\Temp\httporter-build`.
  - `CARGO_TARGET_DIR=D:\Temp\httporter-target\<track>`. The controller seeds each from T1's dependencies, without
    `incremental`.
  - D: filled once in I5: delete your own target dir's `debug\incremental` if D: drops below 20 GB.
- **`Cargo.lock` conflict:** take either side, then `cargo metadata --format-version 1 >NUL`. Never `cargo update`.
- **No `panic = "abort"`, no UPX, no `.env`** except `.env.test` via dotenvy in live tests.
- **Agents never drive the user's desktop** beyond the 5 s start check.

## Review focus (T10 tests these; no task test does)
1. **Scheduled publish while the GUI is open on the same profile:** the second one waits for the lock or reports
   "busy". Exit code 1, no interleaved journal, no resume offer for the other's rows.
2. **Delete guard unattended:**
   - A scheduled or watched publish that would trip the guard deletes nothing.
   - It exits 4, or for the watch folder shows "N deletions need your review".
   - Its uploads still run; only the deletes wait.
3. **Watch folder:**
   - A folder renamed, deleted or moved to another drive while watched → watching stops with a clear note, and
     nothing is deleted remotely.
   - An editor's save storm (many writes) → one publish 10 s after the last change.
4. **CLI quoting, from cmd and PowerShell:** names with spaces, `%`, `#`, Unicode, `--profile` by name and by id.
   Ambiguous names are refused with the candidates listed (exit 2).
5. **Wrong or missing credentials in the CLI:** exit 3 with the same clear message as the GUI within 2 s (S9). Never a
   password prompt; a scheduled run never blocks waiting for input.

---

## 0. Shared interfaces (T1 commits these; everyone codes against them)

- **Names and signatures here are fixed.** Ask the controller before changing one.
- **Startup paths are never `todo!()`:**
  - the `--cli` dispatch;
  - `cli::run` returning `2` with "not built yet" until T2;
  - `watch::install`;
  - `schedule::install`;
  - `zones::install`;
  - the store migration.
- Click-only paths may be `todo!("T<n>")`.

### 0.1 `httporter-core` (real in T1)
```rust
// publish.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HistorySource { #[default] App, Cli, Scheduled, Watch }
// HistoryRecord gains `pub source: HistorySource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleRepeat { Hourly { every: u8 /*1..=24*/ }, Daily, Weekly { days: [bool; 7] /*Mon..Sun*/ } }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule { pub enabled: bool, pub repeat: ScheduleRepeat, pub at_minutes: u16 /*local, 0..1440*/,
    pub purge: Option<PurgeMode> /*None = profile's*/ }
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileExtras { pub watch: bool, pub unattended_deletes: bool, pub schedule: Option<Schedule> }
// ProfileRecord gains `pub extras: ProfileExtras` (T1 fixes the literals with Default).
/// What one headless publish should do (CLI flags map onto it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessOptions { pub dry_run: bool, pub yes: bool, pub no_delete: bool,
    pub purge: Option<PurgeMode>, pub source: HistorySource }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessOutcome {
    Done { uploaded: u64, deleted: u64, failed: u64, bytes: u64, purge: PurgeResult },
    DryRun { new: u64, changed: u64, delete: u64, bytes: u64 },
    GuardTripped { would_delete: u64, uploaded: u64 },   // deletes held back
    Busy,                                                // another publish of this profile holds the lock
}
pub mod exit { pub const OK: i32 = 0; pub const FAILURES: i32 = 1; pub const CONFIG: i32 = 2;
    pub const AUTH: i32 = 3; pub const GUARD: i32 = 4; }
```

### 0.2 `httporter-platform` (T1)
```rust
// store.rs, schema v5 (T1 real): journal.owner TEXT NULL; history.source TEXT NOT NULL DEFAULT 'app';
//   profiles.extras TEXT NOT NULL DEFAULT '{}' (JSON ProfileExtras). Existing rows: owner NULL = "an old run, gone".
// journal.rs, SqliteJournal::with_owner(owner: String); unfinished() returns only rows whose owner is NULL or not alive.
pub fn process_owner() -> String;               // "<pid>:<process start FILETIME>"
pub fn owner_alive(owner: &str) -> bool;        // OpenProcess + GetProcessTimes match
// win/lock.rs
pub struct ProfileLock { /* named mutex handle; Drop releases */ }
impl ProfileLock { pub fn try_acquire(profile: &ProfileId) -> Option<ProfileLock>;
                   pub fn acquire_wait(profile: &ProfileId, timeout: Duration) -> Option<ProfileLock>; }
// win/scheduler.rs (T3)
pub trait Scheduler: Send + Sync {
    fn upsert(&self, task: &ScheduledTask) -> Result<(), String>;
    fn remove(&self, name: &str) -> Result<(), String>;
    fn next_run(&self, name: &str) -> Result<Option<DateTime<Local>>, String>;
    fn exists(&self, name: &str) -> Result<bool, String>;
}
pub struct ScheduledTask { pub name: String /*"Publish <profile id>"*/, pub description: String,
    pub exe: PathBuf, pub args: Vec<String>, pub schedule: Schedule }
pub struct TaskSchedulerCom;     // impl Scheduler via ITaskService, folder \HTTPorter\, current user, interactive token
pub struct FakeScheduler { /* in-memory, for tests */ }
```

### 0.3 Providers (T1 signatures; bodies `todo!("T5")`)
```rust
// httporter-bunny/src/account.rs
pub struct NewZone { pub name: String, pub region: String, pub replication: Vec<String>, pub pull_zone: Option<String> }
pub struct ZoneSettings { pub custom_404_path: Option<String>, pub rewrite_404_to_200: bool,
    pub replication: Vec<String> /*region codes*/ }
impl BunnyAccount {
    pub async fn create_zone(&self, z: &NewZone) -> Result<ZoneInfo, ProviderError>; // POST /storagezone (+ POST /pullzone)
    pub async fn zone_settings(&self, zone_id: i64) -> Result<ZoneSettings, ProviderError>;
    pub async fn update_zone_settings(&self, zone_id: i64, s: &ZoneSettings) -> Result<(), ProviderError>; // only adds regions
}
// httporter-s3/src/buckets.rs (new)
pub struct CorsRule { pub origins: Vec<String>, pub methods: Vec<String>, pub headers: Vec<String>, pub max_age: Option<u32> }
pub struct Website { pub index: String, pub error: Option<String> }
impl S3Storage {
    pub async fn create_bucket(&self, name: &str) -> Result<(), ProviderError>;     // LocationConstraint unless us-east-1/auto
    pub async fn cors(&self) -> Result<Vec<CorsRule>, ProviderError>;              // NoSuchCORSConfiguration → empty
    pub async fn set_cors(&self, rules: &[CorsRule]) -> Result<(), ProviderError>;  // empty → DeleteBucketCors
    pub async fn website(&self) -> Result<Option<Website>, ProviderError>;
    pub async fn set_website(&self, w: Option<&Website>) -> Result<(), ProviderError>;
}
// presets.rs: S3Preset gains `pub fn supports_cors(&self) -> bool` and `pub fn supports_website(&self) -> bool` (T5 fills per B8).
```

### 0.4 App (T1 creates files, with stubs)
```rust
// main.rs: if args[1] == "--cli" → std::process::exit(cli::run(args[2..])) before any Slint, single-instance or reset code.
// cli/mod.rs (T2)
pub fn run(args: Vec<OsString>) -> i32;
// publish/headless.rs (T2 implements; T4 uses)
pub async fn run_headless(ctx: &HeadlessCtx, profile: &ProfileRecord, opts: &HeadlessOptions,
    progress: &(dyn Fn(HeadlessProgress) + Send + Sync)) -> Result<HeadlessOutcome, HeadlessError>;
pub struct HeadlessCtx { /* store, creds, factory, engine, journal: built by HeadlessCtx::open() for CLI, from Publisher for GUI */ }
pub enum HeadlessProgress { Comparing { done: u64, total: u64 }, Uploading { done: u64, total: u64, bytes: u64 }, Deleting { done: u64, total: u64 }, Purging }
pub enum HeadlessError { Config(String), Auth(String), Io(String) }   // → exit 2 / 3 / 1
// publish/schedule_view.rs (T3), pub fn install(ui: &AppWindow, p: &Arc<Publisher>);
// publish/watch.rs (T4)         , pub fn install(ui: &AppWindow, p: &Arc<Publisher>);
// manage/zones.rs (T5)          , pub fn install(ui: &AppWindow, m: &Arc<Manager>);
```

### 0.5 Slint (T1 wires; each track fills its component)
- **Profile page Schedule card** (`profile_page.slint`, T3):
  - the link becomes "Publish on a Schedule…";
  - the card shows `schedule-text` ("Daily at 06:00 · next Tue 06:00" / "Off") and `schedule-last` ("Last run: today
    06:00 · 3 uploaded").
- **`ScheduleSheet`** (new `components/schedule_sheet.slint`, T3):
  - an On switch, then the repeat as a Segmented [Hourly, Daily, Weekly];
  - every-N hours, the time field and the weekday chips;
  - the purge choice;
  - the note "Runs while you're signed in to Windows, even if HTTPorter is closed.";
  - Cancel / Save.
- **`ProfileSheet`** (T4) adds:
  - "Publish changes automatically while HTTPorter is open", with the help "Uploads 10 seconds after the folder
    changes.";
  - "Delete without asking when publishing automatically", under it, off by default, with the destructive caption
    "Files removed from the folder are deleted from the site without a review."
- **Sidebar profile rows** (T4): a small "Watching" mark (label + glyph, not colour only).
- **`CreateZoneSheet` / `CreateBucketSheet`** (new `components/zone_sheets.slint`, T5):
  - Bunny: name, region, replication regions, "Also create a pull zone"; reached from Add Site's account-key list
    ("Create New Zone…").
  - S3: name; reached from the bucket picker ("Create Bucket…").
- **Settings › Sites detail** (T5):
  - **"Zone" section** (Bunny with a kept account key): 404 page path, "Return status 200 for the 404 page", and
    replication regions, where they can only be added, which says so.
  - **"Bucket" section** (S3, when the preset supports it): CORS origins and methods (simple editor); static website
    index and error document.

---

## Tasks

### T1, Skeleton (alone first; `CARGO_BUILD_JOBS=3`)
**What it covers:**
- §0 everything;
- the store v5 migration;
- the `--cli` dispatch;
- the `httporter-com` crate: its real relay, since it is tiny (std only: `Command` with inherited stdio,
  `SetConsoleCtrlHandler` → `GenerateConsoleCtrlEvent` is not possible across subsystems, so on Ctrl+C it signals
  the named event `Local\HTTPorter.Cli.Cancel.<child pid>`, which the CLI waits on alongside its work, and the child
  cancels cleanly);
- the Slint interface and wiring;
- `lint_tests` scope (new `cli/copy.rs`);
- `ProfileRecord.extras` round trip.

**Done:** speed-mode done, plus a "T1 as built" note appended to §0.

### T2, CLI + headless publish (worktree `i6-a`)
**Files:** `src/cli/`, `src/publish/headless.rs`, `crates/httporter-com/`, `platform` journal owner/lock bodies,
`tools/package.ps1`, `installer/httporter.iss`.

**Commands** (help text in `cli/copy.rs`; `--help` per command; `--version`):
- `httporter publish --profile <name|id> [--dry-run] [--yes] [--no-delete] [--purge changed|all|none]`.
- `httporter ls <site>[:/path] [--json]`: one line per entry, `size  modified  name/`, sorted; `--json` = array of
  `{name, size, modified, folder}`.
- `httporter upload <local…> <site>:/remote/dir` and `httporter download <site>:/remote/path… <local dir>`:
  - the conflict default comes from the site's remembered choice, otherwise Skip if same / Replace if different;
  - `--overwrite` / `--skip-existing` override.
- `httporter sites` and `httporter profiles`: plain lists, names and ids.

**Behaviour:**
- Site and profile by exact name (case-insensitive) or id. Ambiguous → exit 2 with the candidates.
- **Progress:** goes to stderr only when it's a console (`IsConsole` via `GetConsoleMode`), at most 2 lines a second.
  Results go to stdout.
- **Exit codes:** `exit::*`. Transfer failures → 1 after the run. Auth → 3 at once. Guard → 4 (deletes held back,
  uploads done) unless `--yes`. A pending reset marker → 2, "Start HTTPorter once first."
- **`--scheduled`** (internal, for tasks):
  - waits ≤ 10 min for the profile lock, otherwise logs "busy" and exits 1;
  - history `source = Scheduled`;
  - a Windows notification only on failure, using the existing notify helper.
- **Records:**
  - history rows record `source`;
  - the activity log records CLI runs, with the same `redact`;
  - journal rows carry `process_owner()`.
- **`httporter.com`:**
  - built by `package.ps1` and copied as `httporter.com` beside the exe;
  - the installer gets an unchecked task "Add HTTPorter to PATH (for the `httporter` command)", which uses
    `HKCU\Environment` and sends `WM_SETTINGCHANGE`; uninstall removes it;
  - the zip README explains it.
- **Headless publish:** the same compare/plan/guard/purge code the Publish sheet uses (factor it, don't copy it). The
  GUI's Publish button keeps its sheet.

### T3, Scheduled publish (worktree `i6-b`)
**Files:** `win/scheduler.rs`, `components/schedule_sheet.slint`, the Schedule card in `profile_page.slint`,
`src/publish/schedule_view.rs`.

**`TaskSchedulerCom`:**
- `ITaskService` → folder `\HTTPorter` (created if missing).
- `TASK_LOGON_INTERACTIVE_TOKEN`, so no password is stored.
- Settings: `StartWhenAvailable`, `DisallowStartIfOnBatteries = false`, `StopIfGoingOnBatteries = false`,
  `ExecutionTimeLimit PT4H`, `MultipleInstances IgnoreNew`.
- The trigger comes from `Schedule`: hourly = a daily trigger repeating every N hours; daily; weekly with a days mask.
- Action: the current exe path, plus `--cli publish --profile <id> --scheduled`.

**The app:**
- Saving the sheet stores `ProfileExtras.schedule` and upserts the task. Off → removes the task. Deleting a profile
  removes it too.
- At start, a task whose profile no longer exists is removed (`\HTTPorter\Publish *` only).
- The card shows `next_run` and the last `source = Scheduled` history row.
- If the task is missing but the schedule is on (the user deleted it in Task Scheduler), the card says "The scheduled
  task is missing" and offers "Recreate".

**Tests use `FakeScheduler`. Never create a real task during building.**

### T4, Watch folder (worktree `i6-c`)
**Files:** `src/publish/watch.rs`, the options in `components/profile_sheet.slint` and its Rust, the "Watching" mark in
`components/sidebar.slint` and its view model.

**Watching:**
- One `DirWatcher` per profile with `extras.watch`, started at app start and on toggling it.
- Changes from the root and sub-folders, filtered by skip rules.
- A 10 s debounce from the last change, then `run_headless` with `source = Watch`:
  - `no_delete = !unattended_deletes`;
  - purge as the profile says.
- One run at a time per profile, via the lock. A change during a run queues one more run.

**Results:**
- Uploads land in the transfer drawer like any batch.
- Held-back deletes show on the profile page as "N files were removed from the folder, Review…", which opens the
  normal Publish sheet.
- A toast "Published 3 changed files" after each run, and failures as a warning toast.

**Stops watching, with a note on the profile page and an activity entry:**
- the folder is gone or renamed, or the watcher overflows twice in a row (then one full compare runs first);
- the site is read-only (watch disabled with the reason).

**While HTTPorter runs only.** Closing the window while watching asks nothing new. The tray (I5) keeps it alive only
while transfers run, which is stated in the help text.

### T5, Zone and bucket tools (worktree `i6-d`)
**Files:**
- `httporter-bunny/src/account.rs` (+ model);
- `httporter-s3/src/{buckets.rs,presets.rs,xml.rs}`;
- `components/zone_sheets.slint`;
- the Add Site entry points in `components/add_site.slint` + `src/add_site.rs`;
- the Settings › Sites "Zone" / "Bucket" sections in `components/settings_tabs.slint` + `src/manage/settings_tabs.rs`
  (Sites part only);
- `src/manage/zones.rs`.

**Bunny:**
- Confirm the account-API fields from bunny.net's docs (B6) and cite the URL in a comment: `POST /storagezone`
  (`Name`, `Region`, `ReplicationRegions`), `POST /pullzone` (`Name`, `OriginType: 2`, `StorageZoneId`),
  `POST /storagezone/{id}` (`Custom404FilePath`, `Rewrite404To200`, `ReplicationZones` as documented).
- Name rules (lowercase letters, digits, hyphens; Bunny's length limit) are checked before sending.
- A created zone becomes a site, the way the account-key sign-in adds one. The pull zone links as the site's CDN.

**S3:**
- `CreateBucket` (with a LocationConstraint where required); `Get/Put/DeleteBucketCors`;
  `Get/Put/DeleteBucketWebsite`.
- XML via the crate's existing writer.
- Preset support from each service's docs (B8): R2, B2, Wasabi, Spaces, AWS, MinIO. Cite them. Unsupported → the
  section is hidden, with "{Service} doesn't offer this through its S3 API."

**Tests:** wiremock only. No live API calls while building.

### T7, Merge, package, install (controller)
1. Merge T1, T2, T3, T4, T5, with a compile check after each.
2. `tools/spdx_headers.py` (new files).
3. `cargo test --workspace` (unit tests only).
4. `package.ps1`: zip + setup now include `httporter.com`.
5. Install the exe + `httporter.com` (as today, plus the companion), and a launch smoke.
6. A CLI smoke: `httporter.com --version`, `httporter.com sites`, and `httporter.com profiles`. These are read-only
   and print no secrets.
7. Update the ledger and memory, then delete the track target dirs.

### T10, After the user has used the build (also the open I3–I5 items)
- **Unit and mock tests:** CLI argument parsing and exit codes, `run_headless` on MockProvider (guard, dry-run,
  no-delete, busy lock), journal owner filtering, the store v5 migration, Schedule → trigger XML/COM fields via
  FakeScheduler, the watch debounce, the Bunny / S3 zone APIs on wiremock.
- **Live, on `httporter-test*` only:**
  1. CLI `publish` to the test zone;
  2. create and delete a zone named `httporter-test-<unix>`;
  3. R2 CORS set/get/clear on the test bucket;
  4. one real scheduled task named `\HTTPorter\Publish test-…`, run once and removed.
- **Then:** the S1–S10 measurements and the user's hands-on list, as in the I5 plan T10.

## Not in I6 / deviations
- **Scheduled tasks call `httporter.exe --cli`, not `httporter.com`:** no console flash.
- **Watching runs only while HTTPorter is open:** a background service is out of scope; that's what the schedule is
  for.
- **Replication regions can only be added:** that is Bunny's rule.
- **macOS and Linux builds:** later, as the spec says.
