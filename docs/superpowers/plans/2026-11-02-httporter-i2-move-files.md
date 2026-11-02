# HTTPorter I2 "Move files" Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** HTTPorter uploads, downloads, deletes and creates folders on Bunny Storage and S3-compatible services, with a queue drawer, conflict sheet, delete alert, adaptive concurrency, a journal that survives crashes, a Settings window (Transfers), site edit/duplicate/remove, Bunny account-key sign-in, S3 sites, a Welcome screen and import from `.env` / AWS profiles.

**Architecture:** Two new crates join the I1 workspace. `httporter-s3` is an S3 client: our own SigV4 signer, reqwest and quick-xml. `httporter-transfer` is the transfer engine: queue, adaptive per-site limits, retries, bandwidth cap and journal, plus planning for uploads, downloads and deletes. `httporter-core` gains the contracts both of them and the UI share: the refined errors, upload metering, the journal trait, transfer view types and transfer settings. The app keeps **one shared provider client per site** (`SiteHub`) for browsing and transfers, so Bunny's per-client limits hold. **Callers own retries**: browsing uses a short interactive policy and the engine its own, so 429/503 reach the engine's adaptive limiter. The UI stays a thin binding over plain view structs. The drawer receives engine snapshots at most 10 times a second.

**Tech Stack:** I1 stack (Rust stable, edition 2024, MSVC · Slint 1.18 cupertino/software · tokio · reqwest rustls · rusqlite bundled · keyring 3 · windows-sys) plus `hmac`, `md-5`, `base64`, `quick-xml` (serde), `url`, `mime_guess`, `serde`/`serde_json` (settings, secrets), `rfd` (file/folder pickers), `windows` (taskbar progress, toast notifications, clipboard). Dev: wiremock, tempfile, i-slint-backend-testing, dotenvy, tokio `test-util` (paused clock).

**Spec:** `docs/superpowers/specs/2026-10-29-httporter-design.md`. This plan implements:
- §15 tracks T3 (S3) and T4 (transfer engine), with the I2 parts of T2, T6 and T7;
- integration milestone **I2**;
- every row marked I2 in `docs/design/mockup-feature-checklist.md`.

The I2 inputs from the I1 final review (Ruling F2, F4, F5 and the "Parked → I2" list) are folded into named tasks below. Visual reference: `docs/design/mockups/` (canvas docs/design/mockups/):
- `Main.dc.html` with `drawer=transfers`, `overlay=conflicts`, `overlay=delete` and `overlay=addsite`;
- `Welcome.dc.html`;
- `Settings.dc.html`.

**Plan series:** plan 2 of 6. Before it: I1 "See it" (merged, `main` @ `3b0cb14`). Next: I3 "Publish".

## Global Constraints

- **Carried over from I1:**
  - Windows 10/11 x64; no WebView.
  - Slint 1.18 with the `cupertino` style and the `software` renderer; Inter font.
  - Colours only from `Theme`; every Slint string in `@tr()`.
- **Rust-built copy:** user-facing strings built in Rust live only in `crates/httporter-app/src/copy.rs` (Task 16).
- **Secrets:**
  - Stored only in Windows Credential Manager:
    - `HTTPorter/site/<site-id>` holds the Bunny zone password, or the S3 key pair as JSON.
    - `HTTPorter/account/<account-id>` holds a kept Bunny account key.
  - Never in SQLite, settings, logs, UI text or test output.
  - Masked in fields, with a per-click reveal that resets when the sheet closes.
- **Concurrency per site:**
  - Uploads + downloads: default **32**, max **96** on Bunny, **128** on S3.
  - Listings: Bunny **5**, S3 **8**.
  - Deletes: Bunny default **16**, cap **30**; S3 batches of up to **1,000** keys.
- **Retries:** waits of **1, 2, 4, 8, 10 s**. The setting offers 5 times (default), 3 times or Never.
  - `Retry-After` is honoured up to **300 s**; interactive listings give up instead of waiting more than **5 s**.
  - Auth failures are never retried and stop that site's transfers.
- **Adaptive concurrency:** on 429, S3 `SlowDown`/503, or Bunny "busy" 400, halve the site's active transfer limit (floor 1). Add 1 back per **20** consecutive successes, up to the configured value. Only when "Slow down when a server asks" is on (default on).
- **S3 multipart:** above **64 MiB**, with **16 MiB** parts and 4 parts in flight per file. Parts are journalled, and the upload is aborted on cancel.
- **Upload integrity:** every upload sends `Content-Type` from the file extension (`mime_guess`, fallback `application/octet-stream`).
  - Bunny also sends `Checksum` (uppercase SHA-256).
  - S3 sends `Content-MD5` (single part) plus `x-amz-meta-httporter-sha256`.
  - On a mismatch: re-hash, retry once, then fail.
- **Progress:** reaches the UI at most **10 Hz**. Lists of 50,000 rows stay smooth (S4).
- **Budgets:**
  - `httporter.exe` ≤ **25 MB**.
  - Cold start ≤ **1 s**.
  - Idle private memory ≤ **40 MB**; ≤ **120 MB** during a 14k-file transfer.
  - Bunny upload of the reference export ≥ **94 files/s** (S1).
- **Release profile:** `lto = "fat"`, `codegen-units = 1`, `strip = true`. **Never `panic = "abort"`**: the app relies on unwinding to contain a failed task.
- **Tests and live data:**
  - Never write to zone `exampleshop`, never purge Pull Zone `7654321`.
  - Live tests only on zones or buckets whose name starts with `httporter-test`.
  - Live credentials come only from the git-ignored `.env.test`, which agents never open or print. Only the test process reads it.
