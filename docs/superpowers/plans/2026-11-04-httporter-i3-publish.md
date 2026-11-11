# HTTPorter I3 "Publish": implementation plan (lean)

**Goal:** every I3 row of `docs/design/mockup-feature-checklist.md` works:
- **Compare:** toolbar toggle, "Comparing" chip, Status column, status-bar summary.
- **Publish:** sheet, sidebar profiles, profile page with stats, history, failed detail and search.
- **CDN purge:** after a publish and from the delete alert.
- **Cleanup:** the Q52+ "→ I3" minors.

**Architecture:**
- Compare and plan code sits in `httporter-transfer`, next to `plan.rs`.
- Uploads and deletes run as normal `Engine` batches; the engine gains per-batch outcomes.
- Purge = core trait `CdnPurger` + a Bunny implementation over the account API.
- Profiles, history and CDN links go in the existing SQLite DB (`%APPDATA%\HTTPorter\httporter.db`, `store.rs`,
  schema v3).
- The app gets a `publish/` module. Each UI task owns its own Slint component file and Rust view file.

**Speed mode:**
- No per-task tests or reviews.
- **Done** = compiles + `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + committed.
- A task that breaks an existing test on purpose (schema-version asserts, the new `FileRow` field) fixes that test.
- All tests run in T10.

**Rules:**
- Reuse the existing components (`Theme`, `CapsuleButton`, `CheckRow`, `AlertCard`, `Modal`, `chips.slint`) and the
  app's Apple-HIG look.
- Slint strings go through `@tr()`; Rust user-facing strings live only in files named `copy.rs` (`lint_tests.rs`
  checks both).
- Secrets go only to Credential Manager (`CredentialKey::account`); never log or show one.
- No `panic = "abort"`.
- Read no `.env` file except `.env.test`, and only via dotenvy in live tests.

---

## 0. Shared interfaces

T1 commits all of this as `todo!()` stubs; everyone codes against it.
- **Names and signatures are exact.** Ask the controller before changing one. Private items and extra fields are fine.
- **Derives:** data types derive `Debug, Clone, PartialEq, Eq` where possible.
- **Serde:** stored enums use serde with `snake_case`.

### 0.1 `httporter-core`: new `publish.rs`, `skip.rs`, `cdn.rs` (re-exported)

```rust
// publish.rs
pub struct ProfileId(pub String);                       // uuid v4, new()/Display/Hash/serde like SiteId
                                                        // T1: new() is real (uuid v4); serde transparent
#[derive(Default)] pub enum PurgeMode { Off, #[default] Changed, Zone }
pub struct DeleteGuard { pub files: u64, pub percent: u8 }  // Default 100 / 10
impl DeleteGuard { pub fn trips(&self, deletes: u64, remote_files: u64) -> bool }  // deletes > min(files, remote*pct/100)
pub struct ProfileRecord { pub id: ProfileId, pub site: SiteId, pub name: String, pub local_dir: PathBuf,
    pub remote_dir: RemotePath, pub skip_rules: Vec<String>, pub remove_extras: bool /*default false*/,
    pub purge: PurgeMode, pub guard: DeleteGuard, pub created_at: DateTime<Utc>, pub last_published_at: Option<DateTime<Utc>> }
pub enum CompareStatus { New, Changed, RemoteOnly, Same }          // Copy, Hash
pub enum PublishResult { Ok, Partial, Failed, Cancelled }
pub enum FileAction { Upload, Delete }
pub enum FileOutcome { Done, Failed(String), Cancelled, NotRun(String) }
pub struct HistoryFile { pub path: RemotePath, pub action: FileAction, pub outcome: FileOutcome, pub size: u64 }
pub enum PurgeResult { Off, Urls(u64), Zone, Failed(String), Unavailable(String) }
pub struct HistoryRecord { pub id: i64 /*0 until stored*/, pub profile: ProfileId, pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>, pub result: PublishResult, pub uploaded: u64, pub deleted: u64, pub skipped: u64,
    pub failed: u64, pub bytes: u64, pub purge: PurgeResult, pub verified: bool,
    pub remote_files: u64, pub remote_folders: u64, pub regions: u32,   // "On Bunny now" card
    pub files: Vec<HistoryFile> }                                       // empty from list_history
pub const HISTORY_KEEP: usize = 200;

// skip.rs: relative '/' paths, case-insensitive. "x/" = folder x anywhere (and all below); no '/' = last segment;
// inner '/' = whole relative path; '*' = run without '/'; '?' = one char.
pub const DEFAULT_SKIP_RULES: &[&str] = &[".env", ".env.*", "*.key", "*.pem", ".git/", ".svn/", "node_modules/",
    "Thumbs.db", "desktop.ini", ".DS_Store", "*.tmp", "~$*"];         // spec 8.6.11 (12 rules)
#[derive(Default)] pub struct SkipRules { /* compiled */ }   // T1: `SkipRules {}`, T2 adds private fields
impl SkipRules { pub fn new(patterns: &[String]) -> Self; pub fn defaults() -> Vec<String>;
                 pub fn is_skipped(&self, rel: &str, is_folder: bool) -> bool; }
pub fn looks_secret(rel: &str) -> bool;   // .env*, *.pem, *.key, id_rsa*, *.p12, wp-config.php (8.6.12)

// cdn.rs
pub struct CdnZone { pub id: u64, pub name: String, pub hostnames: Vec<String> }        // no scheme; serde
pub struct CdnLink { pub account: AccountId, pub zones: Vec<CdnZone>, pub checked_at: DateTime<Utc> }  // serde
// T1: for that, AccountId gained Serialize/Deserialize (transparent) and core's chrono the `serde` feature.
#[async_trait] pub trait CdnPurger: Send + Sync {
    fn zones(&self) -> &[CdnZone];
    async fn purge_url(&self, url: &str) -> Result<(), ProviderError>;   // rate-limited inside
    async fn purge_zone(&self, id: u64) -> Result<(), ProviderError>; }
