# HTTPorter I4 "Manage+", implementation plan (lean)

**Goal:** every I4 row of `docs/design/mockup-feature-checklist.md` works:
- **Main window:** view toggle, "Search {site}", Refresh and Purge buttons, sidebar Favorites and Editing, tabs,
  synchronized browsing.
- **Inspector** (`Inspector.dc.html`): list / thumbnail grid, image preview, metadata, links with copy, actions.
- **Quick Look** (`Main.dc.html` overlay `quicklook`, l.335–371); **command palette** (overlay `palette`, l.377–400,
  data l.550–557).
- **Files:** rename / move, copy public / presigned link, edit with auto re-upload.
- **Sites:** read-only, export / import without secrets, Settings › General / Sites / Advanced; activity log.
- **Deferred:**
  - I3 "Not in I3": toolbar / inspector purge, palette entries, publish-sheet tree + filter, per-profile
    `Cache-Control`, S3 multipart-ETag recompute + HeadObject sha256, a persistent hash cache, "Changes vs Bunny",
    Settings › Sites, search, the view toggle;
  - I2: M3, M5, M6, M8, M11, M12; ledger 97 (case-insensitive region compare), 135 (mono font fallback).
- **Size (S8):** the exe is 27.5 MB against a 25 MB budget; T0 brings it to ≤ 24 MB.

**Architecture:**
- **Providers:** core gains `stat` / `copy` / `presign` (s3, bunny). Move, search and the persistent hash cache go in
  `httporter-transfer`.
- **Moves reuse the engine:** Bunny = download → upload → delete batches; S3 = `CopyObject`, then a delete batch.
- **Storage:** schema v4 (hash cache, favorites, `profiles.cache_control`). Site extras and UI state are JSON in
  `settings`, so `SiteRecord` is unchanged. The activity log is rotating files (§11).
- **App:** a `manage/` module with a `Manager` (the I3 `Publisher` pattern); each UI task owns its Slint and Rust view
  files.
- **Small by rule:** no syntect, tree-sitter, `similar` or `notify`. A hand-written tokenizer, a Myers diff, a
  ReadDirectoryChangesW watcher on `windows-sys`. Images through Slint's own `image::open`, with the `image` crate's
  `webp` + `gif` features on (their decoders are already linked via resvg).

**Speed mode:** no per-task tests or reviews.
- **Done** = `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace --no-run`
  compiles + the app starts (`target\debug\httporter.exe` still running 5 s after `Start-Process`, then
  `Stop-Process`) + committed.
- A task that breaks a test on purpose (new fields, schema version) fixes it. All tests run in T10.

**Rules:**
- **Look:** reuse `Theme`, `CapsuleButton`, `CheckRow`, `AlertCard`, `Modal`, `chips.slint`, `form_controls.slint` and
  `ContextMenuArea`; Apple-HIG look.
- **Strings:** Slint `@tr()`; Rust user-facing strings only in `copy.rs` files (`lint_tests.rs` checks).
- **Secrets:** only in Credential Manager, never in SQLite, settings, exports, logs (`redact`), UI or error text.
- **Safety:** no `panic = "abort"`. No `.env` except `.env.test`, and only via dotenvy in live tests.
- **Unwired controls:** disabled, with "Arrives in a later version".
- **Builds:** one worktree per builder, `CARGO_BUILD_JOBS=2` (T0 and T1 use 3 while alone).
- **`Cargo.lock` conflict:** take either side, then `cargo metadata --format-version 1 >NUL`; never `cargo update`.

---

## 0. Shared interfaces

T1 commits all of this as `todo!()` stubs; everyone codes against it.
- **Exact names and signatures:** ask the controller before changing one; private items and extra fields are fine.
- **Derives:** data types derive `Debug, Clone, PartialEq, Eq` where possible; stored types use serde `snake_case`.
- **Startup paths are never `todo!()`:** installs, `log`, `read_only`, `with_store`, `hash_store`, `navigated`,
  `open_file` and `flush_edits` get real no-op bodies in T1.

### 0.1 `httporter-core`

```rust
// provider.rs, added to StorageProvider (default bodies are real, in T1)
pub struct ObjectInfo { pub entry: Entry, pub created: Option<DateTime<Utc>>, pub content_type: Option<String>,
    pub metadata: Vec<(String, String)> }                        // S3 x-amz-meta-* without prefix, lower-case
async fn stat(&self, path: &RemotePath) -> Result<ObjectInfo, ProviderError>; // default: list parent, find, else NotFound
async fn copy(&self, from: &RemotePath, to: &RemotePath) -> Result<(), ProviderError>; // default Permanent(copy::NO_COPY)
fn presign(&self, path: &RemotePath, expires: Duration) -> Result<String, ProviderError>; // default Permanent(NO_PRESIGN)
// UploadOptions gains `pub cache_control: Option<String>` (new() → None; Debug/PartialEq updated).
pub const META_SHA256: &str = "httporter-sha256";  pub const META_PART_SIZE: &str = "httporter-part-size";
// diff.rs
pub enum DiffOp { Same, Added, Removed }
pub struct DiffLine { pub op: DiffOp, pub old_no: Option<u32>, pub new_no: Option<u32>, pub text: String }
pub const DIFF_MAX_LINES: usize = 20_000;
pub fn diff_lines(old: &str, new: &str) -> Option<Vec<DiffLine>>;        // Myers; None above DIFF_MAX_LINES
// places.rs
pub enum PlaceKind { Local, Remote }
pub struct Favorite { pub id: i64 /*0 until stored*/, pub kind: PlaceKind, pub name: String, pub local: Option<PathBuf>,
    pub site: Option<SiteId>, pub remote: Option<RemotePath> }
pub struct TabState { pub site: Option<SiteId>, pub remote: Option<RemotePath>, pub local: Option<PathBuf> }
#[derive(Default)] pub enum ViewMode { #[default] TwoPanes, Inspector }
#[derive(Default)] pub struct UiState { pub tabs: Vec<TabState>, pub current: usize, pub sync: bool, pub view: ViewMode,
    pub grid: bool, pub appearance: u8 /*0 system 1 light 2 dark*/, pub restore_tabs: bool, pub ask_big_preview: bool }
pub const UI_STATE_KEY: &str = "ui_state";        // the two bools: serde default true
// site.rs
#[derive(Default)] pub struct SiteExtras { pub read_only: bool, pub public_base_url: Option<String>,
    pub presign_expiry_secs: u32 /*0 = 3600*/ }                 // #[serde(default)]
// sites_file.rs, export / import without secrets (T1 adds serde to ProviderKind, S3Site, S3Preset, DeleteGuard)
pub const SITES_FORMAT: &str = "httporter-sites";  pub const SITES_VERSION: u32 = 1;
pub struct SitesFile { pub format: String, pub version: u32, pub exported_at: DateTime<Utc>, pub sites: Vec<ExportedSite> }
pub struct ExportedSite { pub name: String, pub provider: ProviderKind, pub container: String, pub region: String,
    pub host: String, pub s3: Option<S3Site>, pub extras: SiteExtras, pub profiles: Vec<ExportedProfile> }
pub struct ExportedProfile { pub name: String, pub local_dir: PathBuf, pub remote_dir: RemotePath,
    pub skip_rules: Vec<String>, pub remove_extras: bool, pub purge: PurgeMode, pub guard: DeleteGuard,
    pub cache_control: Option<String> }
pub fn export_sites(sites: &[(SiteRecord, SiteExtras, Vec<ProfileRecord>)]) -> SitesFile;
pub fn parse_sites_file(text: &str) -> Result<SitesFile, String>;   // copy:: text: wrong format / newer version
// publish.rs: ProfileRecord gains `pub cache_control: Option<String>` (T1 fixes the 5 literals with None).
// activity.rs
pub enum ActivityKind { Upload, Download, Delete, Move, Publish, Purge, Edit, Site }   // Copy
pub struct ActivityEvent { pub at: DateTime<Utc>, pub kind: ActivityKind, pub site: String, pub path: String,
    pub ok: bool, pub detail: String, pub bytes: u64 }                                  // one JSON line
#[derive(Default)] pub struct ActivityFilter { pub kind: Option<ActivityKind>, pub failed_only: bool,
    pub site: Option<String>, pub text: String }
impl ActivityFilter { pub fn matches(&self, e: &ActivityEvent) -> bool }
/// Masks the values after Authorization, X-Amz-Signature/-Credential/-Security-Token, AccessKey, password=, key=,
/// token=, secret= (case-insensitive).
pub fn redact(text: &str) -> String;
// skip.rs: the existing matcher becomes `pub fn glob_match(pattern: &str, text: &str) -> bool` (case-insensitive).
```