- **Agents never drive the desktop:** no synthetic input, no capture of other windows. `tools/measure.ps1` (HTTPorter's own window) is allowed.
- **Unfinished controls:** a control that isn't wired yet is disabled with a "later version" note, never silently inert.
- **Deviations from the mockups in I2, with reasons:**
  - The Delete alert's "Also purge these URLs from the CDN" checkbox is disabled with the note "CDN purge arrives with Publish." until I3.
  - The collapsed drawer says "Last transfer …" instead of "Last publish …" until I3.
  - The status bar shows the negotiated protocol (Bunny: HTTP/1.1) without the TLS version, which reqwest doesn't expose.
  - The title bar and app icon stay as they are until I5.
- **Commits:** Conventional Commits, ending with the session's attribution trailer.

## Review Focus

- **The user's real job:** a 13,793-file, 865 MB folder dropped on the cloud pane.
  - Expected: throughput ≥ 94 files/s on Bunny, the window stays responsive, private memory ≤ 120 MB, and the drawer shows steady numbers.
  - Tests: Task 12 `twenty_thousand_jobs_stay_cheap`, Task 18 `drawer_view_handles_50k_rows`, Task 26 S1/S7.
- **The PC sleeps, Wi-Fi drops, or the app is killed mid-batch.**
  - Expected: the queue pauses with a banner and continues on its own; no item is marked failed because of the outage. After a crash, HTTPorter offers to resume exactly the unfinished items and doesn't re-send finished ones.
  - Tests: Task 14 `offline_pauses_and_resumes_without_failures`, `crash_then_resume_sends_only_unfinished`.
- **The server pushes back** (Bunny busy 400, 429, S3 `SlowDown`/503).
  - Expected: concurrency halves and recovers, and items succeed instead of piling up in Failed.
  - Tests: Task 12 `slow_down_halves_and_recovers`, `busy_400_counts_as_slow_down`.
- **Wrong or revoked credentials mid-batch.**
  - Expected: one clear message and that site's transfers paused, not 13,793 failures or a retry storm (S9).
  - Test: Task 12 `auth_failure_pauses_site_without_failing_items`.
- **Awkward names:** Unicode, spaces, `#%?+`. Downloads of `CON`, `a:b`, `name.` or case-only duplicates.
  - Expected: correct remote encoding, and safe local names with a note.
  - Tests: Task 8 `canonical_uri_encodes_each_segment_once`, Task 13 `safe_local_names_for_windows`, `case_only_duplicates_get_distinct_names`.

## Execution order and parallel tracks

1. **Tasks 1 → 2 → 3, sequential.** These are the build settings and the contracts every track uses.
2. **Six tracks in parallel**, each in its own worktree, and each touching only its own files:
   - Track Platform: Task 4 → Task 15 (`crates/httporter-platform`)
   - Track Bunny: Task 5 → Task 6 (`crates/httporter-bunny`); Task 7 when the user's test zone exists
   - Track S3: Task 8 → Task 9 → Task 10 (`crates/httporter-s3`); Task 11 when the user's R2 test bucket exists
   - Track Engine: Task 12 → Task 13 → Task 14 (`crates/httporter-transfer`)
   - Track UI-A: Task 16 → Task 17 → Task 22 (`crates/httporter-app`: panes, selection, drag and drop)
   - Track UI-B: starts after Task 16 lands: Task 18 → Task 19 → Task 20 → Task 21 (`crates/httporter-app`: drawer, sheets, settings, Add Site)
   - UI-A and UI-B both add properties to `app.slint`. They merge in order, UI-A first, and conflicts there are expected and small.
3. **Task 23** (transfers on Bunny) after Platform, Bunny, Engine, UI-A and UI-B have merged. Then the **user checkpoint** on the test zone.
4. **Task 24** (sites: S3, account key, import, Welcome, edit/remove) after S3 has merged. Then **Task 25** (browse polish), then **Task 26** (acceptance).
5. Tasks 7 and 11 are gated on the user's test zone and bucket. They never block another task; when they arrive, their findings are applied as small follow-ups.

## File structure

```
crates/
├─ httporter-core/src/
│   ├─ error.rs        ProviderError (Network, Server, Auth{hint}, ClockSkew…), AuthHint
│   ├─ retry.rs        RetryPolicy {delays, max_retry_after}, interactive(), with_retries(n), MAX_RETRY_AFTER
│   ├─ provider.rs     + BodyMeter, PartJournal, MultipartProgress, UploadOptions{md5, meter, parts}, create_folder, delete_batch, reachable
│   ├─ walk.rs         walk(provider, root) -> every entry below root
│   ├─ mime.rs         content_type_for(name)
│   ├─ site.rs         ProviderKind::{Bunny,S3}, S3Preset, S3Site, SiteRecord{container, s3}, AccountId
│   ├─ settings.rs     TransferSettings, HttpProtocol, ProxyMode, ConflictPolicy
│   ├─ transfer.rs     JobId, Job, JobKind, Direction, EngineState, JobView, EngineSnapshot, BatchSummary, Conflict, CompareBy, Resolution, DeleteSummary
│   └─ journal.rs      JobState, JobRecord, Journal, JournalError, MemoryJournal
├─ httporter-platform/src/
│   ├─ store.rs        migration v2, WAL, settings, accounts, list_sites -> SiteList, update_site
│   ├─ journal.rs      SqliteJournal (implements core::Journal)
│   ├─ creds.rs        + CredentialKey::account, SiteSecret
│   └─ win/            taskbar.rs, notify.rs, pickers.rs, clipboard.rs (Task 15)
├─ httporter-bunny/src/  client.rs, list.rs, transfer.rs (split of storage.rs), account.rs
├─ httporter-s3/src/     sigv4.rs, presets.rs, xml.rs, storage.rs, multipart.rs, errors.rs
├─ httporter-transfer/src/ engine.rs, lane.rs, limiter.rs, rate.rs, bucket.rs, hash.rs, plan.rs, names.rs, offline.rs
└─ httporter-app/
    ├─ src/  session/{mod,sites,remote,local,fakes}.rs, site_hub.rs, copy.rs, selection.rs,
    │        drawer_view.rs, sheets_view.rs, settings_view.rs, import.rs, transfers.rs, (existing files)
    └─ ui/components/ drawer.slint, sheets.slint, settings_window.slint, form_controls.slint,
                      provider_grid.slint, welcome.slint, (existing files)
docs/acceptance/I2.md · docs/notes/bunny-behavior.md · docs/notes/s3-presets.md
tools/check.ps1 · deny.toml · .github/workflows/ci.yml
```

---

### Task 1: Build hygiene and release size

**Files:**
- Modify: `Cargo.toml` (workspace), and every crate `Cargo.toml`.
- Create: `tools/check.ps1`, `deny.toml`, `.github/workflows/ci.yml`.

**Interfaces:**
- Produces: `[workspace.dependencies]`, which every later task uses through `dep.workspace = true`, plus `tools/check.ps1`, which every task runs before committing.

- [ ] **Step 1: Move shared versions into `[workspace.dependencies]`.** Cover tokio, reqwest, serde, serde_json, chrono, thiserror, async-trait, bytes, futures-util, slint, keyring, rusqlite, uuid, windows-sys, wiremock and tempfile. Pin keyring to an exact `3.x.y` (ledger 91). Members use `x.workspace = true`.
- [ ] **Step 2: Add the release profile:** `[profile.release] lto = "fat"`, `codegen-units = 1`, `strip = true`. Do not set `panic`.
- [ ] **Step 3: Write `tools/check.ps1`.** It runs `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and `cargo deny check licenses bans`, and exits non-zero on the first failure.
- [ ] **Step 4: Write `deny.toml`.**
  - Allowed licences: MIT, Apache-2.0, Apache-2.0 WITH LLVM-exception, ISC, BSD-2-Clause, BSD-3-Clause, Unicode-3.0, Zlib, CDLA-Permissive-2.0, OFL-1.1.
  - Exception: `GPL-3.0-only` for `slint` and `i-slint-*` only (Slint's GPL option; spec §16).
- [ ] **Step 5: Write `.github/workflows/ci.yml`** (windows-latest, stable, same steps as check.ps1). It is inert until the repo has a remote; note that in a comment at the top.
- [ ] **Step 6: Run** `cargo install cargo-deny --locked` if missing, then `powershell -File tools/check.ps1`. Expected: all green, the same 375 tests.
- [ ] **Step 7: Measure** `cargo build --release -p httporter-app` and `(Get-Item target/release/httporter.exe).Length`. Record the size in the commit message; expected well under 24,491,008 bytes.
- [ ] **Step 8: Commit** `build: workspace dependencies, release profile, check script and licence policy`.

---

### Task 2: Core contracts, round 2, errors, retries, provider trait, walk, mock

**Files:**
- Modify: `crates/httporter-core/src/{error.rs, retry.rs, provider.rs, entry.rs, path.rs, mock.rs, lib.rs}`.
- Create: `crates/httporter-core/src/{walk.rs, mime.rs}`.
- Fix callers in `crates/httporter-bunny`, and in `crates/httporter-app/src` (`session.rs` and wherever errors are described), so the workspace builds.

**Interfaces:**
- Produces:

```rust
pub enum AuthHint { Unknown, AccessKey, Secret, Region }
pub enum ProviderError {
    Auth { hint: AuthHint },
    NotFound,
    RateLimited { retry_after: Option<Duration> },
    ChecksumMismatch,
    Refused(String),
    /// No answer: DNS, connect, TLS, reset, timeout.
    Network(String),
    /// The server answered with a temporary failure (5xx, Bunny "busy" 400).
    Server { status: u16, detail: String },
    /// The PC clock is too far off (S3 RequestTimeTooSkewed).
    ClockSkew,
    Permanent(String),
}
impl ProviderError {
    pub fn is_retryable(&self) -> bool;   // Network | Server | RateLimited
    pub fn is_slow_down(&self) -> bool;   // RateLimited | Server{status: 503} | Server{status: 400}
}

pub const MAX_RETRY_AFTER: Duration;                       // 300 s
pub struct RetryPolicy { pub delays: Vec<Duration>, pub max_retry_after: Duration }
impl RetryPolicy {
    pub fn immediate(retries: usize) -> Self;              // max_retry_after = MAX_RETRY_AFTER
    pub fn with_retries(n: u8) -> Self;                    // first n of [1,2,4,8,10] s
    pub fn interactive() -> Self;                          // delays [1,2] s, max_retry_after 5 s
}
// with_retry: a RateLimited whose retry_after exceeds max_retry_after is returned at once (F4).

#[async_trait] pub trait BodyMeter: Send + Sync { async fn chunk(&self, len: usize); }
pub struct MultipartProgress { pub upload_id: String, pub part_size: u64, pub parts: Vec<(u32, String)> }
pub trait PartJournal: Send + Sync {
    fn load(&self) -> Option<MultipartProgress>;
    fn started(&self, upload_id: &str, part_size: u64);
    fn part_done(&self, number: u32, etag: &str);
    fn finished(&self);
    fn cancelled(&self) -> bool;
}
pub struct UploadOptions {
    pub content_type: String,
    pub sha256: Option<[u8; 32]>,
    pub md5: Option<[u8; 16]>,
    pub meter: Option<Arc<dyn BodyMeter>>,
    pub parts: Option<Arc<dyn PartJournal>>,
}
impl UploadOptions { pub fn new(content_type: impl Into<String>) -> Self; }
// Debug prints meter/parts as "set"/"none"; PartialEq compares them with Arc::ptr_eq.

// StorageProvider gains:
async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError>;
async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>>; // default: delete_file one by one
async fn reachable(&self) -> Result<(), ProviderError>;                               // default: list(root)

pub async fn walk(provider: &dyn StorageProvider, root: &RemotePath) -> Result<Vec<Entry>, ProviderError>;
pub fn content_type_for(name: &str) -> String;
impl RemotePath { pub fn is_within(&self, dir: &RemotePath) -> bool; }  // self == dir or a descendant
pub enum ChecksumKind { Sha256, Md5, MultipartEtag }
```

- [ ] **Step 1: Write the failing tests.**

```rust
// error.rs
#[test] fn retryable_and_slow_down_sets() {
    assert!(ProviderError::Network("x".into()).is_retryable());
    assert!(ProviderError::Server { status: 502, detail: "".into() }.is_retryable());
    assert!(ProviderError::Server { status: 503, detail: "".into() }.is_slow_down());
    assert!(!ProviderError::Server { status: 500, detail: "".into() }.is_slow_down());
    assert!(!ProviderError::Auth { hint: AuthHint::Unknown }.is_retryable());
    assert!(!ProviderError::ClockSkew.is_retryable());
}
// retry.rs (tokio paused clock)
#[tokio::test(start_paused = true)] async fn interactive_gives_up_on_long_retry_after() {
    // op returns RateLimited{Some(60 s)} → with_retry(&RetryPolicy::interactive()) returns it after 1 call, ~0 s elapsed
}
#[test] fn with_retries_takes_the_first_n_delays() {
    assert_eq!(RetryPolicy::with_retries(3).delays, [1,2,4].map(Duration::from_secs).to_vec());
    assert!(RetryPolicy::with_retries(0).delays.is_empty());
}
#[tokio::test(start_paused = true)] async fn retry_after_shorter_than_delay_waits_the_delay() {} // ledger 117
// path.rs
#[test] fn is_within_needs_a_separator_boundary() {
    let d = RemotePath::parse("a/b").unwrap();
    assert!(RemotePath::parse("a/b/c").unwrap().is_within(&d));
    assert!(d.is_within(&d));
    assert!(!RemotePath::parse("a/bc").unwrap().is_within(&d));     // ledger 63
}
// mime.rs
#[test] fn content_types() {
    assert_eq!(content_type_for("index.html"), "text/html");
    assert_eq!(content_type_for("a.webp"), "image/webp");
    assert_eq!(content_type_for("noext"), "application/octet-stream");
}
// walk.rs (MockProvider)
#[tokio::test] async fn walk_returns_every_entry_below_root_once() {} // 3 levels, 7 files, 3 folders; root excluded
#[tokio::test] async fn walk_respects_list_concurrency() {}           // mock max_in_flight(List) <= capabilities.list_concurrency
// mock.rs
#[tokio::test] async fn mock_refuses_recursive_delete_without_capability() {}  // ledger 75
#[tokio::test] async fn mock_rejects_file_source_with_wrong_len() {}          // ledger 77 → Permanent
#[tokio::test] async fn mock_meters_upload_chunks() {}                        // sum of chunk lens == file len
#[tokio::test] async fn mock_create_folder_follows_empty_folders() {}         // Unknown/Unsupported → Refused; Markers/Supported → Ok, listed
```

- [ ] **Step 2: Run** `cargo test -p httporter-core`. Expected: FAIL (the new items don't exist).
- [ ] **Step 3: Implement the core items above.**
  - Document the trait's error rules in `provider.rs` (ledger 74):
    - a missing folder or file → `NotFound`;
    - file/folder collisions → `Refused`;
    - upload overwrites;
    - `delete_*` of a missing path → `NotFound`.
  - `walk` uses `FuturesUnordered`, bounded by `capabilities().list_concurrency`.
  - MockProvider gains `set_latency(op, Duration)`, `max_in_flight(op) -> usize` and `contents(path) -> Option<Bytes>`, plus enforcement of `recursive_delete`, `empty_folders`, the file `len` check, and meter calls per 64 KiB.
- [ ] **Step 4: Update callers.**
  - Bunny maps connect/timeout → `Network`, 5xx → `Server`, busy-400 → `Server{status:400, detail:"busy"}`, 401/403 → `Auth{hint: Unknown}`.
  - App copy matches the new variants. The I1 wording stays identical for Auth, NotFound and Network. `Server` now says "{host} had a problem (HTTP {status}). Try again in a moment." instead of "Couldn't reach" (parked item).
- [ ] **Step 5: Run** `powershell -File tools/check.ps1`. Expected: PASS.
- [ ] **Step 6: Commit** `feat(core): refined errors, caller-owned retries, upload metering, walk`.

---

### Task 3: Core contracts, round 2, sites, settings, transfer view types, journal

**Files:**
- Modify: `crates/httporter-core/src/{site.rs, lib.rs}`, `crates/httporter-core/Cargo.toml` (serde).
- Create: `crates/httporter-core/src/{settings.rs, transfer.rs, journal.rs}`.
- Fix callers in platform (store field rename) and app.

**Interfaces:**
- Produces:

```rust
pub enum ProviderKind { Bunny, S3 }
pub enum S3Preset { AwsS3, CloudflareR2, BackblazeB2, Wasabi, DigitalOceanSpaces, Minio, Other }
impl S3Preset { pub fn label(self) -> &'static str;  // "Amazon S3", "Cloudflare R2", "Backblaze B2", "Wasabi", "DigitalOcean Spaces", "MinIO", "Other S3"
                pub fn short(self) -> &'static str;  // "S3", "R2", "B2", "Wasabi", "Spaces", "MinIO", "S3"  (conflict sheet "On {short}")
                pub fn mono(self) -> &'static str; } // "S3", "R2", "B2", "W", "DO", "M", "…"
pub struct S3Site { pub preset: S3Preset, pub endpoint: String /* scheme://host[:port] */, pub path_style: bool }
pub struct SiteRecord { pub id: SiteId, pub name: String, pub provider: ProviderKind,
    pub container: String /* Bunny zone or S3 bucket; was `zone` */, pub region: String,
    pub host: String, pub s3: Option<S3Site>, pub created_at: DateTime<Utc> }
pub struct AccountId(pub String);   // like SiteId

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TransferSettings {
    pub simultaneous: u32,            // 32
    pub adaptive: bool,               // true
    pub retries: u8,                  // 5 (UI: 5 | 3 | 0)
    pub bandwidth_limit: Option<u64>, // bytes/s; None = Off
    pub protocol: HttpProtocol,       // Http2WhenAvailable | Http1Only
    pub keep_alive: bool,             // true
    pub request_timeout_secs: u32,    // 120 (UI: 120 | 60 | 300)
    pub proxy: ProxyMode,             // System | None | Custom(String)
    pub verify_checksums: bool,       // true
    pub upload_conflicts: ConflictPolicy,   // Ask
    pub download_conflicts: ConflictPolicy, // Ask
}
impl Default for TransferSettings;  impl TransferSettings { pub fn retry_policy(&self) -> RetryPolicy; pub fn transfer_limit(&self, cap: usize) -> usize; }
pub enum ConflictPolicy { Ask, Replace, ReplaceIfDifferent, Skip, KeepBoth }

// transfer.rs
pub struct JobId(pub u64);
pub enum Job { Upload { local: PathBuf, remote: RemotePath, size: u64 },
               Download { remote: RemotePath, local: PathBuf, size: u64 },
               Delete { remote: RemotePath, folder: bool },
               CreateFolder { remote: RemotePath } }
pub enum JobKind { Upload, Download, Delete, CreateFolder }
pub enum Direction { Upload, Download, Delete, Mixed }
pub enum EngineState { Idle, Running, Paused, Offline { site: String }, AuthStopped { site: String },
                       DiskFull { drive: String, needed: u64 } }
pub struct JobView { pub id: JobId, pub kind: JobKind, pub path: String, pub size: u64, pub done_bytes: u64, pub error: Option<String> }
pub struct BatchSummary { pub finished_at: DateTime<Utc>, pub files: u64, pub bytes: u64, pub direction: Direction, pub site: String }
pub struct EngineSnapshot { pub state: EngineState, pub direction: Direction, pub site: String, pub host: String,
    pub files_total: u64, pub files_done: u64, pub files_failed: u64, pub bytes_total: u64, pub bytes_done: u64,
    pub files_per_s: f64, pub bytes_per_s: f64, pub eta: Option<Duration>,
    pub active: Vec<JobView>, pub queued: Vec<JobView> /* first 200 */, pub queued_count: u64,
    pub failed: Vec<JobView>, pub done: Vec<JobView> /* last 200 */, pub done_count: u64,
    pub last_batch: Option<BatchSummary> }
pub enum CompareBy { Sha256, Md5, Size }
pub enum Resolution { Replace, ReplaceIfDifferent, Skip, KeepBoth }
pub struct Conflict { pub index: usize, pub name: String, pub local: PathBuf, pub remote: RemotePath,
    pub local_size: u64, pub local_modified: Option<DateTime<Utc>>,
    pub remote_size: u64, pub remote_modified: Option<DateTime<Utc>>,
    pub same: Option<bool>, pub compared_by: CompareBy }
pub struct DeleteSummary { pub folders: u64, pub files: u64, pub total_files: u64, pub total_bytes: u64,
    pub single: Option<(String, bool /* is_folder */)> }

// journal.rs
pub enum JobState { Queued, Done, Failed, Cancelled }
pub struct JobRecord { pub id: i64, pub batch: String, pub site: SiteId, pub job: Job, pub state: JobState,
    pub error: Option<String>, pub multipart: Option<MultipartProgress> }
#[derive(Debug, thiserror::Error)] #[error("journal: {0}")] pub struct JournalError(pub String);
pub trait Journal: Send + Sync {
    fn insert(&self, records: &mut [JobRecord]) -> Result<(), JournalError>;   // one transaction, assigns ids
    fn set_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<(), JournalError>;
    fn set_multipart(&self, id: i64, mp: Option<&MultipartProgress>) -> Result<(), JournalError>;
    fn unfinished(&self) -> Result<Vec<JobRecord>, JournalError>;              // Queued + Failed
    fn clear_finished(&self) -> Result<(), JournalError>;                      // drops Done + Cancelled
    fn discard_unfinished(&self) -> Result<(), JournalError>;
}
pub struct MemoryJournal;  // impl Journal, for tests
```

- [ ] **Step 1: Write the failing tests.**
  - `transfer_settings_defaults_match_the_mockup`: every default above.
  - `retry_policy_follows_retries`: `retries: 3` → delays 1, 2, 4; `0` → empty.
  - `transfer_limit_clamps`: `simultaneous 200` → `transfer_limit(96) == 96`; `0` → 1.
  - `settings_round_trip_json`.
  - `memory_journal_insert_assigns_ids_and_unfinished_lists_queued_and_failed`.
  - `clear_finished_keeps_unfinished`.
  - `preset_labels_and_monos`: matches the mockup's provider list.
- [ ] **Step 2: Run** `cargo test -p httporter-core`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Rename `SiteRecord.zone` → `container` across the workspace with compiler guidance. The I1 behaviour is unchanged.
  - Fix `SiteItemData.provider` (ledger 129): sites carry their provider so tiles pick colour and mono from it, not from the label.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(core): S3 sites, transfer settings, transfer view types and journal contract`.

---

### Task 4: Store v2, journal and credentials v2 *(Track Platform)*

**Files:**
- Modify: `crates/httporter-platform/src/{store.rs, creds.rs, lib.rs}`.
- Create: `crates/httporter-platform/src/journal.rs`.

**Interfaces:**
- Consumes: Task 3 types.
- Produces:

```rust
pub struct SiteList { pub sites: Vec<SiteRecord>, pub unreadable: usize }
impl Store {
    pub fn list_sites(&self) -> Result<SiteList, StoreError>;       // skips undecodable rows (ledger 111)
    pub fn update_site(&self, site: &SiteRecord) -> Result<(), StoreError>;  // StoreError::NotFound if absent
    pub fn settings<T: DeserializeOwned + Default>(&self, key: &str) -> T;   // missing/corrupt → Default
    pub fn put_settings<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StoreError>;
    pub fn add_account(&self, a: &AccountRecord) -> Result<(), StoreError>;
    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, StoreError>;
    pub fn remove_account(&self, id: &AccountId) -> Result<(), StoreError>;
}
pub struct AccountRecord { pub id: AccountId, pub provider: ProviderKind, pub name: String, pub created_at: DateTime<Utc> }
pub const SETTINGS_TRANSFERS: &str = "transfers";
pub fn site_conflicts_key(id: &SiteId, upload: bool) -> String;   // "conflicts/upload/<id>" | "conflicts/download/<id>"
pub struct SqliteJournal;  impl SqliteJournal { pub fn open(path: &Path) -> Result<Self, StoreError>; pub fn open_in_memory() -> Result<Self, StoreError>; }
impl Journal for SqliteJournal {}
impl CredentialKey { pub fn account(id: &AccountId) -> Self; }   // "HTTPorter/account/<id>"
pub enum SiteSecret { BunnyPassword(String), S3 { access_key_id: String, secret_access_key: String } }
impl SiteSecret { pub fn to_stored(&self) -> String; pub fn from_stored(kind: ProviderKind, text: &str) -> Result<Self, CredError>; }
// Debug redacts every secret field.
```

- [ ] **Step 1: Write the failing tests.**
  - `migration_v1_to_v2_keeps_sites`: build a v1 DB with the I1 schema, open it → same sites; `container` = the old zone; `s3` = None.
  - `opens_with_wal_and_busy_timeout`: `PRAGMA journal_mode` = `wal`; `busy_timeout` = 5000.
  - `migration_runs_in_immediate_transaction` (ledger 109): two `Store::open` on one file from two threads → both succeed, `user_version` = 2.
  - `no_secret_columns_exact` (ledger 108): asserts the exact column list of `sites`, `accounts`, `settings` and `journal`.
  - `undecodable_row_is_skipped_and_counted`.
  - `update_site_changes_every_field`, `update_unknown_site_is_not_found`.
  - `settings_default_when_missing_or_corrupt`, `settings_round_trip`.
  - `s3_site_round_trips`.
  - `sqlite_journal_survives_reopen`: insert 3, mark 1 Done, reopen → `unfinished().len() == 2`.
  - `journal_insert_10k_in_one_transaction_is_fast`: < 2 s in debug.
  - `multipart_progress_round_trips`.
  - `site_secret_round_trip_and_debug_redacts`: Debug output contains neither the key nor the secret.
  - `deleting_a_missing_credential_is_ok` (ledger 89).
- [ ] **Step 2: Run** `cargo test -p httporter-platform`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Migration 2 (inside `BEGIN IMMEDIATE`):
    - `ALTER TABLE sites RENAME COLUMN zone TO container`;
    - add `s3_preset TEXT`, `s3_endpoint TEXT`, `s3_path_style INTEGER`;
    - `CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL)`;
    - `CREATE TABLE accounts(id, provider, name, created_at)`;
    - `CREATE TABLE journal(id INTEGER PRIMARY KEY, batch, site, kind, local, remote, size, folder, state, error, multipart, updated_at)`, plus an index on `state`.
  - The journal connection is separate (WAL lets the store and journal share the file).
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(platform): store v2 with settings, accounts and transfer journal`.

---

### Task 5: Bunny client, round 2 *(Track Bunny)*

**Files:**
- Split `crates/httporter-bunny/src/storage.rs` into `client.rs` (config, client build, error mapping), `list.rs` and `transfer.rs` (ledger 183).
- Modify `tests/storage_list.rs` and `tests/storage_ops.rs`.

**Interfaces:**
- Consumes: Task 2 trait, Task 3 `TransferSettings`.
- Produces:

```rust
impl BunnyConfig {
    /// The one client a site uses for browsing and transfers: single attempt per call (callers own retries),
    /// listings time out after 20 s, transfers after `transfer_idle_timeout` of no progress.
    pub fn for_site(zone: impl Into<String>, host: impl Into<String>, password: impl Into<String>, s: &TransferSettings) -> Self;
}
// new BunnyConfig fields: http1_only: bool, keep_alive: bool, request_timeout: Duration, proxy: ProxyMode
// BunnyStorage implements create_folder (Refused("Bunny Storage creates a folder with its first file.") while
// EmptyFolders::Unknown, Task 7 may change this), reachable (GET https://{host}/ without AccessKey; any HTTP status = Ok),
// and calls opts.meter before sending each body chunk.
```

- [ ] **Step 1: Write the failing tests (wiremock, paused clock where timing matters).**
  - `for_site_is_single_attempt`: 503 once → `Err(Server{503})` after exactly 1 request.
  - `busy_400_maps_to_server_400`.
  - `error_bodies_are_read_so_the_connection_is_reused` (ledger 143): three 503s then 200 on one keep-alive connection → the mock sees 1 connection (assert via `wiremock` request headers or a counting connector).
  - `meter_sees_every_upload_byte`.
  - `idle_timeout_is_clamped_to_five_seconds_minimum` (ledger 181).
  - `slow_tail_is_not_cut_off` (ledger 180): a body trickling 1 KiB/s for its last 8 KiB with a 5 s idle timeout succeeds.
  - `http1_only_setting_disables_h2`.
  - `keep_alive_off_uses_a_fresh_connection_per_request`.
  - `proxy_none_and_custom_are_honoured` (pure builder test).
  - `reachable_ok_on_any_status`, `reachable_network_error_offline`.
  - `create_folder_refused_while_unknown`.
  - `retry_after_capped_at_300s`, still pinned after the split.
- [ ] **Step 2: Run** `cargo test -p httporter-bunny`. Expected: FAIL.
- [ ] **Step 3: Implement** the split and the behaviours above.
  - Replace wall-clock sleeps in tests with `tokio::time::pause()` (ledger 183, 229).
  - Clear the minors in touched code: stale `base_url` doc; snippet strips control characters (ledger 187).
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(bunny): one client per site with caller-owned retries, metering and settings`.

---

### Task 6: Bunny account API *(Track Bunny)*

**Files:**
- Create: `crates/httporter-bunny/src/account.rs`, `crates/httporter-bunny/tests/account.rs`.

**Interfaces:**
- Produces:

```rust
pub struct BunnyAccount;
impl BunnyAccount {
    pub fn new(api_key: String) -> Result<Self, ProviderError>;
    pub fn with_base_url(api_key: String, base_url: String) -> Result<Self, ProviderError>; // tests
    pub async fn list_zones(&self) -> Result<Vec<ZoneInfo>, ProviderError>;
}
pub struct ZoneInfo { pub id: u64, pub name: String, pub region: String /* lowercase code */,
    pub host: String, pub password: String, pub read_only_password: String, pub pull_zones: Vec<PullZoneInfo> }
pub struct PullZoneInfo { pub id: u64, pub name: String, pub hostnames: Vec<String> }
// Debug for BunnyAccount and ZoneInfo redacts the key and both passwords.
```

- [ ] **Step 1: Write the failing tests (wiremock, JSON shaped like spec §5.2).**
  - `lists_zones_with_pull_zone_hostnames`:
    - `GET /storagezone?page=0` (plain array) and `GET /pullzone`;
    - pull zone matched by `OriginType == 2 && StorageZoneId == zone.Id`;
    - hostnames in API order;
    - `region` `"SG"` → `"sg"`;
    - `host` = `StorageHostname`, or `storage_host(region)` when empty.
  - `account_key_sent_as_accesskey_header_only_to_api_host`.
  - `wrong_key_is_auth_unknown`.
  - `debug_never_shows_secrets`.
- [ ] **Step 2: Run** the account tests. Expected: FAIL.
- [ ] **Step 3: Implement** with its own reqwest client (redirects off, sensitive header, 20 s timeout).
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(bunny): account API zone discovery`.

---

### Task 7: Live Bunny probe *(Track Bunny, gated on the user's test zone)*

This is the I1 plan's Task 7, carried over (Ruling P5). It runs only when `D:\Projects\httporter\.env.test` exists. Agents check existence with `Test-Path` and never open or print the file. Variables: `HTTPORTER_TEST_ZONE` (must start with `httporter-test`), `HTTPORTER_TEST_REGION`, `HTTPORTER_TEST_PASSWORD`.

**Files:**
- Create: `crates/httporter-bunny/tests/live.rs` (`#[ignore]`, loads `.env.test` with dotenvy, asserts the zone prefix before any request), `docs/notes/bunny-behavior.md`.

- [ ] **Step 1:** Write the live tests. Each records its observation in the test output, never the password:
  - `b1_put_folder_with_trailing_slash`: does `PUT /{zone}/{dir}/` with an empty body create an empty folder?
  - `b2_case_sensitivity`;
  - `missing_folder_404_or_empty_array`;
  - `busy_400_body_text`;
  - `checksum_mismatch_body_text`;
  - `last_changed_is_utc`;
  - `encoded_parentheses_and_plus_accepted`.
- [ ] **Step 2: Run** `cargo test -p httporter-bunny --test live -- --ignored --nocapture`. Expected: all run and the zone is clean afterwards.
- [ ] **Step 3:** Write the findings into `docs/notes/bunny-behavior.md`.
  - If B1 works, set `EmptyFolders::Supported` and implement `create_folder` as that PUT (with a wiremock test).
  - If a missing folder lists as `[]`, map it to `NotFound` only where the parent listing lacks it.
  - Update P16's busy-body match if the text differs.
- [ ] **Step 4: Commit** `test(bunny): live behaviour probe and findings`.

---

### Task 8: S3 signing, presets and addressing *(Track S3)*

**Files:**
- Create the crate `crates/httporter-s3` (`Cargo.toml`, `src/lib.rs`, `src/sigv4.rs`, `src/presets.rs`) and `docs/notes/s3-presets.md`.

**Interfaces:**
- Produces:

```rust
pub struct Credentials { pub access_key_id: String, pub secret_access_key: String, pub session_token: Option<String> } // Debug redacts
pub struct SignInput<'a> { pub method: &'a str, pub url: &'a Url, pub headers: &'a [(String, String)],
                           pub payload_sha256: &'a str /* hex or "UNSIGNED-PAYLOAD" */ }
/// Returns the headers to add: authorization, x-amz-date, x-amz-content-sha256 (+ x-amz-security-token).
pub fn sign(input: &SignInput, creds: &Credentials, region: &str, service: &str, now: DateTime<Utc>) -> Vec<(String, String)>;
pub fn canonical_uri(path: &str) -> String;     // each segment percent-encoded once; unreserved = A-Z a-z 0-9 - _ . ~
pub struct Endpoint { pub base: Url, pub path_style: bool, pub region: String }
pub fn endpoint(preset: S3Preset, region: &str, account_id: &str, custom: &str, path_style: bool) -> Result<Endpoint, String>;
pub fn bucket_url(ep: &Endpoint, bucket: &str) -> Url;
pub fn object_url(ep: &Endpoint, bucket: &str, key: &RemotePath) -> Url;
pub const REGIONS_AWS: &[&str]; pub const REGIONS_WASABI: &[&str]; pub const REGIONS_SPACES: &[&str]; pub const REGIONS_B2: &[&str];
```

- [ ] **Step 1: Write the failing tests.**

```rust
#[test] fn aws_iam_example_signature() {
    // AWS SigV4 docs example: GET https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08,
    // AKIDEXAMPLE / wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY, us-east-1, iam, 20150830T123600Z,
    // content-type: application/x-www-form-urlencoded; charset=utf-8, empty payload.
    // expected signature 5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7
}
#[test] fn s3_get_object_example_signature() {
    // S3 docs "GET Object" example: examplebucket.s3.amazonaws.com/test.txt, Range: bytes=0-9,
    // AKIAIOSFODNN7EXAMPLE / wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY, us-east-1, 20130524T000000Z, empty payload.
    // expected signature f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41
}
#[test] fn canonical_uri_encodes_each_segment_once() {
    assert_eq!(canonical_uri("/b/a b/c#d%e?f+g/ü.txt"), "/b/a%20b/c%23d%25e%3Ff%2Bg/%C3%BC.txt");
}
#[test] fn preset_endpoints() {
    // AwsS3 eu-west-1 → https://s3.eu-west-1.amazonaws.com, path_style false (bucket in host)
    // CloudflareR2 account "abc" → https://abc.r2.cloudflarestorage.com, region "auto", path_style true
    // BackblazeB2 us-west-004 → https://s3.us-west-004.backblazeb2.com; Wasabi us-east-1 → https://s3.us-east-1.wasabisys.com
    // DigitalOceanSpaces sgp1 → https://sgp1.digitaloceanspaces.com; Minio "http://localhost:9000" → path_style true
    // Other requires a URL; "ftp://x" → Err("Use an http:// or https:// address.")
}
#[test] fn object_url_virtual_hosted_and_path_style() {}
```

- [ ] **Step 2:** Verify both expected signatures against the two AWS documentation pages before relying on them; fix the constants if a page differs. **Run** `cargo test -p httporter-s3`. Expected: FAIL.
- [ ] **Step 3: Implement** with `hmac` + `sha2`. Defaults:
  - path-style for every preset except AWS (virtual-hosted);
  - `Other` follows the user's toggle.
  - Record each preset's quirks found in its docs (B8) in `docs/notes/s3-presets.md`.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(s3): SigV4 signer, presets and addressing`.

---

### Task 9: S3 provider *(Track S3)*

**Files:**
- Create: `crates/httporter-s3/src/{storage.rs, xml.rs, errors.rs}`, `crates/httporter-s3/tests/storage.rs`.

**Interfaces:**
- Consumes: Task 2 trait, Task 8 signing.
- Produces:

```rust
pub struct S3Config { pub bucket: String, pub endpoint: Endpoint, pub creds: Credentials,
    pub transfer_concurrency: usize, pub list_timeout: Duration, pub transfer_idle_timeout: Duration,
    pub http1_only: bool, pub keep_alive: bool, pub request_timeout: Duration, pub proxy: ProxyMode }
impl S3Config { pub fn for_site(bucket: impl Into<String>, endpoint: Endpoint, creds: Credentials, s: &TransferSettings) -> Self; }
pub struct S3Storage; impl S3Storage { pub fn new(cfg: S3Config) -> Result<Self, ProviderError>;
                                       pub async fn list_buckets(&self) -> Result<Vec<String>, ProviderError>; }
impl StorageProvider for S3Storage {}
// capabilities: checksum_in_listing Some(Md5), server_side_copy true, recursive_delete false, batch_delete_max 1000,
// empty_folders Markers, list_concurrency 8, delete_concurrency 8, transfer_concurrency_max 128, presign false (I4), read_only false
```

- [ ] **Step 1: Write the failing tests** (wiremock checks method, path, query and the presence of a valid-looking `Authorization` header).
  - **Listing:**
    - `list_pages_through_continuation_tokens` (2 pages of 1,000 + 1 → 2,001 entries);
    - `common_prefixes_become_folders_and_own_marker_is_hidden`;
    - `etag_without_dash_is_md5_with_dash_is_multipart`.
  - **Uploads:**
    - `upload_sends_content_type_md5_and_sha256_meta`;
    - `upload_bad_digest_is_checksum_mismatch`.
  - **Deletes:**
    - `delete_batch_posts_one_xml_per_1000_keys_with_content_md5`;
    - `delete_batch_maps_per_key_errors`;
    - `delete_folder_walks_then_batch_deletes_including_marker`.
  - `create_folder_puts_zero_byte_marker`.
  - **Error mapping:**
    - `auth_errors_name_the_field`: `InvalidAccessKeyId`→`AccessKey`, `SignatureDoesNotMatch`→`Secret`, `AuthorizationHeaderMalformed`→`Region`, `AccessDenied`→`Unknown`;
    - `request_time_too_skewed_is_clock_skew`;
    - `slow_down_and_503_are_rate_limited`, with `Retry-After` capped at 300 s (ledger 120);
    - `no_such_bucket_is_not_found`.
  - `list_buckets_parses_names`.
  - `download_streams_body`.
- [ ] **Step 2: Run** `cargo test -p httporter-s3`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Uploads sign `UNSIGNED-PAYLOAD` and send `Content-MD5` when `opts.md5` is set. Other requests sign the real payload hash.
  - Body chunks go through `opts.meter`.
  - Read error bodies (≤ 16 KiB) to parse `<Code>`.
  - Parse with `quick-xml` + serde.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(s3): S3-compatible storage provider`.

---

### Task 10: S3 multipart uploads *(Track S3)*

**Files:**
- Create: `crates/httporter-s3/src/multipart.rs`, `crates/httporter-s3/tests/multipart.rs`.

**Interfaces:**
- Consumes: `UploadOptions.parts: Option<Arc<dyn PartJournal>>` (Task 2).
- Produces: `S3Storage::upload` switches to multipart for `UploadSource::File` with `len > 64 MiB`; no public API change. `pub const MULTIPART_THRESHOLD: u64 = 64 << 20; pub const PART_SIZE: u64 = 16 << 20;`

- [ ] **Step 1: Write the failing tests** (wiremock; a `RecordingPartJournal` test double).
  - `large_file_uses_create_parts_complete`: 70 MiB → 5 parts (4 × 16 + 6), each with `Content-MD5`; Complete lists them in order; journal `started` then `part_done` ×5 then `finished`.
  - `at_most_four_parts_in_flight`.
  - `resume_skips_recorded_parts`: journal loads `upload_id` + parts 1–3 → only parts 4–5 are sent.
  - `no_such_upload_on_resume_starts_over`.
  - `cancelled_aborts_the_upload` (`DELETE ?uploadId=`).
  - `dropped_without_cancel_keeps_the_upload` (no abort request).
  - `sha256_meta_set_on_create`.
- [ ] **Step 2: Run** the multipart tests. Expected: FAIL.
- [ ] **Step 3: Implement** with ranged file reads per part (no whole-file buffer); metering per part chunk.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(s3): resumable multipart uploads`.

---

### Task 11: Live S3 probe on R2 *(Track S3, gated on the user's test bucket)*

Runs only when `.env.test` also holds:
- `HTTPORTER_TEST_S3_PRESET`
- `HTTPORTER_TEST_S3_ACCOUNT_OR_ENDPOINT`
- `HTTPORTER_TEST_S3_REGION`
- `HTTPORTER_TEST_S3_BUCKET` (must start with `httporter-test`)
- `HTTPORTER_TEST_S3_KEY_ID`
- `HTTPORTER_TEST_S3_SECRET`

The same rules as Task 7 apply. MinIO is optional; ask the user before downloading its binary.

**Files:**
- Create: `crates/httporter-s3/tests/live.rs` (`#[ignore]`).
- Append: `docs/notes/s3-presets.md`.

- [ ] **Step 1:** Write the live tests:
  - `round_trip_small_and_multipart`;
  - `content_type_served_for_html_css_js_webp`;
  - `delete_batch_and_marker_folders`;
  - `list_buckets_permission`.
- [ ] **Step 2: Run** `cargo test -p httporter-s3 --test live -- --ignored --nocapture`. Expected: PASS, and the bucket is left clean.
- [ ] **Step 3: Commit** `test(s3): live probe on R2`.

---

### Task 12: Transfer engine core *(Track Engine)*

**Files:**
- Create the crate `crates/httporter-transfer`: `src/{lib.rs, engine.rs, lane.rs, limiter.rs, rate.rs, bucket.rs, hash.rs}`, `tests/engine.rs`.

**Interfaces:**
- Consumes: Tasks 2–3 (`StorageProvider`, `MockProvider`, `Journal`, `MemoryJournal`, view types, `TransferSettings`).
- Produces:

```rust
pub struct Batch { pub site: SiteId, pub site_name: String, pub host: String,
                   pub provider: Arc<dyn StorageProvider>, pub jobs: Vec<Job> }
#[derive(Clone)] pub struct Engine;
impl Engine {
    /// `describe` turns an error into the plain words shown in the Failed tab (the app passes copy::describe),
    /// so the engine holds no UI wording.
    pub fn new(rt: tokio::runtime::Handle, journal: Arc<dyn Journal>, settings: TransferSettings,
               describe: fn(&ProviderError) -> String) -> Self;
    pub fn set_settings(&self, s: TransferSettings);
    pub fn submit(&self, batch: Batch) -> String;           // batch id (uuid); journals every job first
    pub fn pause_all(&self); pub fn resume_all(&self); pub fn cancel_all(&self);
    pub fn pause(&self, id: JobId); pub fn resume(&self, id: JobId);   // a single queued item (spec §8.3.9)
    pub fn cancel(&self, id: JobId); pub fn retry(&self, ids: &[JobId]); pub fn retry_all_failed(&self);
    pub fn move_to_top(&self, id: JobId);
    pub fn resume_site(&self, site: &SiteId, provider: Arc<dyn StorageProvider>); // after AuthStopped, with the edited site's client
    pub fn snapshot(&self) -> EngineSnapshot;
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64>;
    pub fn is_busy(&self) -> bool;
}
pub struct AdaptiveLimiter;  impl AdaptiveLimiter { pub fn new(configured: usize) -> Self; pub fn limit(&self) -> usize;
                                                    pub fn on_success(&mut self); pub fn on_slow_down(&mut self); pub fn set_configured(&mut self, n: usize); }
pub struct TokenBucket;  impl TokenBucket { pub fn new(bytes_per_s: Option<u64>) -> Self; pub async fn take(&self, n: usize); }  // impl BodyMeter
pub struct FileHashes { pub sha256: [u8; 32], pub md5: [u8; 16], pub size: u64, pub modified: SystemTime }
pub fn hash_file(path: &Path) -> std::io::Result<FileHashes>;   // one read, both digests, \\?\ for long paths
pub fn eta(remaining_files: u64, remaining_bytes: u64, files_per_s: f64, bytes_per_s: f64) -> Option<Duration>; // max of the two estimates
```

Job runner rules:
- Uploads hash first when `verify_checksums`, then upload with `UploadSource::File`, `content_type_for(name)`, the hashes, and a meter that counts the job's bytes and takes from the global `TokenBucket`.
- Downloads stream into `<target>.httporter-part`, take from the bucket per chunk, verify the SHA-256 when the entry has one, then rename over the target.
- Deletes use `delete_batch` in groups of `batch_delete_max` when that is > 1, `delete_folder` when `folder && recursive_delete`, otherwise `delete_file`. A delete waits while any active upload's remote `is_within` its path.
- Retries follow `settings.retry_policy()`, inside the runner. `is_slow_down()` → `limiter.on_slow_down()` when `adaptive`. Each success → `on_success()`.
- `Auth` → that site's lane stops (`EngineState::AuthStopped`). Its jobs return to Queued and none is marked Failed.
- `NotFound` on a delete retried after a lost response counts as success (ledger 179).
- The upload and transfer limit per lane is `settings.transfer_limit(capabilities.transfer_concurrency_max)`; the delete limit is `capabilities.delete_concurrency`.

- [ ] **Step 1: Write the failing tests** (paused clock; `MockProvider` with latency and faults).
  - `uploads_all_jobs_and_marks_journal_done`: 50 jobs → `files_done == 50`, `journal.unfinished().is_empty()`.
  - `journal_written_before_first_request`: the mock's first upload sees 50 Queued records.
  - `never_exceeds_the_lane_limit`: simultaneous 8 → `max_in_flight(Upload) <= 8`.
  - `slow_down_halves_and_recovers` (unit): 32 → `on_slow_down` → 16 → 20 × `on_success` → 17; never below 1; never above configured.
  - `busy_400_counts_as_slow_down`: mock fails 5 uploads with `Server{400}` → the limit drops, all 50 still succeed.
  - `adaptive_off_keeps_the_limit`.
  - `auth_failure_pauses_site_without_failing_items`: 100 jobs, Auth on the first → `state == AuthStopped`, `files_failed == 0`, 100 queued. `resume_site` → all done.
  - `network_errors_retry_then_fail_with_reason`: retries 5, 6 × Network → Failed with the `describe` hook's text (the test passes a hook returning "No connection").
  - `paused_item_is_skipped_until_resumed`.
  - `zero_retries_fail_on_first_error`.
  - `delete_waits_for_inflight_uploads_under_it`.
  - `delete_uses_batches_when_supported`: `batch_delete_max` 1000, 2,500 files → 3 `delete_batch` calls.
  - `cancel_all_marks_cancelled_and_stops`, `cancel_one`, `pause_and_resume`, `move_to_top_runs_next`, `retry_selected_requeues`.
  - `bandwidth_cap_limits_throughput`: 1 MB/s cap, 3 MB of uploads → ≥ 2.9 s virtual time.
  - `progress_counts_bytes_via_meter_and_rates_use_5s_window`.
  - `eta_is_the_longer_estimate`.
  - `hash_file_matches_known_digests`: "abc" → the SHA-256 and MD5 test vectors.
  - `twenty_thousand_jobs_stay_cheap`: submit 20,000 → `snapshot()` < 5 ms in debug; `queued.len() == 200`; `done.len() <= 200`.
- [ ] **Step 2: Run** `cargo test -p httporter-transfer`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - A lane per site: a queue of `JobId`s, active map, limiter, state.
  - A dispatcher task woken by a `Notify`.
  - Job state lives in a slab (`Vec` + index); views are built only for snapshot windows.
  - Bump `changes()` on every state change; the UI throttles reads.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(transfer): engine with adaptive limits, retries, bandwidth cap and journal`.

---

### Task 13: Planning uploads, downloads and deletes; conflicts *(Track Engine)*

**Files:**
- Create: `crates/httporter-transfer/src/{plan.rs, names.rs}`, `crates/httporter-transfer/tests/plan.rs`.

**Interfaces:**
- Produces:

```rust
pub struct UploadPlan { pub jobs: Vec<Job>, pub conflicts: Vec<Conflict>, pub skipped: Vec<(PathBuf, String)>,
                        pub files: u64, pub bytes: u64, pub folders_to_create: Vec<RemotePath> }
pub async fn plan_upload(provider: &dyn StorageProvider, sources: &[PathBuf], target: &RemotePath) -> Result<UploadPlan, ProviderError>;
pub fn resolve_uploads(plan: UploadPlan, all: Resolution, per_row: &HashMap<usize, Resolution>) -> Vec<Job>;
pub struct DownloadPlan { pub jobs: Vec<Job>, pub conflicts: Vec<Conflict>, pub renamed: Vec<(RemotePath, String)>, pub files: u64, pub bytes: u64 }
pub async fn plan_download(provider: &dyn StorageProvider, entries: &[Entry], local_dir: &Path) -> Result<DownloadPlan, ProviderError>;
pub fn resolve_downloads(plan: DownloadPlan, all: Resolution, per_row: &HashMap<usize, Resolution>) -> Vec<Job>;
pub struct DeletePlan { pub summary: DeleteSummary, pub jobs: Vec<Job> }
pub async fn plan_delete(provider: &dyn StorageProvider, entries: &[Entry]) -> Result<DeletePlan, ProviderError>;
pub fn policy_to_resolution(p: ConflictPolicy) -> Option<Resolution>;   // Ask → None
pub fn keep_both_name(name: &str, taken: &HashSet<String>) -> String;   // "photo.jpg" → "photo (1).jpg"; ".htaccess" → ".htaccess (1)"
pub fn safe_local_name(name: &str) -> String;                           // Windows-safe
```

- [ ] **Step 1: Write the failing tests** (MockProvider + tempfile).
  - `upload_of_a_folder_recurses_and_maps_paths`: `D:\x\site\a\b.txt` with target `/` → `/site/a/b.txt`.
  - `only_existing_remote_folders_are_listed`: an upload into a missing folder makes 0 listings below it.
  - `conflicts_compare_by_sha256_on_bunny_and_md5_on_s3`: identical → `same == Some(true)` with `compared_by` Sha256 or Md5; different → `Some(false)`.
  - `multipart_etag_compares_by_size`.
  - `resolution_replace_if_different_skips_identical`.
  - `keep_both_avoids_taken_and_planned_names`.
  - `file_onto_folder_is_refused_in_skipped`.
  - `unreadable_local_file_is_skipped_with_os_reason`.
  - `linked_folders_are_skipped_not_followed`.
  - `long_paths_over_260_chars_work` (ledger 163).
  - `lossy_names_are_skipped_with_reason` (ledger 164).
  - `safe_local_names_for_windows`: `"a:b"`→`"a_b"`, `"name."`→`"name"`, `"CON.txt"`→`"_CON.txt"`, `"x?y*"`→`"x_y_"`, `"aux"`→`"_aux"`.
  - `case_only_duplicates_get_distinct_names`: `"A.txt"`, `"a.txt"` → `"A.txt"`, `"a (2).txt"`, both listed in `renamed`.
  - `delete_summary_counts_recursively`: 1 folder of 48 files (12.3 MB) → `single == ("old-promo-2025", true)`, `total_files == 48`.
  - `deleting_root_is_refused`: `Err(Refused("The top folder of a site can't be deleted."))`.
  - `delete_without_recursive_capability_expands_to_files_and_markers`.
- [ ] **Step 2: Run** `cargo test -p httporter-transfer --test plan`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Hash only the local files that conflict, in parallel (8 at a time).
  - Remote listings go through `walk`/`list` under the provider's own limits.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(transfer): plan uploads, downloads and deletes with conflict detection`.

---

### Task 14: Resume, offline and integrity edge cases *(Track Engine)*

**Files:**
- Create: `crates/httporter-transfer/src/offline.rs`, `crates/httporter-transfer/tests/resilience.rs`.
- Modify: `engine.rs`, `lane.rs`.

**Interfaces:**
- Produces:

```rust
impl Engine {
    pub fn unfinished_count(&self) -> usize;
    /// Re-queues every unfinished journal record whose site `lookup` still knows. Returns how many.
    pub fn resume_from_journal(&self, lookup: &dyn Fn(&SiteId) -> Option<(String /*name*/, String /*host*/, Arc<dyn StorageProvider>)>) -> usize;
    pub fn discard_journal(&self);
}
```

- Offline rule: a lane goes **Offline** after 3 `Network` errors within 10 s, or after one job exhausts its retries on `Network`.
  - Offline: dispatch stops; jobs failing with `Network` return to Queued without using an attempt.
  - `provider.reachable()` is probed every 5 s; `Ok` → Running.
- Checksum mismatch: re-hash, retry once, then Failed with "The file arrived damaged twice (checksum mismatch)." (copy hook).
- A local file changed since hashing (size or mtime differs) → re-hash before sending.
- Download disk full (`ErrorKind::StorageFull` or OS error 112) → every lane pauses with `EngineState::DiskFull { drive, needed }`, where `needed` = remaining download bytes on that drive.
- A local file that is locked or unreadable → that job Fails with the OS message; others continue.

- [ ] **Step 1: Write the failing tests.**
  - `offline_pauses_and_resumes_without_failures`: mock returns Network for 30 virtual seconds on every op (including `reachable`) → `state == Offline`, `files_failed == 0`; then ok → all done.
  - `crash_then_resume_sends_only_unfinished`: `MemoryJournal` shared; engine A runs 200 jobs on a slow mock and is dropped at ~half; engine B `resume_from_journal` → every path uploaded at least once, paths marked Done before the drop are uploaded exactly once, total uploads ≤ 200 + A's lane limit.
  - `resume_skips_sites_that_no_longer_exist`.
  - `checksum_mismatch_rehashes_and_retries_once`.
  - `changed_file_is_rehashed_before_sending`.
  - `locked_file_fails_alone`.
  - `disk_full_pauses_with_drive_and_bytes` (fake writer returning StorageFull).
  - `discard_journal_clears_unfinished`.
- [ ] **Step 2: Run** the resilience tests. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(transfer): crash resume, offline pause and integrity handling`.

---

### Task 15: Windows services *(Track Platform)*

**Files:**
- Create: `crates/httporter-platform/src/win/{mod.rs, taskbar.rs, notify.rs, pickers.rs, clipboard.rs}`.
- Modify: `crates/httporter-platform/Cargo.toml` (`windows`, `rfd`).

**Interfaces:**
- Produces:

```rust
pub enum TaskbarState { None, Normal { done: u64, total: u64 }, Paused { done: u64, total: u64 }, Error { done: u64, total: u64 } }
pub struct TaskbarProgress;  impl TaskbarProgress { pub fn new(hwnd: isize) -> Option<Self>; pub fn set(&self, s: TaskbarState); }
pub fn notify(title: &str, body: &str) -> Result<(), String>;   // toast with AUMID "HTTPorter.App"
pub fn flash(hwnd: isize);                                       // fallback when notify fails
pub fn is_foreground(hwnd: isize) -> bool;
pub fn pick_files(title: &str) -> Vec<PathBuf>;
pub fn pick_folder(title: &str) -> Option<PathBuf>;
pub fn pick_file(title: &str, filters: &[(&str, &[&str])]) -> Option<PathBuf>;
pub fn clipboard_files() -> Vec<PathBuf>;                        // CF_HDROP
```

- [ ] **Step 1: Write the failing tests** (pure parts only):
  - `taskbar_state_maps_to_flags_and_values`: a pure `fn flags(&TaskbarState) -> (TBPFLAG, u64, u64)`.
  - `notification_xml_escapes_text`: `fn toast_xml(title, body) -> String` escapes `<&"'`.
  - `aumid_registration_values`: `fn aumid_values() -> [(&str, &str); 2]`, `DisplayName = "HTTPorter"`, `IconBackgroundColor = "FF0A84FF"`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Notifications register the AUMID under `HKCU\Software\Classes\AppUserModelId\HTTPorter.App` on first use (per-user, no admin), call `SetCurrentProcessExplicitAppUserModelID`, and fall back to `flash` on any error.
  - COM is initialised per call (`COINIT_APARTMENTTHREADED`) on the calling thread.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(platform): taskbar progress, notifications, pickers and clipboard`.
- The user verifies taskbar progress and the notification in Task 26; agents don't drive the desktop.

---

### Task 16: App restructure, no behaviour change *(Track UI-A; UI-B starts after it)*

**Files:**
- Split `src/session.rs` → `src/session/{mod.rs, sites.rs, remote.rs, local.rs, fakes.rs}` (ledger 248).
- Split `src/session_tests.rs`, `src/keyboard_tests.rs` → plus `src/pane_tests.rs` (ledger 198).
- Split `ui/components/add_site.slint` → plus `ui/components/form_controls.slint` (Field, FormRow, ErrorLine, RegionPicker; ledger 214).
- Create: `src/copy.rs`.
- Modify: `src/main.rs`, `src/binding.rs`, `ui/app.slint`.

**Interfaces:**
- Produces:
  - `copy.rs` holds every Rust-built user string: `items`, `items_status`, `describe(&ProviderError, site, host) -> String`, `drop_status`, the add-site field errors, and the strings Tasks 17–25 add.
  - `sidebar-visible` becomes `in-out` (ledger 152).

- [ ] **Step 1: Write the failing tests.**
  - `tr_scanner_catches_one_line_and_description_forms` (ledger 151): the source scanner flags `Text { text: "x"; }` and `description: "y"` without `@tr`, and ignores `"//"` inside strings.
  - `copy_strings_live_only_in_copy_rs`: a scan of `src/**/*.rs` except `copy.rs` and tests finds no string literal passed to `set_*_text`, `set_status_message` or `format!` feeding a `set_` call.
  - `release_startup_failure_shows_a_message` (ledger 132): `fn startup_error_text(&dyn Error) -> String` is pinned; `main` calls `MessageBoxW` with it in release builds.
- [ ] **Step 2: Run** `cargo test -p httporter-app`. Expected: FAIL.
- [ ] **Step 3: Move code; centralise copy.**
  - Credential, store and client-build calls run in `spawn_blocking` (ledger 248, 252).
  - Wall-clock 50k tests: budgets ×4 in debug builds (ledger 229).
  - The drop-hook timer tests use a paused clock.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: all existing tests plus the new ones pass. Launch once (`tools/measure.ps1`): the window is identical.
- [ ] **Step 5: Commit** `refactor(app): split session and tests, one copy module, blocking work off the runtime`.

---

### Task 17: Selection, toolbar actions and context menus *(Track UI-A)*

**Files:**
- Create: `src/selection.rs`.
- Modify: `ui/components/{pane.slint, toolbar.slint, statusbar.slint}`, `ui/app.slint`, `ui/types.slint` (FileRow gains `selected: bool`), `src/binding.rs`, `src/copy.rs`.

**Interfaces:**
- Produces:

```rust
pub struct Selection;  impl Selection {
    pub fn click(&mut self, i: usize); pub fn ctrl_click(&mut self, i: usize); pub fn shift_click(&mut self, i: usize);
    pub fn select_all(&mut self, n: usize); pub fn band(&mut self, from: usize, to: usize, additive: bool);
    pub fn move_focus(&mut self, delta: isize, extend: bool, n: usize); pub fn clear(&mut self);
    pub fn rows(&self) -> Vec<usize>; pub fn focus(&self) -> Option<usize>; }
pub fn selection_summary(rows: &[&RowData]) -> String;   // copy.rs
```

- Slint callbacks on `AppWindow`:
  - `upload()`, `download()`, `download-to()`, `new-folder()`, `delete()`;
  - `row-pointer(pane: int, row: int, ctrl: bool, shift: bool)`, `band-select(pane: int, from: int, to: int, additive: bool)`, `select-all(pane: int)`;
  - `show-in-explorer(row: int)`.
- `in property` `can-upload`, `can-download`, `can-new-folder`, `can-delete`, `delete-note`.

Rules:
- **Upload** (toolbar, Ctrl+U):
  - uploads the local selection into the cloud pane's current folder;
  - with no local selection, it opens the file picker.
  - Enabled when a site is open and not read-only.
- **Download** (toolbar): the cloud selection → the This PC pane's folder. At the drive list it asks with `pick_folder`.
- **New folder** (toolbar, Ctrl+Shift+N): acts on the active pane.
- **Delete** (toolbar, Del):
  - cloud pane only;
  - disabled when the This PC pane is active, with tooltip "Delete works in the cloud pane. Use Explorer for files on this PC.";
  - disabled when the selection is empty.
- **Context menus** (Slint `ContextMenuArea`):
  - cloud rows: Download, Download to…, New Folder…, Delete…, Refresh;
  - This PC rows: Upload, New Folder…, Show in Explorer, Refresh.
- **Status bar summaries:**
  - one file → "index.html selected · 383.4 KB";
  - one folder → "uploads selected";
  - several → "12 items selected · 4.2 MB" (file bytes only; no size part when only folders).
- **Status bar right:** "HTTP/1.1 · {host} · {n} connections ready".

- [ ] **Step 1: Write the failing tests.**
  - `selection_click_ctrl_shift_band_all`: the Finder/Explorer semantics (Shift extends from the anchor, Ctrl toggles and moves the anchor, Shift+Ctrl adds the range).
  - `move_focus_extends_with_shift`.
  - `summary_texts`: the three cases above.
  - Headless Slint tests:
    - `ctrl_a_selects_all_rows`;
    - `toolbar_buttons_follow_rules`;
    - `delete_disabled_on_this_pc_with_note`;
    - `context_menu_items_present_with_tr`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Rows render `selected`.
  - Rubber band: dragging in empty list space draws a translucent `Theme.accent` rectangle and reports `band-select` as it moves.
  - Callbacks forward to `binding.rs` stubs that Task 23 wires. Until then the buttons stay disabled through `can-*` = false, so nothing is inert.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): multi-select, selection summary, toolbar actions and context menus`.

---

### Task 18: Transfer drawer *(Track UI-B)*

**Files:**
- Create: `ui/components/drawer.slint`, `src/drawer_view.rs`.
- Modify: `ui/app.slint`, `ui/types.slint` (`TransferRow`), `src/copy.rs`.

**Interfaces:**
- Consumes: `EngineSnapshot` (Task 3).
- Produces:

```rust
pub struct DrawerView { pub running: bool, pub paused: bool, pub collapsed_text: String, pub title: String, pub line: String,
    pub files_per_s: String, pub mb_per_s: String, pub eta: String, pub percent: f32,
    pub tab_titles: [String; 4], pub active: Vec<TransferRowData>, pub queued: Vec<TransferRowData>,
    pub failed: Vec<TransferRowData>, pub done: Vec<TransferRowData> }
pub struct TransferRowData { pub id: u64, pub name: String, pub kind: FileKindData, pub size: String, pub percent: f32, pub detail: String }
pub fn drawer_view(s: &EngineSnapshot, now: DateTime<Utc>) -> DrawerView;
pub struct Throttle;  impl Throttle { pub fn new(min: Duration) -> Self; pub fn ready(&mut self, now: Instant) -> bool; }  // 100 ms
```

- Slint `TransferRow { id: int, name: string, kind: FileKind, size: string, percent: float, detail: string }`.
- Callbacks: `transfers-toggle()`, `transfers-pause()`, `transfers-resume()`, `transfers-cancel-all()`, `transfer-cancel(int)`, `transfer-retry(int)`, `transfers-retry-failed()`, `transfer-move-to-top(int)`, `transfer-pause(int)`, `transfer-resume(int)`.

Copy (pinned):
- **Collapsed:**
  - idle → "No transfers running" plus " · Last transfer {relative} · {n} files · {size}" when there is a last batch;
  - running → "Uploading 8,412 of 13,793 files · 96 files/s".
  - Button "Show Transfers" / "Hide Transfers".
- **Title:**
  - "Uploading to {site}" / "Downloading from {site}" / "Deleting from {site}" / "Transferring with {site}";
  - paused → "Paused · {title}";
  - offline → "Waiting for the connection · {title}";
  - auth stopped → "Sign-in failed · {site}".
- **Line:**
  - "{done} of {total} files · {done} of {total} MB · over HTTPS to {host}";
  - deletes → "{done} of {total} items on {host}";
  - the size unit follows the total, as integers when ≥ 100 in that unit.
- **Stats:** "96" "files/s" (one decimal below 10), "6.1" "MB/s", "about" "56 s" / "4 min" / "1 h 20 min" "left".
- **Tabs:** "Active {n}", "Queued {n}", "Failed {n}", "Done {n}".
- **Buttons:** "Pause"/"Resume", "Cancel"; Failed tab "Retry All", row "Retry"; queued row context menu "Move to Top", "Pause" / "Resume".

- [ ] **Step 1: Write the failing tests.**
  - `idle_and_last_batch_text`.
  - `running_title_line_stats_match_mockup`: the snapshot from `Main.dc.html` (8,412 of 13,793, 527 of 865 MB, 96 files/s, 6.1 MB/s, 56 s) → the exact strings above.
  - `paused_offline_auth_titles`.
  - `percent_is_files_done_over_total`.
  - `throttle_allows_10_per_second`.
  - `drawer_view_handles_50k_rows`: a snapshot with 200 queued views + counts → < 5 ms.
  - Headless Slint: `drawer_expands_and_tabs_switch`, `row_cancel_and_retry_callbacks_carry_ids`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Match `Transfers.dc.html`: progress ring, stats block, tab strip, rows with kind icon, path, size, bar and cancel ×.
  - The drawer is collapsible and never a separate window. Use a virtualised `ListView`.
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): transfer drawer`.

---

### Task 19: Sheets, alerts and banners *(Track UI-B)*

**Files:**
- Create: `ui/components/sheets.slint`, `src/sheets_view.rs`.
- Modify: `ui/app.slint`, `src/copy.rs`.

**Interfaces:**
- Consumes: `Conflict`, `DeleteSummary`, `Direction` (Task 3).
- Produces:

```rust
pub struct ConflictSheetView { pub title: String, pub body: String, pub for_all_label: String, pub summary: String,
    pub col_source: String, pub col_target: String, pub rows: Vec<ConflictRowData>,
    pub remember_label: String, pub cancel_label: String }