pub const PURGE_URL_LIMIT: usize = 300;
/// https://{host}/{enc path} for every host; ".../index.html" also ".../" and "..."; root "index.html" also "/".
/// Deduped, sorted.
pub fn purge_urls(zones: &[CdnZone], paths: &[RemotePath]) -> Vec<String>;
/// Off→Off. Changed→every purge_urls URL, continuing past failures (Failed("n of m failed: …")).
/// Zone→purge_zone per zone (no zones→Unavailable).
pub async fn run_purge(p: &dyn CdnPurger, mode: PurgeMode, paths: &[RemotePath],
                       on_progress: &(dyn Fn(usize, usize) + Send + Sync)) -> PurgeResult;
```

### 0.2 `httporter-transfer`: new `compare.rs`, `publish.rs`; `engine.rs` additions

```rust
// compare.rs
#[derive(Copy)] pub struct CompareMode { pub sha256: bool, pub md5: bool, pub mtime_fallback: bool }
impl CompareMode { /// Bunny {verify,false,false} (size only when verify off); S3 {false,true,true}.
    pub fn for_site(kind: ProviderKind, verify_checksums: bool) -> Self; }
#[derive(Default)] pub struct HashCache { /* Mutex<HashMap<(PathBuf,u64,SystemTime), FileHashes>>, memory only */ }
// T1: HashCache derives Debug + Default only (Mutex); its field is `hashes`.
impl HashCache { pub fn get_or_hash(&self, path: &Path) -> io::Result<FileHashes> }
#[derive(Copy, Default)] pub struct ScanProgress { pub local_files: u64, pub remote_files: u64, pub hashed: u64, pub to_hash: u64 }
pub struct CompareItem { pub rel: String, pub status: CompareStatus, pub local_size: Option<u64>,
    pub remote_size: Option<u64>, pub by: CompareBy, pub by_date: bool }
#[derive(Copy, Default)] pub struct CompareCounts { pub changed: u64, pub new: u64, pub remote_only: u64, pub same: u64, pub upload_bytes: u64 }
pub struct CompareResult { pub local_root: PathBuf, pub remote_root: RemotePath, pub items: Vec<CompareItem> /*files, by rel*/,
    pub remote_folders: u64, pub skipped_local: u64, pub secrets: Vec<String>, pub regions: u32 }
impl CompareResult {
    pub fn counts(&self) -> CompareCounts;
    /// Direct children of rel_dir ("" = root). Folder: Changed if anything below differs, New if only local,
    /// RemoteOnly if only remote, else Same.
    pub fn child_statuses(&self, rel_dir: &str) -> HashMap<String, CompareStatus>;
    pub fn weakest_method(&self) -> CompareBy; }                       // "Compared N files by SHA-256 checksum"
pub enum CompareError { Local { path: PathBuf, message: String }, Remote(ProviderError) }  // T1: Debug, Clone, PartialEq (no Eq)
/// Local walk ‖ remote walk(); skip rules both sides; hashes only equal-size pairs the mode can check.
/// Missing remote root = empty. Cancel = drop.
pub async fn compare_trees(provider: &dyn StorageProvider, local_root: &Path, remote_root: &RemotePath, skip: &SkipRules,
    mode: CompareMode, hashes: &HashCache, progress: &(dyn Fn(ScanProgress) + Send + Sync)) -> Result<CompareResult, CompareError>;

// publish.rs
pub struct PlanItem { pub rel: String, pub status: CompareStatus, pub size: u64, pub include: bool }
pub struct PublishPlan { pub local_root: PathBuf, pub remote_root: RemotePath, pub items: Vec<PlanItem> /*no Same*/,
    pub same: u64, pub remote_files: u64, pub remote_folders: u64, pub regions: u32 }
impl PublishPlan {
    pub fn from_compare(r: &CompareResult, remove_extras: bool) -> Self;  // Changed, New, RemoteOnly; then rel
    pub fn set_remove_extras(&mut self, on: bool);                         // RemoteOnly.include = on
    pub fn uploads(&self) -> Vec<Job>;      pub fn deletes(&self) -> Vec<Job>;   // one file job per delete (Q54)
    pub fn upload_totals(&self) -> (u64, u64);   pub fn delete_count(&self) -> u64;
    pub fn touched_paths(&self) -> Vec<RemotePath>;                        // included items, full remote paths
    pub fn export_text(&self, title: &str, destination: &str) -> String;
    pub fn retry_of(local_root: PathBuf, remote_root: RemotePath, failed: &[HistoryFile]) -> Self; }
pub struct BatchTarget { pub site: SiteId, pub site_name: String, pub host: String, pub provider: Arc<dyn StorageProvider> }
// T1: BatchTarget derives Clone only (dyn provider).
pub enum PublishPhase { Uploading, Deleting, Purging { done: usize, total: usize }, Finished }
pub struct PublishOutcome { pub result: PublishResult, pub files: Vec<HistoryFile>, pub uploaded: u64, pub deleted: u64,
    pub failed: u64, pub bytes: u64, pub purge: PurgeResult, pub started_at: DateTime<Utc>, pub finished_at: DateTime<Utc> }
impl PublishOutcome { pub fn into_record(self, profile: ProfileId, plan: &PublishPlan, verified: bool) -> HistoryRecord }
/// uploads (submit_tracked + wait_batch) → deletes, only if no upload failed (else NotRun("Not deleted because uploads
/// failed.")) → run_purge on the done paths. wait_batch includes the Q55 late-delete checks, so the purge never starts
/// before them. A purge failure never fails the publish.
pub async fn run_publish(engine: &Engine, target: BatchTarget, plan: &PublishPlan, purge: PurgeMode,
    purger: Option<Arc<dyn CdnPurger>>, on_phase: &(dyn Fn(PublishPhase) + Send + Sync)) -> Result<PublishOutcome, JournalError>;