### 0.2 `httporter-transfer`

```rust
pub trait HashStore: Send + Sync {                                 // hash.rs: the hash cache survives restarts
    fn load(&self, path: &Path, size: u64, modified: SystemTime) -> Option<FileHashes>;
    fn save(&self, path: &Path, h: &FileHashes); }
impl HashCache { pub fn with_store(store: Arc<dyn HashStore>) -> Self; }   // T1 real (keeps it); Debug made manual
// engine.rs
pub enum JobEventOutcome { Done, Failed(String), Cancelled }
pub struct JobEvent { pub site: SiteId, pub site_name: String, pub kind: JobKind, pub path: String, pub bytes: u64,
    pub outcome: JobEventOutcome }
impl Engine { pub fn events(&self) -> tokio::sync::broadcast::Receiver<JobEvent>;   // capacity 4096, lagging = lost
    /// Uploads under prefix on site send this Cache-Control (longest prefix wins); None removes the rule.
    pub fn set_cache_control(&self, site: &SiteId, prefix: &RemotePath, value: Option<String>); }
// moves.rs
pub struct MoveItem { pub from: RemotePath, pub to: RemotePath, pub size: u64 }
pub struct MovePlan { pub items: Vec<MoveItem> /*files*/, pub folders: Vec<(RemotePath, RemotePath)>, pub bytes: u64,
    pub server_side: bool /*caps.server_side_copy && every file ≤ 5 GiB*/ }
/// Folders expand via walk(); rename_to = new last segment (one source). Existing target → Conflict; into itself refused.
pub async fn plan_move(provider: &dyn StorageProvider, sources: &[Entry], to_dir: &RemotePath,
    rename_to: Option<&str>) -> Result<MovePlan, ProviderError>;
pub fn estimate_secs(plan: &MovePlan, files_per_s: f64, bytes_per_s: f64) -> u64;
pub enum MovePhase { Copying { done: u64, total: u64 }, Deleting, Finished }
pub struct MoveOutcome { pub copied: u64, pub deleted: u64, pub failed: Vec<(RemotePath, String)>, pub kept_copies: Vec<RemotePath> }
/// server_side: provider.copy 8 at a time. Else download batch into temp_dir → upload batch (checksums), each
/// submit_tracked + wait_batch. One delete batch only if every copy succeeded; else nothing is deleted and kept_copies
/// lists the copies. Bunny: empty folders created. temp_dir emptied at the end.
pub async fn run_move(engine: &Engine, target: BatchTarget, plan: &MovePlan, temp_dir: &Path,
    on_phase: &(dyn Fn(MovePhase) + Send + Sync)) -> Result<MoveOutcome, JournalError>;
// search.rs
pub enum KindFilter { Image, Code, Document, Archive, Media }        // by core::mime of the extension
#[derive(Default)] pub struct SearchQuery { pub text: String /*substring; glob_match if * or ?*/,
    pub kind: Option<KindFilter>, pub min_size: Option<u64>, pub modified_after: Option<DateTime<Utc>> }
#[derive(Copy, Default)] pub struct SearchProgress { pub folders: u64, pub found: u64, pub done: bool }
pub const SEARCH_LIMIT: usize = 10_000;
/// Lists from root with caps.list_concurrency; each folder's matches (files and folders) go to on_found as soon as it
/// is listed; stops at SEARCH_LIMIT; cancel = drop.
pub async fn search(provider: &dyn StorageProvider, root: &RemotePath, q: &SearchQuery,
    on_found: &(dyn Fn(Vec<Entry>, SearchProgress) + Send + Sync)) -> Result<SearchProgress, ProviderError>;
```

### 0.3 `httporter-platform`