pub struct ConflictRowData { pub name: String, pub source: String /* "357.2 KB · Today 09:58" */, pub target: String,
    pub contents: String /* Identical | Different | Unknown */, pub action_index: i32 /* 0 Replace, 1 Skip, 2 Keep both */ }
pub fn conflict_sheet(dir: Direction, conflicts: &[Conflict], total_files: u64, target: &str, site: &str, short: &str) -> ConflictSheetView;
pub fn delete_alert(summary: &DeleteSummary, site: &str, provider: ProviderKind) -> (String /*title*/, String /*body*/, String /*button*/);
pub fn remove_site_alert(site: &str, provider_label: &str) -> (String, String);
pub fn quit_alert(unfinished: u64) -> (String, String);
pub fn resume_banner(n: usize) -> String; pub fn offline_banner() -> String; pub fn auth_banner(site: &str) -> String;
pub fn disk_full_banner(drive: &str, needed: u64) -> String;
pub fn new_folder_error(name: &str, existing: &HashSet<String>) -> Option<String>;
```

Copy (pinned):
- **Conflict sheet (mockup):**
  - Title: "12 files already exist on example-shop.com".
  - Body: "You're uploading 640 files to wp-content / uploads / 2026 / 10. Choose once what happens to existing files; you can change single files below."
  - Segmented control "For all 12:" Replace if different | Replace all | Skip all | Keep both.
  - Summary: "7 differ · 5 identical (same SHA-256)" (MD5 → "(same MD5)", size → "(same size)").
  - Columns: Name · On this PC · On {short} · Contents (Identical/Different/Unknown) · Action (per-row menu: Replace, Skip, Keep both; an identical row under "Replace if different" shows "Skip (identical)").
  - Checkbox: "Remember for uploads to this site".
  - Buttons: Cancel Upload / Continue (default).
  - Downloads: "12 files already exist in {folder}", "You're downloading …", columns On {short} · On this PC, "Remember for downloads from this site", "Cancel Download".
- **Delete alert:**
  - one folder: "Delete “old-promo-2025” from example-shop.com?" / "This folder holds 48 files (12.3 MB). Bunny Storage has no trash, so this can't be undone." / "Delete 48 Files";
  - one file: "Delete “index.html” from example-shop.com?" / "Bunny Storage has no trash, so this can't be undone." / "Delete";
  - several: "Delete 3 folders and 12 files from example-shop.com?" / "That's 1,284 files (54 MB) in all. Bunny Storage has no trash, so this can't be undone." / "Delete 1,284 Files";
  - for S3 sites the trash sentence becomes "This can't be undone.";
  - "Cancel" is the default button; the delete button uses the destructive style;
  - the checkbox "Also purge these URLs from the CDN" is disabled with the note "CDN purge arrives with Publish."
- **New Folder sheet:**
  - title "New Folder", field "Name" (default "untitled folder"), buttons Create / Cancel;
  - errors: "Enter a name.", "Names can't contain / or \\.", "“{name}” already exists here."
- **Remove site:** "Remove “{site}”?" / "HTTPorter forgets this site and its saved password. Files on {provider} aren't touched." / Remove (destructive) / Cancel (default).
- **Quit alert:** "Transfers are still running" / "{n} files haven't finished." / Pause and Quit · Cancel and Quit · Keep Running (default).
- **Banners:**
  - "{n} transfers didn't finish last time." [Resume] [Discard];
  - "You're offline. Transfers continue when the connection is back.";
  - "Couldn't sign in to {site}. Its transfers are paused." [Edit Site…];
  - "Drive {drive} is full. Free up {size} and click Resume." [Resume].

- [ ] **Step 1: Write the failing tests.** Each pinned text appears in at least one test:
  - `conflict_sheet_matches_mockup`, `conflict_sheet_download_variant`;
  - `delete_alert_variants_bunny_and_s3`;
  - `new_folder_errors`;
  - `remove_quit_and_banner_texts`.
  - Headless Slint:
    - `delete_alert_default_is_cancel`: Return activates Cancel;
    - `conflict_rows_change_action`;
    - `sheets_trap_focus_and_escape_cancels`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement** to match `Conflicts.dc.html` and `DeleteAlert.dc.html` (sheet attached under the toolbar; the alert is centred and compact).
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): conflict sheet, delete alert, new folder, remove site, quit alert and banners`.