// engine.rs
#[derive(Default)] pub struct BatchOutcome { pub done: Vec<RemotePath>, pub failed: Vec<(RemotePath, String)>,
                                             pub cancelled: Vec<RemotePath>, pub bytes_done: u64 }
impl Engine {
    pub fn submit_tracked(&self, batch: Batch) -> Result<String, JournalError>;  // keeps per-job outcomes for wait_batch
    /// Resolves when every job is done/failed/cancelled AND pending_checks_for(site) == 0; a check that re-sends a
    /// file reopens the batch; a stopped engine reports the rest as cancelled.
    pub async fn wait_batch(&self, batch_id: &str) -> BatchOutcome;
    pub fn pending_checks_for(&self, site: &SiteId) -> usize; }
```

### 0.3 `httporter-bunny`: new `cdn.rs`

```rust
impl BunnyAccount {
    /// Pull zones (OriginType 2) whose StorageZoneId is the zone named storage_zone (via list_zones); NotFound if none.
    pub async fn cdn_zones_for(&self, storage_zone: &str) -> Result<Vec<CdnZone>, ProviderError>;
    pub async fn purge_url(&self, url: &str) -> Result<(), ProviderError>;      // POST /purge?url=<enc>&async=false
    pub async fn purge_pull_zone(&self, id: u64) -> Result<(), ProviderError>; } // POST /pullzone/{id}/purgeCache
pub struct BunnyCdn { /* account, zones, token buckets: URLs 120 burst + 5/s, zones 20 burst + 0.5/s */ }
impl BunnyCdn { pub fn new(account: BunnyAccount, zones: Vec<CdnZone>) -> Self }  // T1: real (stores both); T5 adds buckets
impl CdnPurger for BunnyCdn {}   // 429: wait Retry-After (clamped), at most 3 tries
```

### 0.4 Storage: `httporter-platform/src/store.rs` (schema v3) and `httporter-app/src/session/profiles.rs`

```rust
impl Store {   // all return Result<_, StoreError> (T1: so list_profiles → Result<Vec<_>>, cdn_link → Result<Option<_>>)
    pub fn add_profile(&self, p: &ProfileRecord);   pub fn update_profile(&self, p: &ProfileRecord);
    pub fn remove_profile(&self, id: &ProfileId) /*+ its history*/;   pub fn list_profiles(&self) -> Vec<ProfileRecord>;
    pub fn add_history(&self, h: &HistoryRecord) -> i64;   // + files, sets last_published_at, trims to HISTORY_KEEP
    pub fn update_history_purge(&self, id: i64, purge: &PurgeResult);
    pub fn list_history(&self, profile: &ProfileId, search: &str) -> Vec<HistoryRecord>; // newest first, no files;
                                                         // search: LIKE on result, date text, any file path
    pub fn history_files(&self, id: i64) -> Vec<HistoryFile>;
    pub fn cdn_link(&self, site: &SiteId) -> Option<CdnLink>;   pub fn put_cdn_link(&self, site: &SiteId, link: &CdnLink);
    pub fn remove_cdn_links_for(&self, account: &AccountId); }
// remove_site() also deletes the site's profiles, their history and its cdn link (one transaction).
impl Session {   // async wrappers on blocking threads (pattern: session/store.rs); errors as String
    pub async fn profiles(&self) -> Vec<ProfileRecord>;
    pub async fn save_profile(&self, p: ProfileRecord) -> Result<(), String>;      // add or update
    pub async fn remove_profile(&self, id: ProfileId) -> Result<(), String>;
    pub async fn add_history(&self, h: HistoryRecord) -> Result<i64, String>;
    pub async fn set_history_purge(&self, id: i64, purge: PurgeResult) -> Result<(), String>;
    pub async fn history(&self, profile: ProfileId, search: String) -> Vec<HistoryRecord>;
    pub async fn history_files(&self, id: i64) -> Vec<HistoryFile>;
    pub async fn cdn_link(&self, site: SiteId) -> Option<CdnLink>;
    /// With key: creds.set(CredentialKey::account) + a Bunny AccountRecord.
    pub async fn save_cdn_link(&self, site: SiteId, link: CdnLink, key: Option<String>) -> Result<(), String>;
    pub async fn account_key(&self, account: AccountId) -> Option<String>;
    pub async fn bunny_accounts(&self) -> Vec<AccountRecord>;
    pub async fn forget_cdn_key(&self, account: AccountId) -> Result<(), String>; }  // cred + record + links
```

```sql
-- v3: append to MIGRATIONS (never edit v1/v2). skip_rules, zones, purge = JSON; dates RFC 3339.
CREATE TABLE profiles (id TEXT PRIMARY KEY, site TEXT NOT NULL, name TEXT NOT NULL, local_dir TEXT NOT NULL,
  remote_dir TEXT NOT NULL, skip_rules TEXT NOT NULL, remove_extras INTEGER NOT NULL, purge TEXT NOT NULL,
  guard_files INTEGER NOT NULL, guard_percent INTEGER NOT NULL, created_at TEXT NOT NULL, last_published_at TEXT);
CREATE TABLE publish_history (id INTEGER PRIMARY KEY AUTOINCREMENT, profile TEXT NOT NULL, started_at TEXT NOT NULL,
  finished_at TEXT NOT NULL, result TEXT NOT NULL, uploaded INTEGER NOT NULL, deleted INTEGER NOT NULL,
  skipped INTEGER NOT NULL, failed INTEGER NOT NULL, bytes INTEGER NOT NULL, purge TEXT NOT NULL, verified INTEGER NOT NULL,
  remote_files INTEGER NOT NULL, remote_folders INTEGER NOT NULL, regions INTEGER NOT NULL);
CREATE INDEX publish_history_profile ON publish_history (profile, id);
CREATE TABLE publish_files (history INTEGER NOT NULL, path TEXT NOT NULL, action TEXT NOT NULL, outcome TEXT NOT NULL,
  error TEXT, size INTEGER NOT NULL);