```rust
// store.rs v4 (append): CREATE TABLE hash_cache (path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
//   sha256 BLOB NOT NULL, md5 BLOB NOT NULL, used_at TEXT NOT NULL); CREATE TABLE favorites (id INTEGER PRIMARY KEY
//   AUTOINCREMENT, kind TEXT NOT NULL, name TEXT NOT NULL, local TEXT, site TEXT, remote TEXT);
//   ALTER TABLE profiles ADD COLUMN cache_control TEXT;
pub fn site_extras_key(id: &SiteId) -> String;            // "site_extras:<id>", next to site_conflicts_key
impl Store {   // all Result<_, StoreError>; remove_site also drops the site's favorites, extras and conflict keys
    pub fn hash_get(&self, path: &Path, size: u64, mtime_ns: u64) -> Option<([u8; 32], [u8; 16])>;  // touches used_at
    pub fn hash_put(&self, path: &Path, size: u64, mtime_ns: u64, sha256: &[u8; 32], md5: &[u8; 16]);
    pub fn hash_prune(&self, unused_since: DateTime<Utc>) -> u64;  pub fn hash_count(&self) -> u64;  pub fn hash_clear(&self);
    pub fn favorites(&self) -> Vec<Favorite>;  pub fn add_favorite(&self, f: &Favorite) -> i64;  pub fn remove_favorite(&self, id: i64); }
// activity.rs, %APPDATA%\HTTPorter\logs\activity.log, activity.1.log … .9.log, 5 MB each
pub const LOG_FILES: usize = 10;  pub const LOG_FILE_BYTES: u64 = 5 << 20;  pub fn default_log_dir() -> PathBuf;
pub struct ActivityLog { /* dir, Mutex<(File, u64)> */ }
impl ActivityLog { pub fn open(dir: &Path) -> io::Result<Self>;
    pub fn append(&self, e: &ActivityEvent);                // redacts path/detail, rotates, never panics
    pub fn read(&self, f: &ActivityFilter, limit: usize) -> Vec<ActivityEvent>;   // newest first, across files
    pub fn export_text(&self, f: &ActivityFilter, to: &Path) -> io::Result<u64>;  pub fn dir(&self) -> &Path; }
// cache.rs, %LOCALAPPDATA%\HTTPorter\cache\{preview,thumbs,edit,move}
pub fn cache_dir(kind: &str) -> PathBuf;  pub fn prune_cache(older_than: Duration) -> u64;  pub fn cache_bytes() -> u64;
// win/shell.rs, win/clipboard.rs, win/watch.rs
pub fn open_default(path: &Path) -> Result<(), String>;  pub fn open_url(url: &str) -> Result<(), String>;
pub fn open_with(path: &Path) -> Result<(), String>;     // SHOpenWithDialog
pub fn set_clipboard_text(text: &str) -> Result<(), String>;
pub fn mono_font_family() -> &'static str;               // "Cascadia Mono" if installed, else "Consolas" (ledger 135)
pub struct DirWatcher { /* thread + handle; Drop stops it */ }
impl DirWatcher { pub fn start(dir: &Path, on_change: Box<dyn Fn(PathBuf) + Send>) -> io::Result<Self>; } // subtree
```

### 0.4 App, session and `crates/httporter-app/src/manage/`

```rust
impl Session {   // session/places.rs (T4), the session/store.rs pattern; errors as String
    pub async fn favorites(&self) -> Vec<Favorite>;  pub async fn add_favorite(&self, f: Favorite) -> Result<i64, String>;
    pub async fn remove_favorite(&self, id: i64) -> Result<(), String>;
    pub fn ui_state(&self) -> UiState;  pub fn save_ui_state(&self, s: &UiState);
    pub fn site_extras(&self, id: &SiteId) -> SiteExtras;   // cached
    pub async fn save_site_extras(&self, id: SiteId, x: SiteExtras) -> Result<(), String>;
    pub async fn forget_conflicts(&self, id: SiteId, upload: bool) -> Result<(), String>;   // M3
    pub fn hash_store(&self) -> Arc<dyn HashStore>;          // T1: real adapter, no-op bodies
    pub async fn export_sites(&self, to: PathBuf) -> Result<usize, String>;
    pub async fn import_sites(&self, from: PathBuf) -> Result<ImportReport, String>; }
pub struct ImportReport { pub added: usize, pub skipped: usize /*same provider+container+host*/, pub need_secret: Vec<String> }
// manage/mod.rs (T1; children read the fields)
pub enum Side { Local, Remote }
pub struct Manager { session: Arc<Session>, transfers: Arc<Transfers>, publisher: Arc<Publisher>, link: Link,
    inspector: Mutex<inspector_view::InspectorState>, quicklook: Mutex<quicklook_view::QuickLookState>,
    edits: Mutex<edit::Edits>, palette: Mutex<palette_view::PaletteState>, tabs: Mutex<tabs::TabsState>,
    search: Mutex<search_view::SearchState>, moves: Mutex<move_view::MoveState>,
    activity: Mutex<activity_view::ActivityState>, settings: Mutex<settings_tabs::SettingsState> }
impl Manager {
    pub fn new(session: Arc<Session>, transfers: Arc<Transfers>, publisher: Arc<Publisher>, link: Link) -> Arc<Self>;
    pub fn read_only(&self, site: &SiteId) -> bool;                    // T8 (T1: false)
    pub fn open_file(self: &Arc<Self>, side: Side, row: usize);        // Enter / double-click on a file (T6)
    pub fn flush_edits(&self);                                         // window close: send pending re-uploads (T6)
    /// Direct provider.download to cache_dir(kind)/<site>/<path> (no engine row); checks the listed checksum; > 50 MB
    /// asks unless ask_big_preview is off; reuses a cached copy with the same size + checksum. (T4)
    pub async fn fetch_to_cache(&self, site: &SiteRecord, entry: &Entry, kind: &str) -> Result<PathBuf, String>; }
pub fn install(ui: &AppWindow, m: &Arc<Manager>);   // calls every view's install; sets the global MANAGER
// Each view file has `pub fn install(ui: &AppWindow, m: &Arc<Manager>)`. Owners: T4 cache.rs · T5 inspector_view,
//   thumbs, links · T6 quicklook_view, highlight, edit, places · T7 palette_view, search_view, tabs · T8 settings_tabs
//   · T9 activity_view, move_view. Also: tabs::navigated(ui, Side) (binding.rs calls it after apply_local/apply_remote);
//   activity_view::open_log(dir) (main.rs) and log(ActivityEvent) (no-op without a log); settings_tabs::wire(&SettingsWindow)
//   (transfers.rs settings_window()). manage/copy.rs: one section per task, each header followed by 5 `//` lines.
impl Publisher {   // publish/purge_now.rs (new; T5): waits for pending_checks_for(site) == 0 (Q55), then purges
    // purge_urls(files) + "https://{host}/{folder}/*" per folder; NeedKey → CDN key sheet; logs a Purge event.
    pub fn purge_now(self: &Arc<Self>, site: SiteRecord, files: Vec<RemotePath>, folders: Vec<RemotePath>); }