---

### Task 20: Settings window *(Track UI-B)*

**Files:**
- Create: `ui/components/settings_window.slint` (a separate `Window` component), `src/settings_view.rs`.
- Modify: `ui/app.slint` (enable the Settings gear; drop the "later version" disabling from P22), `src/copy.rs`.

**Interfaces:**
- Consumes: `TransferSettings` (Task 3).
- Produces:

```rust
pub struct SettingsFormData { pub simultaneous: i32, pub adaptive: bool, pub retries_index: i32 /*0:5,1:3,2:never*/,
    pub bandwidth_on: bool, pub bandwidth_mb: String, pub protocol_index: i32, pub keep_alive: bool,
    pub timeout_index: i32 /*0:120,1:60,2:300*/, pub proxy_index: i32 /*0 system,1 none,2 custom*/, pub proxy_url: String,
    pub verify: bool, pub upload_conflicts_index: i32, pub download_conflicts_index: i32 }
pub fn to_form(s: &TransferSettings) -> SettingsFormData;
pub fn from_form(f: &SettingsFormData) -> Result<TransferSettings, (String /*field*/, String /*message*/)>;
```

- Slint `SettingsWindow` callbacks: `changed()` (every edit applies at once, the HIG way) and `restore-defaults()`.
- Tabs: General, Sites, Transfers, Security, Publish, Advanced.
  - Only Transfers is active in I2.
  - The others show the centred note "These settings arrive in a later version." and their tab buttons stay enabled, so the note is visible, never inert.