CREATE INDEX publish_files_history ON publish_files (history);
CREATE TABLE cdn_links (site TEXT PRIMARY KEY, account TEXT NOT NULL, zones TEXT NOT NULL, checked_at TEXT NOT NULL);
```

### 0.5 App: `crates/httporter-app/src/publish/`

```rust
// mod.rs (T1 stub, T7 fills)
pub enum PurgerAnswer { Ready(Arc<dyn CdnPurger>), NeedKey, Unavailable(String) }   // S3 → Unavailable
pub struct Publisher { /* session, transfers, link, HashCache, state */ }
// T1 fields (private; child modules can read them): session: Arc<Session>, transfers: Arc<Transfers>, link: Link,
//   hashes: HashCache, state: Mutex<State> (mod.rs, T7), compare: Mutex<compare_view::CompareState> (T6),
//   sheet: Mutex<sheet_view::SheetState> (T7), profile: Mutex<profile_view::ProfileState> (T8): each view's state
//   struct is in its own file, so add fields there.
impl Publisher {
    pub fn new(session: Arc<Session>, transfers: Arc<Transfers>, link: Link) -> Arc<Self>;
    pub fn open_sheet(self: &Arc<Self>, profile: ProfileId);   // compare, then show the sheet
    pub fn publish_current(self: &Arc<Self>);                  // toolbar Publish + Ctrl+P
    pub fn retry_failed(self: &Arc<Self>, history: i64);   pub fn retry_purge(self: &Arc<Self>, history: i64);
    pub async fn purger_for(&self, site: &SiteRecord) -> PurgerAnswer;
    pub fn ask_cdn_key(self: &Arc<Self>, site: SiteRecord); }  // CdnKeySheet; Edit Profile uses it too
pub fn install(ui: &AppWindow, p: &Arc<Publisher>);          // calls the three installs below
// T1: the installs (compare_view, sheet_view, profile_view) run at startup from install_app: never todo!() there.
//   cdn.rs has no install; sheet_view::install wires the CDN key sheet and the delete-alert purge too.
// compare_view.rs (T6): install(ui, p).  sheet_view.rs + cdn.rs (T7): install(ui, p).
// profile_view.rs (T8): install(ui, p); pub fn open_new(ui, site: &SiteRecord, local: PathBuf, remote: RemotePath).
// copy.rs: sections "// ---- compare (T6) ----", "// ---- sheet (T7) ----", "// ---- profile (T8) ----",
//   each followed by 5 blank lines so parallel inserts merge.
//   T1: rustfmt collapses blank lines, so each header is followed by 5 `//` lines instead. Add right under yours.
// transfers.rs (real bodies in T1): pub fn engine(&self) -> &Engine;
//   pub async fn batch_target(&self, site: &SiteRecord) -> Option<BatchTarget>
//   T1: batch_target uses target() when the cloud pane has that site open, else session.connect(site); None if no
//   client. binding.rs: install_with also returns the Link, which install_app hands to Publisher::new.
```

### 0.6 Slint: `ui/types.slint` additions and `app.slint` hooks (T1)

```slint
export enum CompareStatus { none, changed, new, remote-only, same }   // FileRow gains `status: CompareStatus`
export struct CompareSummary { active: bool, busy: bool, progress: string, changed: int, new: int, remote-only: int,
    remote-label: string, error: string }
export struct ProfileItem { id: string, name: string, meta: string }
export enum PlanGroup { changed, new, kept, deleted }
export struct PlanRow { path: string, kind: FileKind, group: PlanGroup, label: string, size: string, checked: bool, header: bool }
export struct PublishSheetData { title: string, subtitle: string, folder: string, destination: string,
    destination-note: string, busy: bool, progress: string, error: string, changed: int, new: int, remote-only: int,
    same: int, upload-text: string, remove-extras: bool, remove-label: string, purge: int /*0 off,1 changed,2 zone*/,
    purge-enabled: bool, purge-note: string, warning: string, button-label: string, can-publish: bool,
    needs-review: bool, review-name: string }
export struct HistoryRow { id: int, when: string, result: string, failed: bool, uploaded: string, deleted: string,
    size: string, time: string, purge: string }
export struct HistoryDetail { id: int, summary: string, paths: [string], total: int, failed: int, purge-failed: bool, all-shown: bool }
export struct ProfileView { id: string, name: string, folder: string, destination: string, chips: [string],
    last-title: string, last-detail: string, remote-title: string, remote-label: string, remote-detail: string,
    schedule: string, footer: string, status: string }
export struct ProfileDraft { id: string, name: string, site-name: string, folder: string, remote-dir: string,
    skip-rules: string, remove-extras: bool, purge: int, guard-files: string, guard-percent: string, cdn-note: string, error: string }