```

### 0.5 Slint (T1), `types.slint`, `theme.slint`, `app.slint`

```slint
export struct InspectorData { shown: bool, name: string, kind-text: string, is-image: bool, multi: string,
    modified: string, created: string, content-type: string, regions: string, checksum-label: string, checksum: string,
    matches: string, public-url: string, public-note: string, storage-url: string, storage-note: string,
    can-purge: bool, purge-note: string, can-write: bool, busy: bool }
export struct CodeSpan { text: string, style: int }   // 0 plain 1 tag 2 attr 3 string 4 comment 5 keyword 6 number 7 punct
export struct CodeLine { no: string, spans: [CodeSpan], mark: int }   // 0 none 1 added 2 removed
export struct QuickLookData { title: string, subtitle: string, kind: int /*0 text 1 image 2 other*/, can-diff: bool,
    diff-label: string, diff-note: string, footer-left: string, footer-right: string, busy: bool, error: string, can-browser: bool }
export struct EditConflictData { name: string, body: string }
export struct PlaceItem { id: int, name: string, subtitle: string, remote: bool }
export struct EditItem { id: int, name: string, state: string, busy: bool, failed: bool }
export enum PaletteIcon { action, publish, purge, upload, download, folder, site, profile, favorite, shortcut, settings }
export struct PaletteItem { header: bool, title: string, subtitle: string, icon: PaletteIcon, keys: string, enabled: bool }
export struct TabItem { title: string, subtitle: string }
export struct SearchFilters { scope: int /*0 site 1 folder*/, kind: int, size: int, modified: int }
export struct RenameData { title: string, name: string, note: string, error: string, busy: bool }
export struct MoveData { title: string, destination: string, note: string, error: string, busy: bool }
export struct MoveWarningData { title: string, body: string, button: string }
export struct PresignData { title: string, expiry: int, error: string }   // 0 15 min 1 1 h 2 1 day 3 7 days
export struct ActivityRow { when: string, kind: string, site: string, path: string, detail: string, failed: bool }
export struct ActivityFilterData { kind: int /*0 all … 6 errors*/, site: int /*-1 all*/, text: string }
export struct SiteSettingsRow { id: string, name: string, subtitle: string, read-only: bool }
export struct SiteSettingsData { id: string, read-only: bool, public-url: string, presign: bool, presign-expiry: int,
    cdn: string, upload-choice: string, download-choice: string, error: string }
// FileRow + `thumb: image`; PlanRow + `depth: int, folder: bool, expanded: bool`; ProfileDraft + `cache-control: string,
// cache-control-enabled: bool, cache-control-note: string` (T1 fixes the Rust literals: FileRow 5, PlanRow 2, Draft 2).
// Theme + `in-out property <int> appearance` (0 system 1 light 2 dark; `dark` derives from it) and
// `in-out property <string> font-mono`. app.slint exports Theme. Globals are per window: set it on both windows.
```

**`AppWindow` hooks** (nobody edits `app.slint` after T1):
- **View / inspector:** `view-mode` (in-out, 0 two panes / 1 inspector), `single-side` (in-out, 0 local /
  1 remote), `grid` (in-out), `set-view(int)`, `set-grid(bool)`, `inspector`, `inspector-image`,
  `inspector-quick-look()`, `inspector-download()`, `inspector-delete()`, `inspector-copy(int)`, `inspector-purge()`.
- **Toolbar:** `can-purge`, `purge()`, `sync-browsing` (in-out), `toggle-sync()`, `search-placeholder`, `search-text`
  (in-out), `searching`, `search-filters` (in-out), `search-submitted(string)`,
  `search-filters-changed(SearchFilters)`, `search-cancel()`.
- **Panes:** `can-write`, `presign-available`, `quick-look(int pane)`, `rename-start()`, `move-start()`,
  `copy-link(int pane)`, `copy-path(int pane)`, `copy-presigned()`, `add-favorite(int pane, int row)`,
  `drag-within-remote(int row)`.
- **Quick Look and edit:** `ql-open` (in-out), `ql`, `ql-lines`, `ql-image`, `ql-mode` (in-out),
  `ql-set-mode(int)`, `ql-step(int)`, `ql-open-browser()`, `ql-open-with()`, `ql-close()`; `edit-conflict-open`,
  `edit-conflict`, `edit-conflict-choice(int /*0 overwrite 1 copy 2 discard*/)`.
- **Sidebar:** `favorites`, `favorite-selected(int)`, `favorite-remove(int)`, `edits`, `edit-selected(int)`,
  `edit-stop(int)`.
- **Palette and tabs:** `palette-open` (in-out), `palette-items`, `palette-query` (in-out),
  `palette-query-changed(string)`, `palette-run(int)`; `tabs`, `current-tab` (in-out), `tab-selected(int)`,
  `tab-close(int)`, `tab-new()`.
- **Sheets:** `rename-open`/`-data`/`-confirm(string)`, `move-open`/`-data`/`-confirm(string)`,
  `move-warning-open`/`move-warning`/`move-warning-confirm()`, `presign-open`/`-data`/`presign-copy(int)`.
- **Activity:** `activity-open`, `activity-rows`, `activity-sites`, `activity-filter` (in-out),
  `activity-filter-changed(ActivityFilterData)`, `activity-export()`, `activity-open-folder()`.
- **Other:** `read-only-banner-text` (a banner like the others); `transfers-retry-selected([int])` (M11);
  `plan-view(int)`, `plan-filter-changed(string)`, `plan-expand(int)`.
- **Keys** (the FocusScope; none fire while a text field or sheet has focus):
  - Space → `quick-look(active)`; Ctrl+K → palette; `?` → palette with the query "?";
  - F2 → `rename-start`; Ctrl+Shift+C → `copy-link(active)`; Ctrl+F → focus search; Ctrl+1/2 → focus pane;
  - Ctrl+T → `tab-new`; Ctrl+W → `tab-close`; Ctrl+(Shift+)Tab → next / previous tab.

**Shells**, placed in `app.slint`, each with its properties plus an empty `Rectangle` behind its open flag; Esc closes:
- **Panels:**
  - `inspector.slint` `Inspector`: 300 px, right of the shown pane in view-mode 1; the other pane gets
    `visible: false`.
  - `quicklook.slint`: `QuickLook` (760×560 on a scrim), `EditConflictSheet`.
  - `palette.slint` `CommandPalette`: 600 px, 90 px from the top.
  - `tab_strip.slint` `TabStrip`: while `tabs.length > 1`.
  - `search_bar.slint` `SearchFilterBar`: under the toolbar while `searching`.
- **Sheets:** `manage_sheets.slint` (`RenameSheet`, `MoveSheet`, `MoveWarningSheet`, `PresignSheet`);
  `activity.slint` `ActivitySheet`.
- **Settings tabs:** `settings_tabs.slint` `GeneralTab` / `SitesTab` / `AdvancedTab`, shown by
  `settings_window.slint` for tabs 0 / 1 / 5. `SettingsWindow` declares:
  - **General:** `appearance`, `start-view`, `restore-tabs`, `ask-big-preview` (in-out), `preview-cache-text`,
    `clear-preview-cache()`, `general-edited()`;
  - **Sites:** `site-rows`, `site-current`, `site-form` (in-out), `site-form-save(SiteSettingsData)`,
    `site-forget-choice(bool)`, `export-sites()`, `import-sites()`, `sites-message`;
  - **Advanced:** `hash-cache-text`, `clear-hash-cache()`, `show-activity()`, `open-log-folder()`.

**T1 also builds, fully** (tiny, so no two tasks share them):
- **Toolbar search field:** LineEdit + `i-search`; Enter → `search-submitted`, Esc / clear → `search-cancel`.
- **Pane menus** (`pane.slint`):
  - remote: Quick Look, Open / Edit, Rename… (F2), Move To…, Copy Public Link, Copy Path, Copy Presigned Link… (if
    `presign-available`), Add to Favorites (folders), Purge from CDN;
  - local: Quick Look, Add to Favorites;
  - write items are disabled when `!can-write`.
- **`Pane.grid` property.**
- **Same-pane drag:** a remote-pane drag that ends on a folder row in the same pane → `drag-within-remote(row)`.

**T1 Rust edits:**
- **`binding.rs`:** `install_app` builds the `Manager` after the `Publisher` and calls `manage::install`;
  `apply_local` / `apply_remote` end with `tabs::navigated`; a file row's `open-local` / `open-remote` calls
  `open_file`; the `can-upload` / `can-new-folder` / `can-delete` / `can-write` spots check `read_only`.
- **`main.rs`:** `open_log(default_log_dir())`; a thread for `prune_cache(7 days)`; `flush_edits` first in the close
  path.
- **`transfers.rs`:** `settings_window()` calls `settings_tabs::wire`. The file is CRLF: restore CRLF after
  `cargo fmt`, or the diff becomes the whole file.
- **`publish/mod.rs`:** `HashCache::with_store(session.hash_store())`.
- **Lint allows:** stubs carry `#![allow(unused_variables, dead_code)]`; drop them as you fill.