Copy is exactly `Settings.dc.html`:
- **Speed:** "Simultaneous transfers" with the help "Per site. Bunny Storage allows up to 96 from one computer; S3 services up to 128."; slider and number 1–128.
- **Slow down:** "Slow down when a server asks" / "On HTTP 429 or 503, halve parallel transfers, then ramp back up."
- **Retry:** "Retry failed transfers" / "Waits 1, 2, 4, 8 and 10 seconds between attempts." / 5 times · 3 times · Never.
- **Bandwidth:** "Bandwidth limit" / "Applies to all transfers together." / Off · Limit to [__] MB/s.
- **HTTP connection:**
  - "Protocol" / "Every connection is encrypted with TLS 1.2 or newer." / HTTP/2 when available · HTTP/1.1 only;
  - "Keep connections open between files" / "Saves a TLS handshake per file. Strongly recommended.";
  - "Request timeout" 120 · 60 · 300 seconds;
  - "Proxy" Use Windows settings · No proxy · Custom….
- **Integrity and conflicts:**
  - "Verify every upload with a checksum" / "SHA-256 on Bunny Storage, MD5 on S3-compatible services. The server rejects damaged files.";
  - "When a file already exists" / "One choice per batch, never one dialog per file." Upload choices: Ask once per batch · Replace if different · Skip · Replace all · Keep both. Download choices: Ask once per batch · Replace if different · Keep both · Replace all · Skip.