```

`AppWindow` properties and callbacks:
- **Compare:** `comparing`, `can-compare`, `compare`, `toggle-compare()`.
- **Sidebar:** `profiles`, `selected-profile` (in-out, -1), `profile-selected(int)`.
- **Publish sheet:** `publish-open` (in-out), `publish-sheet`, `plan-rows`, `plan-toggle(int)`,
  `publish-remove-extras(bool)`, `publish-purge(int)`, `publish-change-folder()`, `publish-export()`,
  `publish-cancel()`, `publish-confirm()`, `review-open` (in-out), `publish-review-confirm(string)`.
- **Profile page:** `profile`, `history-rows`, `history-detail`, `history-search` (in-out),
  `history-search-changed(string)`, `history-selected(int)`, `history-retry(int)`, `history-show-all(int)`,
  `history-retry-purge(int)`, `profile-change-folder()`, `edit-profile()`, `profile-publish()`.
- **Profile sheet:** `profile-sheet-open` (in-out), `profile-draft`, `profile-save(ProfileDraft)`,
  `profile-delete(string)`, `profile-pick-folder()`, `profile-add-key()`.
- **CDN key sheet:** `cdn-key-open` (in-out), `cdn-key-zone`, `cdn-key-error` (in-out), `cdn-key-busy`,
  `cdn-key-save(string)`, `cdn-key-cancel()`.
- **Delete alert:** `delete-purge` (in-out), `delete-purge-available`, `delete-purge-note`.
- **T1 additions:** `can-publish` + `publish()` (the toolbar's Publish and Ctrl+P → `publish_current`; §0.6 had no
  hook for them). Ctrl+P is already in `app.slint`'s key handler. `comparing` is in-out.

T1 also creates empty shells, already placed in `app.slint`, so later tasks edit only their own file:
- `components/publish_sheet.slint`: `PublishSheet`, `ReviewDeletionsSheet`, `CdnKeySheet`.
- `components/profile_page.slint`: `ProfilePage`, shown in place of the panes while `selected-profile >= 0`.
- `components/profile_sheet.slint`: `ProfileSheet`.

**T1 as built** (nobody edits `app.slint` after T1, so every hook is wired there already):
- **Hooks declared on existing components** (declarations only; the owner builds them):
  - Toolbar (T6): `can-compare`, `comparing`, `profile-page`, `toggle-compare()`, `edit-profile()`. On the profile
    page `app.slint` already sets the title to the profile name and the subtitle to "Publish profile".
  - Pane (T6): `comparing`, `compare: CompareSummary`. StatusBar (T6): `compare`.
  - Sidebar (T7): `profiles`, `selected-profile`, `profile-selected(int)`.
  - DeleteAlert (T7): `purge` (in-out, `<=> delete-purge`), `purge-available`, `purge-note`.
- **Shells:**
  - `PublishSheet`: `data`, `rows`, `toggle(int)`, `remove-extras(bool)`, `purge(int)`, `change-folder()`,
    `export-plan()` (not `export`, a Slint keyword), `cancelled()`, `confirmed()`.
  - `ReviewDeletionsSheet`: `data`, `rows`, `confirmed(string)`, `cancelled()`.
  - `CdnKeySheet`: `zone`, `error` (in-out), `busy`, `save(string)`, `cancelled()`.
  - `ProfilePage`: `profile`, `history-rows`, `history-detail`, `search` (in-out), `search-changed(string)`,
    `history-selected(int)`, `retry(int)`, `show-all(int)`, `retry-purge(int)`, `change-folder()`, `edit-profile()`,
    `publish()`.
  - `ProfileSheet`: `draft`, `save(ProfileDraft)`, `delete-profile(string)`, `pick-folder()`, `add-key()`,
    `cancelled()`.
- **Who closes what** (`app.slint`):
  - Publish sheet: Cancel closes it, then `publish-cancel`. Its button opens Review Deletions when
    `publish-sheet.needs-review`; otherwise it closes the sheet, then `publish-confirm`.
  - Review Deletions: confirm closes both sheets, then `publish-review-confirm(name)`. Cancel closes only the review.
  - Profile sheet: Cancel closes it. Save and Delete leave closing to Rust.
  - CDN key sheet: Cancel closes it, then `cdn-key-cancel`. Save leaves closing to Rust.
- **Profile page:** shown inside `panes` while `selected-profile` is a valid row; the two panes get `visible: false`
  and keep their state; the banners and the drawer stay. Clicking a site sets `selected-profile = -1` first.
- **Name clash:** Slint generates `crate::CompareStatus` (`None`, `Changed`, `New`, `RemoteOnly`, `Same`); alias it or
  `httporter_core::CompareStatus` in app code. `FileRow.status` defaults to `None`.
- **Lint allows:** stub files carry `#![allow(unused_variables)]` (app `publish/` and `session/profiles.rs` also
  `dead_code`); the `engine.rs`, `store.rs` and `transfers.rs` stubs carry item-level allows. Drop them as you fill.
- **Line endings:** app `transfers.rs` is stored with CRLF; `cargo fmt` rewrites it to LF. Put CRLF back before
  committing (T7, T9), or the diff becomes the whole file.

---

## Tracks

| Track | Tasks | Owns |
|---|---|---|
| -- | **T1** skeleton (blocks all, ~20 min) | every stub in §0 |
| A engine | T2 → T3 → T9 | core skip/cdn; transfer compare, publish, engine, lane, offline; s3 multipart meter |
| B data + CDN | T4 ‖ T5 | platform `store.rs` + app `session/profiles.rs` ‖ bunny `cdn.rs`, `account.rs` |
| C UI | T6 ‖ T7 ‖ T8 | each task's own Slint and Rust view files |
| -- | **T10** tests + gate + live smoke | after every branch is merged |

**After T1, T2, T4, T5, T6, T7 and T8 start together.**
- T3 waits for T2, because they share files.
- T9 waits for T3, because both change `engine.rs`.
- T7 and T9 both touch `transfers.rs`, in different functions.
- The UI tasks compile against the stubs. They work end to end once tracks A and B are merged; merge each branch as it
  lands.

---

### T1: Interfaces skeleton
**Files:**
- **Crates:** core `publish/skip/cdn.rs`; transfer `compare/publish.rs` and the `engine.rs` stubs; bunny `cdn.rs`;
  platform `store.rs` stubs.
- **App, new files:** `session/profiles.rs`, `publish/{mod,compare_view,sheet_view,cdn,profile_view,copy}.rs`.
- **App, edits:**
  - `binding.rs`: `install_app` builds the `Publisher` and calls `publish::install`;
  - `transfers.rs`: `engine()` and `batch_target()`;
  - `viewmodel.rs`: `status: none`;
  - Slint: `types.slint`, `app.slint`, and the 3 shell files.

**Build:** §0 exactly, with bodies `todo!("T<n>")`.
- **Real bodies** only for the trivial accessors, `Default`, the constants, and `engine()`/`batch_target()` (reuse
  `target()`).
- **Slint shells:** their properties plus an empty `Rectangle`, hidden behind their open flags.

**Done:** criteria above; existing tests still compile; report the commit hash.