---

## Tracks

| Track | Tasks | Owns |
|---|---|---|
|, | **T0** size (at once, ‖ T1) | Cargo manifests (root, bunny, s3, app), `ui/fonts/`, `deny.toml`, `tools/subset_fonts.py` |
|, | **T1** skeleton (~25 min) | every §0 stub, `app.slint`, `types.slint`, `theme.slint` |
| A engine | T2 → T3 | core `provider.rs`, s3, bunny (T2); transfer moves / search / engine / compare / hash / lane (T3) |
| B data | T4 → T9 | platform store / activity / cache / win, `session/places.rs`, `manage/cache.rs`; then T9 |
| C1 | T5 | inspector, single pane, grid, thumbnails, toolbar buttons, links, `publish/purge_now.rs` |
| C2 | T6 | Quick Look, highlight, core `diff.rs`, edit, sidebar Favorites / Editing |
| C3 | T7 | palette, search, tabs, sync browsing, `session/remote.rs` |
| C4 | T8 | Settings General / Sites / Advanced, read-only, export / import, M3 M5 M6 M12 |
|, | **T10** tests + live smoke | **only after the user has used the build** |

**Order:**
- T0 and T1 start together; T0 shares only `Cargo.lock` with T1 and merges when green.
- After T1: T2, T4, T5, T6, T7, T8 start together (6 builders); T3 follows T2, T9 follows T4.
- UI tasks compile against the stubs and work end to end once A and B are merged; merge each branch as it lands.

**Shared files** (small edits, named spots only):
- `binding.rs`: T1, T5 (the grid / thumb row spot).
- `transfers.rs` (CRLF): T8 (read-only guard in `submit`), T9 (M8).
- `publish/mod.rs`: T1, then T9.
- `toolbar.slint`, `pane.slint`: T1, then T5.
- Core `lib.rs`: T1.
- `manage/copy.rs`, `publish/copy.rs`: own section.

---

### T0, Exe size ≤ 24 MB (starts at once)
**Files:** root `Cargo.toml`; `Cargo.toml` of bunny, s3 and app; one line per reqwest client builder (bunny
`client.rs`/`account.rs`, s3 `storage.rs`); `ui/fonts/*.ttf`; `tools/subset_fonts.py`; `deny.toml`.

**Measure first:**
- `cargo build --release -p httporter-app`; record the bytes.
- Attribute I3's +3.8 MB (23.7 → 27.5): link with `-C link-arg=/MAP:<scratch>\h.map -C symbol-mangling-version=legacy`
  and list the top crates and `httporter` symbols.
- Measure after each step; keep a step only if it shrinks the exe and the app still draws right.

**Steps:**
1. **rustls on ring** (expected −2.5 to −3.5 MB):
   - reqwest `rustls` → `rustls-no-provider`;
   - workspace dep `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12",
     "logging"] }` in bunny and s3;
   - before each `Client` build, `let _ = rustls::crypto::ring::default_provider().install_default();` (idempotent,
     so tests work too);
   - profiles: drop the `aws-lc-*` entries, add `ring` at opt-level 3;
   - `cargo tree -i aws-lc-rs -e features` must print nothing.
2. **Subset the 4 Inter weights** (`pyftsubset`; expected −1.2 to −1.4 MB):
   - unicodes `U+0000-024F,0300-036F,1E00-1EFF,2000-206F,20A0-20CF,2100-215F,2190-21FF,2200-22FF,2300-23FF,25A0-25FF,
     2713,2715`;
   - features `kern,liga,calt,tnum,case`; hinting kept; same file names; `OFL.txt` stays;
   - **`pip install fonttools` is a PyPI download: only with the user's yes, relayed by the controller**, else skip
     and report;
   - check that a Cyrillic / CJK file name still renders (system-font fallback).
3. **`[profile.release.package.httporter-app] opt-level = "z"`:** keep it if it saves ≥ 200 KB (expected −0.3 to
   −0.8 MB).
4. **App image formats:** `image = { version = "0.25", default-features = false, features = ["webp", "gif"] }`, so
   Slint loads webp / gif. Expected < +150 KB.