- "Restore Defaults".

- [ ] **Step 1: Write the failing tests.**
  - `form_round_trips_defaults`.
  - `bandwidth_must_be_a_positive_number`: `"0"` → `Err(("bandwidth", "Enter a speed above 0 MB/s."))`; `"2.5"` → 2,621,440 B/s.
  - `custom_proxy_needs_a_url`: "Enter a proxy address such as http://proxy:8080."
  - `simultaneous_clamped_1_to_128`.
  - Headless Slint: `only_transfers_tab_has_controls`, `restore_defaults_fires`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): Settings window with the Transfers tab`.

---

### Task 21: Add Site, round 2, and Welcome *(Track UI-B)*

**Files:**
- Create: `ui/components/{provider_grid.slint, welcome.slint}`, `src/import.rs`.
- Modify: `ui/components/add_site.slint`, `ui/components/form_controls.slint`, `src/add_site.rs`, `src/copy.rs`, `ui/app.slint`.

**Interfaces:**
- Produces:

```rust
pub enum SiteForm { BunnyPassword { zone: String, region_index: i32, password: String },
                    BunnyAccount { key: String },
                    S3 { preset: S3Preset, region: String, account_id: String, endpoint: String, path_style: bool,
                         bucket: String, key_id: String, secret: String } }