### T2: Compare, skip rules, plan (Track A)
**Files:** core `skip.rs`, `publish.rs` (`trips`, `ProfileId::new`); transfer `compare.rs`, `publish.rs` (all but
`run_publish` / `into_record`).

**Build:**
- **Glob matcher:** written by hand, no new crate (exe size budget). Plus `looks_secret`.
- **Local walk:** on a blocking thread, with `extended()` long paths. Prune skipped folders. Names that aren't Unicode
  count in `skipped_local`. Collect `secrets`.
- **Remote walk:** `httporter_core::walk`. NotFound means empty. Drop skipped entries and everything under a skipped
  folder. `regions` is the largest `replicated_regions.len()`.
- **Status:**
  - Sizes differ → Changed.
  - Sizes equal, checked in this order:
    - SHA-256, if the mode allows it and the remote has one;
    - MD5, if the remote checksum is `Md5`;
    - with `mtime_fallback`: Same iff `remote.modified >= local.modified` (seconds; sets `by_date`);
    - otherwise Same.
- **Hashing:** through `HashCache`, 8 at a time (the `HASH_AT_ONCE`/`blocking` pattern of `plan.rs`). Report progress
  at most every 100 ms.
- **`PublishPlan`:**
  - `uploads` → `Job::Upload { local: root.join(rel), remote: remote_root.join(rel), size }`.
  - `deletes` → `Job::Delete { remote, folder: false }`.
  - `export_text`: a header (title, destination, date, counts), then `UPLOAD changed|new` / `DELETE` / `KEEP` lines
    with path and size.
  - `retry_of`: Upload rows → New; Delete rows → RemoteOnly with include on.

### T3: Engine batch outcomes, publish runner, purge URLs (Track A, after T2)
**Files:** transfer `engine.rs` (+ `lane.rs` if needed), `publish.rs` (`run_publish`, `into_record`); core `cdn.rs`.

**Build:**
- **`submit_tracked`:** flags the batch, so `batch_changed` keeps its per-job results until `wait_batch` takes them.
- **`wait_batch`:** loops on `changes()` until the batch is closed and `pending_checks_for(site) == 0`. A failed job
  counts as closed.
- **`pending_checks_for`:** counts the `checks` that belong to that site.
- **`run_publish`:** per §0.2. Result: Cancelled if any job was cancelled; else Ok if nothing failed; else Partial if
  something was done; else Failed.
- **`purge_urls` encoding:** percent-encode each segment; RFC 3986 unreserved characters and `/` stay
  (`a b/ü.html` → `a%20b/%C3%BC.html`). Paths are full remote paths, because the pull zone serves the zone root.
- **`purge_urls` forms:** `x/index.html` → `/x/index.html`, `/x/`, `/x`; the root `index.html` → `/index.html`, `/`.

### T4: Store v3 + session wrappers (Track B)
**Files:** platform `store.rs`; app `session/profiles.rs`.

**Build:**
- **Schema:** v3 and every §0.4 method. Trim history in the same transaction; `remove_site` cascades. Change the
  `user_version == 2` asserts to `MIGRATIONS.len()`.
- **Wrappers:** the `session/store.rs` pattern.
- **`save_cdn_link` with a key:** `creds.set(CredentialKey::account(id))` + `add_account(Bunny)`, with the name taken
  from copy.
- **`forget_cdn_key`:** `creds.delete` → `remove_account` → `remove_cdn_links_for`.

### T5: Bunny CDN client (Track B)
**Files:** bunny `cdn.rs`, `account.rs` (reuse `get`, add a `post` helper).

**Build:**
- **`cdn_zones_for`:** `list_zones()`, an exact name match (Bunny is case-sensitive), each `PullZoneInfo` → `CdnZone`.
- **Requests:** `purgeCache` gets an empty JSON body.
- **Errors:** 401/403 → Auth; 429 → RateLimited(Retry-After); 404 → NotFound.
- **Limits:** two token buckets on tokio time.
- **Secrets:** no key in any log or error text.

### T6: Compare in the main window (Track C)
**Files:**
- **Slint:** `pane.slint`, `toolbar.slint` (T6 owns every toolbar change), `statusbar.slint`, `chips.slint`,
  `icons.slint` (`i-compare`).
- **Rust:** `publish/compare_view.rs`, the compare section of `publish/copy.rs`, and only the row-building spots in
  `binding.rs`/`viewmodel.rs`.

**Build:**
- **Toolbar** (`Main.dc.html` l.72–80):
  - Compare toggle: enabled when the cloud pane shows a site folder and This PC shows a folder.
  - Publish: enabled when a site is open or a profile is selected.
  - On the profile page: title = the profile name, subtitle "Publish profile", plus an Edit Profile button.
- **Compare on:**
  - Roots: the two pane folders. Skip rules: those of a profile with the same roots, else the defaults.
  - Runs `compare_trees` with `CompareMode::for_site(kind, verify_checksums)` and the Publisher's `HashCache`.
  - Turning it off or changing site cancels it.
  - Rows inside both roots at the same relative path get `child_statuses`.
  - Compare again after a batch on that site finishes; apply the statuses again after every listing.
- **Pane:**
  - A Status column between Name and Size, shown only while comparing.
  - Chips: Changed / New / "Only on Bunny" (S3: "Only on {site}") / Same.
  - A "Comparing" accent chip in the This PC header.
- **Status bar:** "34 changed · 2 new · 1 only on Bunny", the progress while busy, and errors.

### T7: Publish sheet, runs, CDN key, delete-alert purge, sidebar (Track C)
**Files:**
- **Slint:** `publish_sheet.slint`, `sidebar.slint`, `sheets.slint` (DeleteAlert only).
- **Rust:**
  - `publish/{mod,sheet_view,cdn}.rs` and the sheet section of `publish/copy.rs`;
  - `transfers.rs`, the delete alert / `delete_confirm` only;
  - `win/pickers.rs`: add `pick_save_file(title, default_name, filters)` (rfd);
  - Ctrl+P in the existing shortcut handler.