5. **Licences:** update `deny.toml` if `cargo deny check licenses bans` asks (ring: "Apache-2.0 AND ISC").

**Done:** the criteria above + `cargo deny`; the release build still lists the user's Bunny site; report the bytes
per step and the I3 attribution.

### T1, Interfaces skeleton
**Files:** everything named in §0 (new: core `diff/places/sites_file/activity.rs`, transfer `moves/search.rs`, platform
`activity/cache.rs` and `win/{shell,watch}.rs`, app `session/places.rs`, `manage/*.rs`, `publish/purge_now.rs`, the 8
Slint shells) plus the edits in §0.5.

**Build:** §0 exactly, with bodies `todo!("T<n>")`, except under the startup-path rule. No v4 schema yet.

**Done:** the criteria above; report the commit hash.

### T2, Provider additions (Track A)
**Files:** core `provider.rs`, `copy.rs`; s3 `storage.rs`, `sigv4.rs`, `xml.rs`, `multipart.rs`; bunny `client.rs`,
`list.rs`, `model.rs`.

**S3:**
- **`stat`:** `HeadObject` → `ObjectInfo` (Content-Type, Last-Modified, ETag kind, x-amz-meta-*).
- **`copy`:** `CopyObject` (`x-amz-copy-source` with an encoded key, `MetadataDirective: COPY`). A 200 with an
  `<Error>` body is an error. Objects > 5 GiB → Permanent.
- **`presign`:** SigV4 query (`X-Amz-Algorithm/-Credential/-Date/-Expires ≤ 604800/-SignedHeaders=host/-Signature`,
  `X-Amz-Security-Token` when set, `UNSIGNED-PAYLOAD`); `Capabilities.presign = true`.
- **Uploads:** send `x-amz-meta-httporter-sha256` when `opts.sha256` is set; `x-amz-meta-httporter-part-size` on
  multipart Create; `Cache-Control` when set.

**Bunny:** `stat` = the parent listing + `DateCreated` (keep the raw field). `copy` / `presign` stay default;
`cache_control` is ignored (the pull zone sets caching).

### T3, Move, search, persistent hashes, multipart compare, events (Track A, after T2)
**Files:** transfer `moves.rs`, `search.rs`, `engine.rs`, `lane.rs`, `hash.rs`, `compare.rs`.

**Build:**
- **`moves.rs`, `search.rs`:** per §0.2; search is case-insensitive and uses the provider's listing caps.
- **`HashCache`:** memory, then `store.load`; `store.save` after hashing.
- **Multipart compare:** an equal-size pair whose remote is a `MultipartEtag` → `provider.stat()`, 8 at a time:
  - `httporter-sha256` meta → compare SHA-256;
  - else `httporter-part-size` → recompute the multipart ETag locally (MD5 of the part MD5s + `-N`);
  - else size + date, as now (the only case the weakest method reports as "size and date").
- **Lane:** each upload hashes SHA-256 + MD5 together (S3 needs the sha for the meta) and applies the longest
  `set_cache_control` rule.
- **`events()`:** one event per job when it reaches Done / Failed / Cancelled.

### T4, Store v4, places, export/import, activity file, cache, Windows helpers (Track B)
**Files:** platform `store.rs`, `activity.rs`, `cache.rs`, `win/*`, platform `Cargo.toml` (windows features
`Win32_UI_Shell`, `Win32_System_DataExchange`, `Win32_System_Memory`, `Win32_System_IO`); core `sites_file.rs`,
`activity.rs`; app `session/places.rs`, `manage/cache.rs`.

**Build:**
- **Store:** v4 per §0.3; `user_version` asserts use `MIGRATIONS.len()`; `hash_prune(90 days)` once at start, on a
  background thread.
- **`import_sites`:** skip sites already present; add the rest with new ids, extras and profiles; they go in
  `need_secret` (the existing "needs password" state; Edit Site asks).
- **`export_sites`:** pretty JSON; assert the output has no `password`, `key` or `secret` field names.
- **`DirWatcher`:** a thread in `ReadDirectoryChangesW` on a `FILE_LIST_DIRECTORY` handle; `CancelIoEx` + close on
  Drop; an overflow reports the dir.
- **`mono_font_family`:** checks `%WINDIR%\Fonts\CascadiaMono*.ttf`.
- **`fetch_to_cache`:** writes `.part`, then renames.

### T5, Single pane, inspector, grid, thumbnails, toolbar, links, purge (Track C1)
**Files:** `inspector.slint`, `pane.slint` (grid), `toolbar.slint`, `icons.slint`;
`manage/{inspector_view,thumbs,links}.rs` + their copy section; `publish/purge_now.rs`; the grid spot in
`binding.rs` / `viewmodel.rs`.

**Toolbar** (`Inspector.dc.html`, `Main.dc.html` l.69–80):
- View group: Two panes | Single pane with inspector | Compare.
- In inspector mode: a Layout group (List | Thumbnails) and Refresh.
- Purge from CDN: Bunny, with a remote selection.
- A sync button (`i-lock`) in two-pane mode.
- The view persists in `UiState`.

**Single pane:** shows the remote pane when a site is open, else the local one.

**Inspector:**
- **Preview:** an image (local, or `fetch_to_cache("preview")` ≤ 20 MB) or a large kind icon.
- **Header:** the name, and "JPEG image · 2048 × 1536 · 211.6 KB" (`image::image_dimensions`).
- **Buttons:** Quick Look and Download.
- **Rows:** Modified, Created (`stat`), Content type, Replicated "Singapore → Los Angeles, Frankfurt" (region codes
  case-insensitive, ledger 97), SHA-256 / MD5 / ETag.
- **Matches local:** "Yes · same checksum" / "No · different" / "No local copy", against the same relative path under
  the other pane's folder, through the Publisher's `HashCache`.
- **Links**, each with copy (`set_clipboard_text`):
  - Public: `cdn_link` hostnames or `public_base_url`, "Public · Pull Zone N"; with neither, "Add a public URL in
    Settings › Sites".
  - Storage: `endpoint()` + path, "Storage API · needs password".
- **Purge from CDN** → `purge_now`.
- **Multi-select:** "3 items · 1.2 MB".

**Grid:** 120 px tiles, virtualized rows.

**`thumbs.rs`:** images ≤ 5 MB → `image::open` → `thumbnail(240, 240)` → PNG in `cache_dir("thumbs")` (key: path +
size + mtime); 4 at a time on blocking threads; visible rows only.