pub fn validate(form: &SiteForm, editing: bool) -> Result<(), Vec<(&'static str /*field*/, String)>>;  // editing: empty secret = keep
pub fn s3_endpoint_preview(preset: S3Preset, region: &str, account_id: &str, endpoint: &str, bucket: &str, path_style: bool) -> String;
pub struct EnvImport { pub zone: String, pub password: String, pub region: String }   // Debug redacts password
pub fn parse_env(text: &str) -> Result<EnvImport, String>;    // BUNNY_ZONE, BUNNY_KEY, BUNNY_REGION (BUNNY_LOCAL_DIR / BUNNY_PREFIX ignored until I3)
pub struct AwsProfile { pub name: String, pub key_id: String, pub secret: String, pub region: Option<String> } // Debug redacts secret
pub fn parse_aws_profiles(credentials: &str, config: &str) -> Vec<AwsProfile>;
```

- Slint callbacks:
  - `add-site-provider(int)`, `add-site-sign-in-mode(int)`, `add-site-find-zones(string)`, `add-site-zones-chosen([bool], bool /*keep key*/)`;
  - `add-site-list-buckets()`, `add-site-reveal()`;
  - `welcome-add()`, `welcome-import()`.
- Properties for the zone list (name, region, pull-zone hostnames, checked) and bucket list.

Copy and layout follow `Main.dc.html` `overlay=addsite` and `Welcome.dc.html`:
- **Header:** "Add Site" / "Connect to cloud storage over HTTPS. Passwords and keys are stored in Windows Credential Manager, never in plain files."
- **Provider grid:** Bunny Storage (HTTP API, B, #e8590c), Amazon S3 (S3), Cloudflare R2 (S3-compatible), Backblaze B2, Wasabi, DigitalOcean Spaces, MinIO, Other S3 (Custom endpoint). Each tile's colour comes from the mockup's provider list.
- **Bunny sign-in:**
  - "Sign in with" Storage zone password · Bunny account key.
  - Password help: "Bunny dashboard → Storage → {zone} → FTP & API Access. Use the full password, not the read-only one."
  - Account key flow:
    - Key field, then "Find Zones".
    - A checklist of zones ("{name} · {region} · {hostnames}").
    - Radio: "Use it once to fetch zone passwords, then forget it" (default) · "Keep it for cache purging and zone tools".
    - Warning: "An account key can change anything in your Bunny account. HTTPorter uses it only for what you choose here."
    - Button "Add {n} Sites".
- **S3 fields by preset:**
  - AWS: Region, Bucket.
  - R2: Account ID, Bucket.
  - B2 / Wasabi / Spaces: Region, Bucket.
  - MinIO: Endpoint.
  - Other: Endpoint, Region, "Use path-style addresses".
  - All: Access key ID, Secret access key; Bucket with a "Choose…" button (lists buckets) or typed.
- Endpoint preview row; "Test Connection"; "Cancel"; "Add Site".
- **Reveal:** an eye button on every secret field; the secret hides again when the sheet closes.
- **Edit mode:**
  - Title "Edit Site", button "Save".
  - The secret placeholder reads "Saved. Leave empty to keep it."
- **Welcome:**
  - Wordmark "HTTP"+"orter".
  - Tagline "Fast, safe file transfers to cloud storage over HTTP."
  - Three rows:
    - "Made for HTTP storage" / "Talks straight to Bunny Storage and S3-compatible services over HTTPS, sending many files at once instead of one by one."
    - "Publish a whole folder" / "Uploads only what changed, checked by checksum, then clears the CDN so visitors see it right away."
    - "Safe by design" / "You see every change before it happens. Passwords stay in Windows Credential Manager, never in plain files."
  - Buttons "Add Your First Site" and "Import from .env or AWS profile…".
  - Footer "Free software · No telemetry · Your files go only to the services you add".
  - Sidebar empty states: "No sites yet. Add one to start browsing and publishing." / "Publish profiles appear here after you add a site."

- [ ] **Step 1: Write the failing tests.**
  - `validate_each_form_kind_and_edit_keeps_secret`.
  - `s3_endpoint_previews_per_preset`.
  - `parse_env_reads_the_scripts_variables`: BOM and quotes are tolerated; a missing `BUNNY_KEY` → `Err("The file has no BUNNY_KEY line.")`.
  - `parse_aws_profiles_merges_config_regions`: `[profile x]` in config, `[x]` in credentials.
  - `import_debug_never_shows_secrets`.
  - Headless Slint:
    - `provider_grid_has_eight_tiles_in_mockup_order`;
    - `reveal_toggles_and_resets_on_close`;
    - `welcome_texts_match_mockup`;
    - `account_key_zone_list_checkboxes`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): Add Site for every provider, account key sign-in, Welcome and import`.

---

### Task 22: Drag and drop, drop on folder rows, paste *(Track UI-A)*

**Files:**
- Modify: `src/drop_hook.rs`, `ui/components/pane.slint`, `ui/app.slint`, `src/main.rs`, `src/copy.rs`.

**Interfaces:**
- Produces:

```rust
pub struct PaneGeometry { pub x: f32, pub y: f32, pub w: f32, pub h: f32, pub list_top: f32, pub row_height: f32, pub scroll_y: f32, pub rows: usize }
pub struct PaneLayout { pub local: PaneGeometry, pub remote: PaneGeometry }   // replaces split_x / stacked / remote_y
pub fn hit(pos: (f32, f32), layout: &PaneLayout) -> Option<(PaneSide, Option<usize> /*row*/)>;
pub struct DropBatch { pub side: PaneSide, pub row: Option<usize>, pub paths: Vec<PathBuf> }
```

- Slint callbacks: `drag-to-remote(row: int)` (-1 = current folder), `drag-to-local(row: int)`, `paste()`.
- `out property` geometry per pane.

Rules:
- A position outside both panes' rectangles (toolbar, status bar, drawer) hits nothing (ledger 228).
- Over a folder row, the row highlights and the hover chip reads "Release to upload to {folder}"; elsewhere on the cloud pane, "Release to upload".
- In-app drags start after 4 px of movement. They show a ghost "{n} items" and can be cancelled with Esc. Dropping on the other pane or one of its folder rows fires the callback; dropping on the same pane does nothing.
- Ctrl+V on the cloud pane uploads `clipboard_files()` into its current folder.

- [ ] **Step 1: Write the failing tests.**
  - `hit_maps_rows_with_scroll`.
  - `toolbar_and_status_bar_hit_nothing`.
  - `stacked_layout_hits`.
  - `row_beyond_count_is_pane_not_row`.
  - `hover_text_names_the_folder`.
  - `hook_hover_is_change_gated_and_ends_once` and `negative_origin_to_logical` (ledger 226).
  - Headless Slint: `in_app_drag_fires_with_target_row`, `escape_cancels_drag`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.** Remove `capture_geometry`'s fail-open (ledger 227): with no geometry, the drop is ignored with the status "Couldn't tell where the files were dropped. Try again."
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): drag between panes, drop onto folder rows and paste`.

---

### Task 23: Integration, transfers on Bunny *(after Platform, Bunny, Engine, UI-A, UI-B)*

**Files:**
- Create: `src/site_hub.rs`, `src/transfers.rs`, `src/transfers_tests.rs`.
- Modify: `src/main.rs`, `src/binding.rs`, `src/session/*`, `src/copy.rs`.

**Interfaces:**
- Consumes: every earlier task.
- Produces:

```rust
pub struct SiteHub;  impl SiteHub {
    pub fn new(creds: Arc<dyn CredentialStore>, factory: Arc<dyn ProviderFactory>, settings: TransferSettings) -> Self;
    pub fn provider(&self, site: &SiteRecord) -> Result<Arc<dyn StorageProvider>, HubError>;  // cached, one per site
    pub fn invalidate(&self, id: &SiteId); pub fn set_settings(&self, s: TransferSettings);  // rebuilds lazily
}
pub enum HubError { MissingSecret, Credential(String), Provider(ProviderError) }   // MissingSecret → the F5 copy
// ProviderFactory gains: fn s3(&self, cfg: S3Config) -> Result<Arc<dyn StorageProvider>, ProviderError>;
pub struct Transfers;  // owns Engine; on the UI side through invoke_from_event_loop
```

Wiring:
- **Browsing:** browsing listings use `SiteHub` + `with_retry(&RetryPolicy::interactive(), …)`. The I1 `interactive()` config hack is removed (parked item).
- **Upload, download, new folder:**
  - Upload/download from the toolbar, context menu, drag, Explorer drop (row-aware) and paste → plan (on tokio) → conflict sheet when the policy is Ask and there are conflicts → resolve → `submit`.
  - Bunny "New folder" with `Refused` → a virtual folder row "{name}" with a note "Appears on Bunny Storage when you add its first file." The note stays until a refresh shows it real.
  - "Remember for … this site" stores `site_conflicts_key`.
- **Delete:** plan_delete → alert → `submit`.
- **Settings:** the Settings window loads and saves `SETTINGS_TRANSFERS`; changes go to `Engine::set_settings` + `SiteHub::set_settings`.
- **Drawer:** `changes()` → `Throttle` (100 ms) → `drawer_view` → properties.
- **Taskbar progress:** `TaskbarProgress` follows the snapshot.
- **Notification:** a batch longer than 20 s finishing while `!is_foreground` → `notify("Upload finished", "Uploaded 1,204 files to example-shop.com.")` (download, delete and failure variants live in copy.rs).
- **Refresh:** a finished batch refreshes any pane showing an affected folder.
- **Startup:** if `unfinished_count() > 0`, show the resume banner (Resume → `resume_from_journal` with the hub; Discard → `discard_journal`).
- **Close:** a close request while busy → quit alert:
  - Pause and Quit: the journal keeps everything;
  - Cancel and Quit: `cancel_all`, then exit;
  - Keep Running: stay open.
- **Banners:** offline, auth-stopped ("Edit Site…" opens edit), disk full.
- **Status bar right:** "{protocol} · {host} · {n} connections ready".

- [ ] **Step 1: Write the failing tests** (`transfers_tests.rs`; mock factory, `MemoryJournal`, headless window).
  - `drop_on_folder_row_uploads_into_that_folder`.
  - `upload_with_conflicts_shows_sheet_then_submits_resolved`.
  - `ask_policy_remembered_per_site`.
  - `delete_shows_counts_then_deletes`.
  - `new_folder_on_bunny_is_virtual_until_refresh`.
  - `browse_and_transfers_share_one_client`: factory called once per site.
  - `settings_change_rebuilds_clients_lazily`.
  - `resume_banner_resumes_and_discard_clears`.
  - `quit_while_busy_asks`.
  - `auth_stop_banner_opens_edit`.
  - `finished_batch_refreshes_visible_folder`.
  - `drawer_updates_at_most_10_hz`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `tools/check.ps1`, release build, `tools/measure.ps1`. Expected: PASS; exe ≤ 25 MB; idle ≤ 40 MB.
- [ ] **Step 5: Commit** `feat(app): uploads, downloads, deletes and folders on Bunny with the transfer drawer`.
- [ ] **Step 6: User checkpoint** (on the test zone `httporter-test-sg`, never `exampleshop`):
  - upload a folder by drop, by drag and by toolbar;
  - download;
  - conflict sheet;
  - delete alert;
  - pause/resume;
  - kill the app mid-upload and resume;
  - Wi-Fi off and on mid-upload;
  - Settings changes.

  Record the results in `docs/acceptance/I2.md`.

---

### Task 24: Integration, sites: S3, account key, import, Welcome, edit, duplicate, remove *(after S3, Task 23)*

**Files:**
- Modify: `src/session/sites.rs`, `src/binding.rs`, `src/site_hub.rs`, `src/add_site.rs`, `src/import.rs`, `src/copy.rs`, `src/main.rs`.

**Interfaces:**
- Consumes: Tasks 4, 6, 9, 10, 21, 23.
- Produces:

```rust
impl Session {
    pub fn save_site(&self, form: SiteForm, editing: Option<SiteId>, name: String) -> impl Future<Output = Result<SiteRecord, String>>; // tests the connection first
    pub fn duplicate_site(&self, id: &SiteId) -> Result<SiteRecord, String>;   // copies record + secret, name "{name} copy"
    pub fn remove_site(&self, id: &SiteId) -> Result<(), String>;              // DB row + credential; never remote files
    pub fn find_zones(&self, key: String) -> impl Future<Output = Result<Vec<ZoneInfo>, String>>;
    pub fn add_zones(&self, zones: Vec<ZoneInfo>, keep_key: Option<String>) -> impl Future<Output = Result<Vec<SiteRecord>, String>>;
    pub fn list_buckets(&self, form: SiteForm) -> impl Future<Output = Result<Vec<String>, String>>;
}
```

- **Sidebar:**
  - Site rows get a context menu: Edit…, Duplicate, Remove… (also Shift+F10 / the Menu key).
  - Removal deletes `HTTPorter/site/<id>`, invalidates the hub, and cancels that site's queued jobs after the remove alert.
  - Kept account keys go to `HTTPorter/account/<id>` + `accounts`.
  - The missing-password message (F5) now has a working "Remove this site" path, and Edit as well. The copy becomes "No password saved for {name}. Edit the site to enter it again."
- **Welcome:** shows when there are no sites. Import:
  - a `.env` → prefilled Bunny form, then tested on save;
  - AWS profiles → a pick list → prefilled S3 form.

- [ ] **Step 1: Write the failing tests.**
  - `edit_changes_name_and_password_and_rebuilds_client`.
  - `edit_with_empty_secret_keeps_it`.
  - `duplicate_copies_secret_under_new_id`.
  - `remove_deletes_row_and_credential_and_cancels_jobs`.
  - `s3_site_saved_after_successful_test_with_secret_json`.
  - `s3_auth_errors_name_the_field` (copy: AccessKey → "The access key ID wasn't recognized.", Secret → "The secret access key doesn't match the access key ID.", Region → "The region doesn't match the bucket. Check the region.", ClockSkew → "Your PC's clock is off. Turn on “Set time automatically” in Windows Settings.").
  - `account_key_adds_ticked_zones_and_forgets_key_by_default`.
  - `keep_key_stores_account_credential`.
  - `welcome_shows_only_without_sites`.
  - `import_env_prefills_bunny`.
  - `aws_profile_prefills_s3`.
  - `missing_password_message_offers_edit`.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(app): S3 sites, Bunny account key, import, Welcome, edit, duplicate and remove`.

---

### Task 25: Browse polish from I1 *(after Task 24)*

**Files:**
- Modify: `src/session/*`, `src/binding.rs`, `ui/components/pane.slint`, `ui/app.slint`, `src/copy.rs`.

Items, each with its test in `session_tests.rs` / `pane_tests.rs`:
1. **While connecting:** the status bar shows "Connecting…" instead of "0 items"; Back/Forward are disabled until the switch view lands (ledger 252).
2. **NotFound before any successful listing:** the pill goes orange and the dot amber, not healthy (ledger 252).
3. **Back and Forward:**
   - Two quick Backs move two steps (ledger 252).
   - Back to a deleted folder shows "Couldn't find …" and stays navigable (ledger 252).
   - Alt+←/→ act on the pane that has focus, including breadcrumbs and headers, and work at startup without a click (ledger 252).
4. **Alt key:** Alt+Space and Alt+Return no longer press focused buttons (ledger 252).
5. **F5 during loading** re-requests the pending folder instead of the shown one (parked).
6. **Loading state:**
   - The pane's "Loading…" appears only after 200 ms and is announced once (parked).
   - Stale listings are aborted when superseded, freeing their slot (parked).
7. **Drop messages:** the drop message is cleared only by the pane it belongs to (parked).
8. **Refresh button:** a visible refresh control in each pane header (spec §8.2.1; parked).
9. **Add Site cancel:** cancel during save does not save (parked). The token is checked before writing.
10. **Root 404:** a 404 at the zone root names the field: "Couldn't find zone “{zone}” in {region}. Check the zone name and region." (Minor 4).
11. **Old site's listing:** a listing from the previous site never shows under the new site's name (Minor 5).
12. **Binding tests:** they run `upgrade_in_event_loop` closures through a real event-loop pump (ledger 248).

- [ ] **Step 1:** Write the failing tests named `polish_<n>_<what>` for items 1–12.
- [ ] **Step 2: Run.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `tools/check.ps1`. Expected: PASS.
- [ ] **Step 5: Commit** `fix(app): browse polish carried over from I1`.

---

### Task 26: I2 acceptance on the user's PC

**Files:**
- Create: `docs/acceptance/I2.md`, `docs/acceptance/i2-*.png` (HTTPorter's own window only).
- Modify: `docs/design/mockup-feature-checklist.md` (tick the I2 rows).

- [ ] **Step 1: The controller measures** (release build):
  - S8 exe size;
  - S6 cold start;
  - S7 idle memory;
  - S7 memory during a 14k-file upload to `httporter-test-sg` (`Get-Process` private bytes, sampled every second, max reported);
  - S1 files/s for the reference export `D:\site-export` uploaded to the empty test zone;
  - S10 on the R2 test bucket: content types of html/css/js/webp served correctly, plus files/s;
  - S9 (wrong S3 secret and wrong Bunny password → message within 2 s, one request).
  - Screenshots of the drawer, the conflict sheet, the delete alert, Settings, Add Site (S3) and Welcome, light and dark, via `tools/measure.ps1`.
- [ ] **Step 2: Hands-on checks by the user:**
  1. drop a folder onto a folder row;
  2. drag between panes both ways;
  3. paste from Explorer;
  4. conflict sheet with "Remember";
  5. delete a folder (counts shown, Cancel is the default);
  6. new folder on Bunny and on R2;
  7. pause / resume / cancel / retry failed;
  8. kill mid-upload → resume (S5);
  9. Wi-Fi off/on and sleep/wake mid-upload;
  10. taskbar progress and the finish notification with HTTPorter in the background;
  11. Settings: simultaneous transfers 8 visibly limits active rows; bandwidth limit 1 MB/s visibly limits speed;
  12. edit, duplicate and remove a site (Credential Manager entry gone);
  13. account-key sign-in adds zones and forgets the key;
  14. Welcome on a fresh profile (rename `%APPDATA%\HTTPorter` temporarily; the user does this);
  15. import `.env` (a copy of the uploader's `.env` the user chooses) and an AWS profile.
- [ ] **Step 3:** Write `docs/acceptance/I2.md`: measurements, user results, deviations with reasons. Tick the checklist.
- [ ] **Step 4: Commit** `docs: I2 acceptance results`.

---

## Ledger items not in I2 (recorded so none is lost)

- **I4:** 97 (case-insensitive region compare in the inspector), 135 (mono font fallback).
- **I5 accessibility and polish:** 128, 130, 131, 133, 153–155, 198 (sizes), 202, 219.
- **Backlog:** 61, 62, 64, 65, 76, 78, 90, 95, 96, 102, 110, 112, 118, 119, 141, 142, 144, 162, 166, 182.
- **Closed as already resolved:** 94, 134, 145, 165, 196.