**Build:**
- **`open_sheet`:** profile → site → `compare_trees` → `from_compare`. Shows progress ("Comparing… 4,210 of 13,793
  files"); Cancel stops it.
- **`publish_current`:** the selected profile; else the open site's last-published profile; else
  `profile_view::open_new` with the pane folders.
- **Sheet** (`Main.dc.html` l.277–333):
  - **Header:** title "Publish to {site}". Subtitle "Compared N files by SHA-256 checksum | MD5 | size | size and
    date. Nothing is uploaded or deleted until you click Publish."
  - **Folder row:** Change Folder… saves the profile and compares again.
  - **Destination row:** the endpoint, with "Destination · HTTPS · {simultaneous} parallel uploads".
  - **Summary:** count chips and "Upload N files · X MB".
  - **Plan list:** virtualized. Group headers have a group checkbox and items have their own. RemoteOnly items read
    "Will be kept" (unchecked) or "Will be deleted" (checked, red).
  - **Checkbox:** "Remove files from {Bunny|site} that aren't in the folder".
  - **Purge CDN:** segmented Off / Changed files / Whole zone.
    - S3: disabled, with "Purging needs a Bunny pull zone."
    - Default: the profile's mode. When `purge_urls(zones, touched).len() > PURGE_URL_LIMIT`, it becomes Whole zone,
      with the note "{n} URLs to purge, purging the whole zone is faster." With no link yet, estimate 1 host × 3
      forms.
  - **Warning:** a line about files that look like secrets.
  - **Footer:** Export Plan… (`pick_save_file` + `export_text`), Cancel, and "Publish N Files" (included uploads +
    deletes).
  - **Delete guard:** when `DeleteGuard::trips`, the button reads "Review Deletions…" and opens
    `ReviewDeletionsSheet`. Typing the zone or bucket name enables "Delete N Files and Publish".
- **`purger_for`:**
  - Bunny, in order: the stored link + `account_key`; else the first kept Bunny account whose
    `cdn_zones_for(site.container)` answers, which is saved as the link; else `NeedKey`.
  - **`CdnKeySheet`:**
    - Text: "Purging needs your Bunny account API key. HTTPorter keeps it in Windows Credential Manager."
    - Save: validates with `cdn_zones_for`, saves the link and key, and resumes the pending publish or purge. On an
      error it shows `cdn-key-error`.
    - Cancel: goes on without the purge (`Unavailable`).
- **Run:**
  - Close the sheet and run `run_publish` on the runtime. The drawer shows the batches; the status line shows the
    phase ("Purging CDN… 120 of 300").
  - Then `add_history`, refresh the sidebar and the profile page, and show the result.
  - A purge failure says "CDN purge failed, Retry Purge". `retry_purge` purges the done paths again, then calls
    `set_history_purge`.
  - `retry_failed` = `retry_of`, with the profile's purge mode.
- **Delete alert:**
  - The purge checkbox is enabled on Bunny sites. It is checked when a link exists; otherwise it is unchecked, with
    "Asks for your Bunny account key".
  - Confirm: `submit_tracked` → `wait_batch` → `run_purge(Changed, done)`. A missing key opens `CdnKeySheet`; the
    delete never waits for it.
- **Sidebar:**
  - Publish section: an `i-send` row per profile, with its name and "2h ago"/"Never".
  - With no profile: "Click Publish to make a profile for the open site."

### T8: Profile page, Edit Profile, history, Settings key, `.env` profile (Track C)
**Files:**
- **Slint:** `profile_page.slint`, `profile_sheet.slint`, `settings_window.slint` (Publish tab).
- **Rust:** `publish/profile_view.rs`, the profile section of `publish/copy.rs`, `settings_view.rs` (Publish tab),
  `import.rs`, and `add_site.rs` (only the hook that runs after an imported site is saved).

**Build:**
- **Profile page** (`History.dc.html`):
  - **Header:** a folder → destination row with Change Folder…, and Publish (`open_sheet`).
  - **Chips:** "12 skip rules", "Remove extras: off", "Purge: changed files · Pull Zone 7654321", "Delete guard: 100
    files or 10%".
  - **Cards:**
    - Last publish: "Today 15:12" with "36 files · 4.2 MB · all verified".
    - On Bunny now, from the newest history row: "13,795 files" with "1,011 folders · replicated to 3 regions".
    - Schedule: "Off", with a disabled "Publish on a schedule…" link and the note "Arrives in a later version" (I6).
  - **History table:** When / Result / Uploaded / Deleted / Size / Time / CDN purge, with "Search history" (debounced
    250 ms).
  - **Failed detail:** "N files failed after 5 attempts (HTTP 503)" (the first error) and the failed paths. Buttons:
    "Show All N Files" (`history_files`), "Retry N Failed", and "Retry Purge" when the purge failed.
  - **Footer:** "N publishes · kept for the last 200".
- **ProfileSheet** (new and edit):
  - **Fields:** name; site (read-only); folder with Change Folder…; remote folder; skip rules (one per line, with
    Restore Defaults); remove extras; purge Off/Changed/Whole zone; guard files and percent.
  - **CDN note** under the purge choice: "Pull Zone 7654321 · key saved", or "Add Account Key…" → `profile-add-key` →
    `ask_cdn_key`.
  - **Save:** checks the folder, the name and the numbers.
  - **Delete Profile:** destructive, with a confirm.
- **Settings › Publish:** each kept Bunny account key (`bunny_accounts`) with a "Forget Key" button (`forget_cdn_key`).
- **`.env` import** (decision 5):
  - `EnvImport` gains `local_dir: Option<PathBuf>` (BUNNY_LOCAL_DIR) and `prefix: String` (BUNNY_PREFIX, trimmed of
    `/`).
  - After the imported site saves, create a profile: name = the zone, remote dir = the prefix, defaults otherwise.
    Skip it if the folder is missing.
  - Fix the `import.rs` doc line that says these are "ignored until I3".

### T9: Deferred I2 minors, Q52 and later (Track A, after T3)
1. **`engine.rs`, the `JOURNAL_CHUNK` writer:** a failed chunk loses up to 512 updates. Put it back at the head and
   retry with a short backoff; still count `journal_errors`.
2. **`lane.rs` `denied_or_auth`:** the probe lists the whole parent folder. List one page only, through a provider
   method that takes a prefix and a limit (S3 `prefix=key&max-keys=1`) and by default falls back to `list`.
3. **`httporter-s3/src/multipart.rs`:** a resumed upload meters all its skipped parts at once. Count them as done
   without feeding the speed/ETA meter.
4. **`engine.rs:197` `AbortRunning::drop`:** it bumps under the State lock at shutdown. Bump after releasing the lock.
5. **App `transfers.rs`:** `forget_site` drops that site's waiting questions; the waiting queue keeps the order they
   were asked in; `submit` drops, with a status message, a batch whose site no longer exists.
6. **`offline.rs`:** a lane that flaps within one `PROBE_EVERY` can start duplicate probes. Keep one probe per lane.
7. **M10, single-instance guard:** a second HTTPorter started during a run must not treat the first one's live queue
   as unfinished (resume would upload twice, discard would cancel the first one's rows). Use a named Windows mutex;
   the second start brings the first window to the front and exits.
8. **M13, drawer state precedence (`drawer_view.rs`):** user-paused plus offline shows Paused, and the button reads
   "Resume".
9. **M14, `resume_journal` (`transfers.rs`):** resume each site on its own; one site with an unreadable password must
   not block the others (it stays offered).
10. **M4, planning progress and cancel:** a long plan (a big folder upload, delete or publish compare) shows
    "Preparing… N files" and can be cancelled.

Source of items 7–10: the I2 final review's triage (`.superpowers/sdd/2026-11-02-httporter-i2-move-files/final-review.md`
M4, M10, M13, M14).

### T10: Tests, gate, live smoke (after every branch is merged)
**Unit and mock tests:**
- **Compare** (MockProvider + temp dir):
  - New; Changed by size; Changed by SHA-256; Same; RemoteOnly.
  - Skip rules on both sides (folder prune, `.env`).
  - Bunny with verify off compares by size only. S3: MD5 ETag, and multipart → mtime.
  - A missing remote root gives all New.
  - `child_statuses`.
  - Plan toggles, jobs, `retry_of`, and the `trips` edges.
  - The 12 default rules and `looks_secret`.
- **Purge URLs:**
  - A file; `blog/index.html` → 3 forms; the root `index.html` → 2.
  - Spaces and non-ASCII encoding.
  - 2 zones × 3 hosts, with dedupe.
  - `run_purge`: a partial failure gives Failed with counts; Zone with no zones gives Unavailable.
- **Purge gate:**
  - A mock provider with `cut_uploads_delete_late`, an upload re-sent after a cut-off (a pending check), and a
    recording `CdnPurger`.
  - No purge call while `pending_checks_for(site) > 0`; one after it. Use paused tokio time.
  - A failed upload makes the deletes NotRun, and the purge still runs for the done paths.
- **Store:**
  - v2 → v3 on an existing file; profile CRUD.
  - `add_history` sets `last_published_at` and trims at 201.
  - Search by path; the files round trip; `update_history_purge`.
  - The `remove_site` cascade; `cdn_link`; `remove_cdn_links_for`.
- **BunnyCdn** (wiremock): query encoding, the `purgeCache` path, 429 with Retry-After then OK, 401 → Auth.
- **App:** `lint_tests` and the existing view tests still pass. New view tests: the sheet button label (including
  Review Deletions…), the default switch above 300 URLs, the history row text.

**Gate:** `powershell -NoProfile -File tools/check.ps1` exits 0.

**Live smoke** (`crates/httporter-transfer/tests/publish_live.rs`, `#[ignore]`, `.env.test` via dotenvy, never
printed):
- **Guards:** assert the zone starts with `httporter-test` (the run uses `httporter-test-sg`). Refuse `exampleshop` and
  pull zone 7654321 by name.
- **Steps:**
  1. Make 50 files, including `sub/index.html`.
  2. Compare, then `run_publish` into `publish-smoke-<unix>/`.
  3. Change 3 files.
  4. Compare again: exactly 3 Changed.
  5. Publish: done == 3.
  6. Log the purge URLs that would be sent (host from `HTTPORTER_TEST_PULL_ZONE_HOST`, else
     `httporter-test-sg.b-cdn.net`).
  7. Send them only when `HTTPORTER_TEST_ACCOUNT_KEY` and `HTTPORTER_TEST_PULL_ZONE` (≠ 7654321) are both set. Today
     `.env.test` has neither, so the smoke only logs.
  8. Delete `publish-smoke-*`.
- **Report:** counts, the check.ps1 result, and the I3 checklist rows that pass, ticked to ✅.

---

## Not in I3
- **I4:**
  - Toolbar purge button, Inspector "Purge from CDN".
  - Command palette entries.
  - Sheet tree view and filter.
  - Per-profile `Cache-Control`.
  - S3 multipart-ETag recompute and the HeadObject sha256 lookup; decision 4 uses size + mtime instead.
  - A hash cache that survives restarts.
  - Quick Look "Changes vs Bunny".
  - Settings › Sites.
  - Search.
  - The view toggle.
- **I5:**
  - The Security tab and other key removal (M7).
  - Translation.
  - The tray.
  - The title-bar toolbar.
  - The app icon.
- **I6:**
  - Scheduled publish (only a disabled card link now).
  - The CLI.
  - The watch folder.
  - Creating zones and pull zones.
- **Q51 triage, marked I3 but outside this brief (which covers Q52+):**
  - Left out: M10 single-instance guard, M13 drawer pause/offline precedence, M14 resume all-or-nothing.
  - Already covered: M4 (planning progress/cancel), by the sheet's compare progress and Cancel.