**`links.rs`:** Copy Public Link (Ctrl+Shift+C; one URL per line; the first hostname) and Copy Path, then the status
"Copied 3 links".

**`purge_now`:** per §0.4, with the status "Purging CDN… n of m".

### T6, Quick Look, edit with re-upload, sidebar Favorites / Editing (Track C2)
**Files:** `quicklook.slint`, `sidebar.slint`; `manage/{quicklook_view,highlight,edit,places}.rs` + their copy
section; core `diff.rs`.

**Quick Look** (`Main.dc.html` l.335–371):
- **Header:**
  - close; the title; the subtitle "HTML · 383.4 KB · on this PC · changed since last publish";
  - Source | "Changes vs Bunny" (S3: "Changes vs {site}");
  - Open in Browser (HTML: the public URL for a remote file, the file for a local one);
  - Open With… (`open_with`).
- **Body:**
  - text: a virtualized `ListView` of `CodeLine`, 44 px numbers, mono (`mono_font_family` → `Theme.font-mono`,
    ledger 135);
  - image: `ql-image`;
  - other: a large icon + "Open in Default App".
- **Footer:** "Space or Esc to close · ← → previous / next file" and "UTF-8 · LF · 3 lines changed".
- **Keys:** Space / Esc close; ← → step through the selection (or the folder's files).

**Text:**
- **Which files:** known extensions (html htm css js mjs json xml svg txt md map csv yml yaml toml ini), or sniffed
  (no NUL in 8 KB + valid UTF-8).
- **Limits:** at most 2 MB / 20,000 lines; over 50 MB asks.

**`highlight.rs`:** a per-line state machine carried across lines.
- html / xml / svg: tags, attributes, strings, comments; `<script>` / `<style>` as js / css.
- css: selectors, properties, strings, comments, numbers.
- js / json: keywords, strings, templates, comments, numbers.
- md: headings, code spans, links.

**Changes vs Bunny:**
- **Counterpart:** the same relative path under the two pane roots (the compare rule), else via a profile whose folder
  holds the file.
- The remote side comes from `fetch_to_cache("preview")`; then `diff_lines(remote, local)`.
- Added lines green-soft, removed red-soft.
- Disabled with a note when there is no counterpart or the file is too large.

**Edit** (§8.5.2):
- **Start:** a remote file opened (Enter / double-click / menu) → `fetch_to_cache("edit")`, keeping the base checksum
  and modified time → `open_default`. One `DirWatcher` on the edit root.
- **On change:** debounce 1 s; skip when size + SHA-256 equal the last sent.
- **Before upload:** `stat` the remote. If it differs from the base → `EditConflictSheet` "index.html changed on Bunny
  since you opened it": Overwrite / Save as Copy (`name (edited).ext`) / Discard My Edit.
- **Upload:** `submit_tracked` + `wait_batch`; the new base is the uploaded checksum; the status "Uploaded index.html
  to example-shop.com"; an Edit event.
- **Local files:** open with `open_default`, unwatched.

**Sidebar:**
- **Editing:** a row per session ("Opening…", "Editing", "Uploading…", "Uploaded 15:12", "Changed on Bunny",
  "Failed"), with Stop in the menu; `flush_edits` sends pending changes now.
- **Favorites:** folder / cloud rows (the subtitle is the site or the parent path). A click opens it (local:
  `open_local`; remote: select the site + `open_remote`). The menu has Remove.

### T7, Command palette, search, tabs, synchronized browsing (Track C3)
**Files:** `palette.slint`, `tab_strip.slint`, `search_bar.slint`; `manage/{palette_view,search_view,tabs}.rs` + their
copy section; `session/remote.rs` (search mode only).

**Palette** (`Main.dc.html` l.377–400, 550–557):
- **Sections:**
  - Actions: Publish "{profile}" (Ctrl P), Purge from CDN…, Upload Files… (Ctrl U), Download, New Folder, Rename,
    Move To…, Copy Public Link, Compare, Two Panes / Single Pane, New Tab, Activity Log, Settings, Add Site, Refresh;
  - Sites, Profiles, Favorites, Folders (the current remote folder's subfolders);
  - Shortcuts (§8.7.3), for a `?` query.
- **Matching:** subsequence fuzzy match (prefix and word starts first); the top 50.
- **Run:** ↑↓ / Enter → the window's `invoke_*`. Disabled items say why.

**Search** (§8.2.6):
- **Start:** Enter in the toolbar field searches the open site, from the root ("Whole site") or the remote folder
  ("This folder").
- **Filter bar:** Kind (Any / Images / Code / Documents / Archives / Media); Size (Any / > 100 KB / > 1 MB / > 10 MB);
  Modified (Any / Today / 7 / 30 days).
- **Results:** they stream into the remote pane in search mode.
  - Crumbs "Results for “x”"; row names are relative paths.
  - Status "Searching… 1,204 folders · 37 found".
  - Selection, Quick Look, download, delete, copy link and the inspector all work: `remote_entries` resolves full
    paths.
- **End:** Esc / clear / a site change.

**Tabs** (§8.2.3):
- **A tab** = `TabState`; switching saves the places and opens the tab's (`select_site` + `open_remote`,
  `open_local`).
- **Keys:** Ctrl+T duplicates; Ctrl+W closes (never the last).
- **Restore:** at start when `restore_tabs`. `navigated` updates the title "site · folder".

**Synchronized browsing** (§8.2.8):
- **When on:** opening subfolder x in one pane opens x in the other if it exists; Up / back mirror.
- **Otherwise:** sync turns off with "Synchronized browsing stopped: {x} isn't in {other}". A `mirroring` flag stops
  echoes. Persists in `UiState`.

### T8, Settings General / Sites / Advanced, read-only, export/import, M3 M5 M6 M12 (Track C4)
**Files:** `settings_tabs.slint`, `add_site.slint` (M6 / M12 lines only); `manage/settings_tabs.rs` + its copy section;
`import.rs` (M5); `add_site.rs` / `session/sites.rs` (M6, M12); `transfers.rs` (read-only guard in `submit`).

**General:** Appearance System / Light / Dark (`Theme.appearance` on both windows; saved in `UiState`); Start in Two
panes / Single pane; Restore tabs at start; Ask before previewing files over 50 MB; Preview cache "123 MB" + Clear.

**Sites** (master-detail):
- **Form:**
  - Read-only ("Writes are disabled. Use this with a Bunny read-only password or read-only S3 keys.");
  - Public base URL (validated `https://…/`);
  - CDN "Pull Zone 7654321 · key saved" / "No pull zone linked";
  - presigned expiry (S3 only);
  - M3: the remembered "file exists" choices for uploads and downloads, each with Forget.
- **Save:** inline errors.
- **Export Sites… / Import Sites…** (`pick_save_file` / `pick_file`, `.json`) → `sites-message`: "Added 2 sites.
  Enter the password for example-shop.com in Edit Site."

**Advanced:** Show Activity Log…, Open Log Folder; Checksum cache "12,345 files remembered" + Clear.

**Read-only:**
- `read_only` = `SiteExtras.read_only`. Every write is off (toolbar, menus, drops, paste, edit, rename, move, publish).
- `submit` refuses write jobs for that site.
- The sidebar subtitle ends "· Read-only"; the banner says "example-shop.com is read-only. Uploads, deletes and edits are
  off."

**I2 minors:**
- **M5:** `aws_session_token` profiles are labelled "Temporary credentials, not supported" and disabled.
- **M6:** account-key zones that are already sites show "Already added", unchecked and disabled.
- **M12:** an S3 `http://` endpoint to a non-loopback host warns "Not encrypted: files and signed requests travel in
  clear text." Saving is still allowed.

### T9, Activity, rename/move/presign, M8, M11, publish tree/filter, Cache-Control (Track B, after T4)
**Files:** `activity.slint`, `manage_sheets.slint`, `drawer.slint` (Failed tab), `publish_sheet.slint`,
`profile_sheet.slint`; `manage/{activity_view,move_view}.rs` + their copy section; `drawer_view.rs`;
`publish/{mod,sheet_view,profile_view,copy}.rs`; `transfers.rs` (M8 only).

**Activity:**
- **Events:** a runtime task maps `engine.events()` → `ActivityEvent`; publish (after `add_history`), purge, move,
  edit and site add / remove go through `activity_view::log`.
- **Sheet:** a kind control (All / Uploads / Downloads / Deletes / Moves / Publish / Errors), a site popup, search
  (250 ms), a virtualized list of the newest 2,000, Export… (`export_text`), Open Log Folder.

**Rename / move:**
- **Rename:** F2 → `RenameSheet` (name selected, "/" refused).
- **Move To…:** `MoveSheet` (the destination path, the current folder by default) or `drag-within-remote` →
  `plan_move`.
- **Bunny first shows `MoveWarningSheet`:** "Bunny can't copy on the server. HTTPorter downloads and re-uploads N files
  (X MB, about Y min), then deletes the originals, only if every copy succeeds."
- **Run:** `run_move` into `cache_dir("move")` with "Moving… n of m"; refresh both folders.
- **Failure:** "3 copies failed; nothing was deleted. Copies made: …".

**Presign:** `PresignSheet` (15 min / 1 hour default / 1 day / 7 days; the default from `SiteExtras`) → `presign` →
clipboard, one line per file.

**I2 minors:**
- **M8:** "N connections ready" uses `engine.site_transfer_limit(site)` when it is below the setting.
- **M11:** Failed tab Ctrl/Shift multi-select + "Retry Selected" → `engine.retry(&ids)`.

**Publish:**
- **Sheet:** Flat | Tree (folders with counts, expand / collapse, a folder checkbox toggles its files) and a filter
  (path substring, 200 ms).
- **Cache-Control:**
  - `ProfileSheet` field "Cache-Control header" (e.g. `public, max-age=3600`), S3 only. Bunny: disabled, "Bunny sets
    caching in the pull zone."
  - Publish calls `engine.set_cache_control(site, remote_dir, value)` first; every profile's rule is registered at
    start.

### T10, Tests, gate, live smoke (**only after the user has used the build**)
The user builds, installs and uses the I4 build first; start when the controller says so.

**Unit and mock tests:**
- **Core / transfer:**
  - `diff_lines` (insert / delete / change / empty / cap), `glob_match`, `redact` (each key), `ActivityFilter`;
  - the sites-file round trip without secret fields;
  - `plan_move` (rename, folder expansion, into itself, existing target);
  - `run_move` on MockProvider: Bunny (a failed upload → nothing deleted, `kept_copies`) and S3 server-side;
  - `search` (glob, kind, size, limit);
  - `HashCache` with a store; multipart compare (meta, recompute, fallback);
  - `set_cache_control` longest prefix; `events()`.
- **s3 / bunny (wiremock):**
  - `stat` and its meta; CopyObject (200 + `<Error>`); the upload meta and Cache-Control;
  - presign = the AWS documented example (`examplebucket/test.txt`, 86400 s);
  - Bunny `DateCreated`.
- **Platform:** v3 → v4 on a file; `hash_*`; favorites; the `remove_site` cascade; `ActivityLog` rotation and `read`;
  `DirWatcher` sees a write and a rename-replace.
- **App:** `lint_tests` + the existing tests; highlight tokens; palette scoring; read-only flags; inspector texts
  (case-insensitive regions, matches local); M8.

**Gate:** `powershell -NoProfile -File tools/check.ps1` exits 0. Then the S8 release size, ≤ 25 MB.

**Live smoke** (`crates/httporter-transfer/tests/manage_live.rs`, `#[ignore]`, `.env.test` via dotenvy, never printed):
- **Guards:** zone and bucket start with `httporter-test`. Refuse `exampleshop` and pull zone 7654321 by name.
- **R2:**
  1. upload 3 files + a 70 MB file;
  2. `stat` has the sha256 meta;
  3. compare says Same for the multipart file;
  4. rename via `CopyObject`;
  5. presign GET (60 s) → 200, a tampered signature → 403;
  6. search `*.txt` → 3.
- **Bunny:** a 20-file folder move (originals gone after the late-delete settle); `stat` created; search.
- **Clean up:** delete `manage-smoke-*`.
- **Report:** counts, `check.ps1`, exe bytes, and the I4 checklist rows that pass, ticked to ✅.

---

## Not in I4
- **I5:**
  - Settings › Security, other key removal (M7), translation (M9), the tray, the title-bar toolbar, the app icon;
  - undo (§8.7.7), in-app toasts (I4 uses the status line), "Reset HTTPorter…" (§11);
  - persisted window size and column widths;
  - the accessibility pass, the ledger I5 items, M15.
- **I6:** the CLI, scheduled publish, the watch folder, creating zones / buckets, zone and bucket settings.
- **Later, by design:**
  - `UploadPartCopy` for S3 objects > 5 GiB (they move by download + upload);
  - free size / date ranges in search (presets now);
  - several public hostnames as a copy submenu (the first is copied; the inspector lists all).
