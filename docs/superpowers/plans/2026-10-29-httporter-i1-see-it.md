# HTTPorter I1 "See it" Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native HTTPorter window (Rust + Slint, Cupertino look) that adds a Bunny Storage site, keeps its password in Windows Credential Manager, and browses the local PC and the Bunny zone side by side over HTTPS.

**Architecture:** A Cargo workspace of four crates. `httporter-core` holds paths, entries, errors, the `StorageProvider` trait, a mock provider and the retry policy. `httporter-bunny` is the Bunny Storage HTTP client. `httporter-platform` holds the credential store and the SQLite store. `httporter-app` is the Slint UI plus a `Session` controller that returns plain view structs, which a thin binding layer applies to the window. UI runs on the main thread; network and disk work run on tokio.

**Tech Stack:** Rust stable, edition 2024, MSVC toolchain · Slint 1.18 (`cupertino` style, software renderer, feature `unstable-winit-030`) · tokio · reqwest (rustls, http2, stream) · serde/serde_json · chrono · async-trait · thiserror · percent-encoding · sha2 + hex · bytes, futures-util, tokio-util · keyring 3 (`windows-native`) · rusqlite (`bundled`) · dirs · uuid · windows-sys. Dev: wiremock, tempfile, i-slint-backend-testing, dotenvy.

**Spec:** `docs/superpowers/specs/2026-10-29-httporter-design.md`, this plan implements its §15 tracks T1, T2 (storage part), T6 (browser part), T7 (credentials + store) and integration milestone **I1**. Visual reference: `docs/design/mockups/` (copy of the canvas docs/design/mockups/); screen `Main.dc.html` with `drawer=collapsed, overlay=none`, and `overlay=addsite` for the Add Site sheet.

**Plan series:** plan 1 of 6, one per integration milestone. Next: I2 "Move files" (S3 provider, transfer engine with adaptive concurrency and journal, queue drawer, conflicts, delete), I3 "Publish", I4 "Manage+", I5 "Polish & release", I6 "Later".

## Global Constraints

- Platform: Windows 10/11 x64. No WebView, no browser engine.
- Product name `HTTPorter`; binary `httporter.exe`; crate prefix `httporter-`.
- Slint `1.18.x`, style `cupertino`, renderer `software` by default (spike: ~15 MB private memory).
- Font Inter (SIL OFL 1.1), bundled. Icons are the project's own stroke SVGs from the mockup sprite. Never SF Pro or SF Symbols.
- Secrets only in Windows Credential Manager, target `HTTPorter/site/<site-id>`; never in SQLite, settings, logs or test output.
- App data: `%APPDATA%\HTTPorter\httporter.db`.
- Bunny limits: at most **5** concurrent listings, deletes capped at **30**, transfers capped at **96** per site.
- Retry: delays **1, 2, 4, 8, 10 s** (5 retries after the first attempt); auth failures never retried.
- Tests never write to zone `exampleshop` and never purge Pull Zone `7654321`. Live tests run only against zones whose name starts with `httporter-test`.
- UI text contrast WCAG AA; colors are exactly the tokens in Task 10. All user-facing strings wrapped in `@tr()` (Slint) for later translation.
- Budgets: `httporter.exe` ≤ **25 MB**; cold start to usable window ≤ **1 s**; idle private memory ≤ **40 MB**.
- Deviations from the mockups need a reason written in the task. Known ones in I1: the standard Windows title bar is kept (an integrated title bar needs Win32 hit-testing for snap layouts → I5); the Status column and compare chips wait for I3; TLS version isn't shown because reqwest doesn't expose it.
- Commit messages: Conventional Commits, ending with the attribution lines the session provides.

## Review Focus

- **Wrong password, zone or region** → one request only, then the exact message "Couldn't sign in to {zone}. Check the password, zone name and region." within 2 s. Tests: Task 5 `auth_failure_is_single_request`, Task 15 `auth_error_shows_exact_message`.
- **Names with spaces, Unicode, `#`, `%`, `?`, `+`** → listing works and breadcrumbs show the decoded name. Tests: Task 1 `url_encoding_per_segment`, Task 5 `list_encodes_segments`.
- **Very large folders** (1,829 real files in `wp-content/uploads/2026/10`; 50,000 synthetic) → the UI stays responsive. Tests: Task 12 `converts_50k_entries_quickly`; listings never run on the UI thread (Task 15).
- **A non-JSON answer** (proxy or captive-portal HTML, truncated body) → a clear error, no panic, no retry storm. Test: Task 5 `non_json_body_is_permanent_error`.
- **Flaky network** (reset, 5xx, 400 "busy") → retries with backoff, then a clear error instead of an endless spinner. Tests: Task 3 `gives_up_after_six_attempts`, Task 5 `server_error_then_success_retries`, Task 5 `busy_400_is_transient_and_retried`.

## Execution order and parallel tracks

1. **Tasks 1–3 (contracts), sequential.** Everything else builds on these types.
2. **Three independent tracks, in parallel** (separate worktrees or sessions; each touches only its own crate):
   - Track Bunny: Tasks 4 → 5 → 6 → 7 (`crates/httporter-bunny`)
   - Track Platform: Tasks 8 → 9 (`crates/httporter-platform`)
   - Track UI: Tasks 10 → 11 → 12 → 13 → 14 (`crates/httporter-app`, UI and pure logic only; no Bunny or platform code)
3. **Task 15 (integration)** after all three tracks are merged; then **Task 16 (acceptance)**.

## File structure

```
D:\Projects\httporter
├─ Cargo.toml                      workspace (members = crates/*, resolver 3)
├─ .gitignore                      target/, *.db, .env.test
├─ tools/measure.ps1               launch + memory + screenshot (from the spike)
├─ docs/design/mockups/            canvas copy (visual reference)
├─ docs/notes/bunny-behavior.md    Task 7 findings (B1 empty folders, B2 case)
├─ docs/acceptance/I1.md           Task 16 measurements
└─ crates/
   ├─ httporter-core/src/
   │   ├─ lib.rs          re-exports
   │   ├─ path.rs         RemotePath, encode_segment
   │   ├─ entry.rs        Entry, Checksum, ChecksumKind
   │   ├─ capabilities.rs Capabilities, EmptyFolders
   │   ├─ error.rs        ProviderError
   │   ├─ provider.rs     StorageProvider, Listing, RequestInfo, UploadSource, UploadOptions, ByteStream
   │   ├─ site.rs         SiteId, ProviderKind, SiteRecord
   │   ├─ retry.rs        RetryPolicy, with_retry
   │   └─ mock.rs         MockProvider, MockOp
   ├─ httporter-bunny/
   │   ├─ src/lib.rs      re-exports
   │   ├─ src/regions.rs  REGIONS, storage_host
   │   ├─ src/model.rs    BunnyObject, to_entry
   │   ├─ src/storage.rs  BunnyConfig, BunnyStorage
   │   ├─ tests/fixtures/list_root.json
   │   ├─ tests/storage_list.rs, tests/storage_ops.rs
   │   └─ tests/live.rs   #[ignore] behavior probe
   ├─ httporter-platform/src/
   │   ├─ lib.rs
   │   ├─ creds.rs        CredentialStore, CredentialKey, KeyringStore, MemoryStore
   │   └─ store.rs        Store, default_db_path
   └─ httporter-app/
       ├─ build.rs        cupertino style
       ├─ ui/app.slint, ui/theme.slint
       ├─ ui/components/{toolbar,sidebar,pane,statusbar,add_site,chips}.slint
       ├─ ui/icons/*.svg, ui/fonts/Inter-*.ttf + OFL.txt
       └─ src/
           ├─ main.rs        backend selection, runtime, binding
           ├─ format.rs      format_size, format_date
           ├─ sort.rs        SortKey, sort_rows
           ├─ local_fs.rs    list_local, list_drives
           ├─ viewmodel.rs   RowData, FileKindData, rows_from_remote, rows_from_local, crumbs
           ├─ add_site.rs    AddSiteForm, ValidSite, FieldError
           ├─ drop_hook.rs   DropTracker, PaneSide, install_drop_hook
           └─ session.rs     Session, ProviderFactory, RemoteView, TestResult
```

---

### Task 1: Workspace, repository and `RemotePath`

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `crates/httporter-core/Cargo.toml`, `crates/httporter-core/src/lib.rs`, `crates/httporter-core/src/path.rs`

**Interfaces:**
- Produces: `RemotePath` (`root()`, `parse(&str) -> Result<Self, PathError>`, `join(&self, name: &str) -> Result<Self, PathError>`, `parent(&self) -> Option<Self>`, `name(&self) -> Option<&str>`, `is_root(&self) -> bool`, `as_str(&self) -> &str`, `segments(&self) -> impl Iterator<Item = &str>`, `to_url_path(&self, trailing_slash: bool) -> String`); `encode_segment(&str) -> String`; `PathError` (thiserror). `RemotePath` derives `Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord`.

- [ ] **Step 1: Initialise the repository and commit the existing docs**

```bash
cd D:/Projects/httporter
git init
printf "target/\n*.db\n.env.test\n" > .gitignore
git add .gitignore docs tools
git commit -m "docs: HTTPorter design, mockups and measure script"
```

- [ ] **Step 2: Create the workspace and the core crate**

`Cargo.toml`: `[workspace] members = ["crates/*"]`, `resolver = "3"`, `[workspace.package] edition = "2024"`, `version = "0.1.0"`, `publish = false`. Then `cargo new --lib crates/httporter-core` and `cargo add -p httporter-core thiserror percent-encoding`.

- [ ] **Step 3: Write the failing tests** (in `path.rs`, `#[cfg(test)] mod tests`)

```rust
#[test] fn parse_normalizes_slashes() {
    assert_eq!(RemotePath::parse("/wp-content//uploads/").unwrap().as_str(), "wp-content/uploads");
}
#[test] fn parse_rejects_dot_segments() {
    assert!(RemotePath::parse("a/../b").is_err());
    assert!(RemotePath::parse("./a").is_err());
}
#[test] fn root_properties() {
    let r = RemotePath::root();
    assert!(r.is_root());
    assert_eq!(r.parent(), None);
    assert_eq!(r.to_url_path(true), "");
}
#[test] fn join_parent_name() {
    let p = RemotePath::root().join("wp-content").unwrap().join("uploads").unwrap();
    assert_eq!(p.name(), Some("uploads"));
    assert_eq!(p.parent().unwrap().as_str(), "wp-content");
    assert!(RemotePath::root().join("a/b").is_err());
    assert!(RemotePath::root().join("..").is_err());
}
#[test] fn url_encoding_per_segment() {
    let p = RemotePath::parse("a/x y#?%+é").unwrap();
    assert_eq!(p.to_url_path(false), "a/x%20y%23%3F%25%2B%C3%A9");
    assert_eq!(p.to_url_path(true), "a/x%20y%23%3F%25%2B%C3%A9/");
}
#[test] fn unreserved_chars_untouched() {
    assert_eq!(RemotePath::parse("1-ge_bidi.120km~x").unwrap().to_url_path(false), "1-ge_bidi.120km~x");
}
```

- [ ] **Step 4: Run to see them fail**

Run: `cargo test -p httporter-core`
Expected: compile errors (`RemotePath` not found).

- [ ] **Step 5: Implement `path.rs`**

`encode_segment` keeps only RFC 3986 unreserved characters (`A–Z a–z 0–9 - . _ ~`) and percent-encodes every other byte of the UTF-8 encoding in uppercase hex. `parse` splits on `/`, drops empty segments, rejects `.` and `..`.

- [ ] **Step 6: Run to see them pass**

Run: `cargo test -p httporter-core`
Expected: 6 passed.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/httporter-core
git commit -m "feat(core): workspace and RemotePath with per-segment URL encoding"
```

---

### Task 2: Core contracts and `MockProvider`

**Files:**
- Create: `crates/httporter-core/src/{entry.rs,capabilities.rs,error.rs,provider.rs,site.rs,mock.rs}`
- Modify: `crates/httporter-core/src/lib.rs` (modules + re-exports)

**Interfaces:**
- Consumes: `RemotePath` (Task 1).
- Produces:
  - `enum ChecksumKind { Sha256, Md5 }`; `struct Checksum { pub kind: ChecksumKind, pub hex: String }` with `Checksum::sha256_hex(&str) -> Result<Checksum, ProviderError>` (validates 64 hex chars, stores lowercase).
  - `struct Entry { pub path: RemotePath, pub is_folder: bool, pub size: u64, pub modified: Option<chrono::DateTime<Utc>>, pub checksum: Option<Checksum>, pub content_type: Option<String>, pub replicated_regions: Vec<String> }` with `fn name(&self) -> &str`.
  - `enum EmptyFolders { Supported, Markers, Unsupported, Unknown }`; `struct Capabilities { pub checksum_in_listing: Option<ChecksumKind>, pub server_side_copy: bool, pub recursive_delete: bool, pub batch_delete_max: u32, pub empty_folders: EmptyFolders, pub list_concurrency: usize, pub delete_concurrency: usize, pub transfer_concurrency_max: usize, pub presign: bool, pub read_only: bool }` with `Default` (None, false, false, 1, Unknown, 4, 4, 32, false, false).
  - `enum ProviderError { Auth, NotFound, RateLimited { retry_after: Option<Duration> }, ChecksumMismatch, Refused(String), Transient(String), Permanent(String) }` deriving `Debug, Clone, PartialEq, thiserror::Error`, with `fn is_retryable(&self) -> bool` (true only for `Transient` and `RateLimited`).
  - `type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>`; `enum UploadSource { Bytes(Bytes), File { path: PathBuf, len: u64 } }`; `struct UploadOptions { pub content_type: String, pub sha256: Option<[u8; 32]> }`; `struct RequestInfo { pub latency: Duration, pub http_version: String }`; `struct Listing { pub entries: Vec<Entry>, pub info: RequestInfo }`.
  - `#[async_trait] trait StorageProvider: Send + Sync { fn capabilities(&self) -> &Capabilities; fn endpoint(&self) -> String; async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError>; async fn upload(&self, path: &RemotePath, source: UploadSource, opts: &UploadOptions) -> Result<(), ProviderError>; async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError>; async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError>; async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError>; }`
  - `struct SiteId(pub String)` with `SiteId::new()` (uuid v4) and `Display`; `enum ProviderKind { Bunny }`; `struct SiteRecord { pub id: SiteId, pub name: String, pub provider: ProviderKind, pub zone: String, pub region: String, pub host: String, pub created_at: DateTime<Utc> }`.
  - `enum MockOp { List, Upload, Download, DeleteFile, DeleteFolder }`; `MockProvider::new(Capabilities)`, `.with_file(&str, &[u8]) -> Self`, `.with_folder(&str) -> Self`, `.fail_next(MockOp, ProviderError, times: usize)`, `.calls(MockOp) -> usize`. Its `list` returns direct children sorted by name, files carrying a SHA-256 checksum; `RequestInfo { latency: 0 ms, http_version: "HTTP/2" }`; `endpoint()` = `"mock://"`.

- [ ] **Step 1: Add dependencies**

`cargo add -p httporter-core async-trait bytes chrono futures-util sha2 hex uuid --features uuid/v4` and `cargo add -p httporter-core --dev tokio --features tokio/macros,tokio/rt-multi-thread,tokio/test-util`.

- [ ] **Step 2: Write the failing tests**

```rust
// error.rs
#[test] fn only_transient_and_rate_limited_retry() {
    assert!(ProviderError::Transient("x".into()).is_retryable());
    assert!(ProviderError::RateLimited { retry_after: None }.is_retryable());
    for e in [ProviderError::Auth, ProviderError::NotFound, ProviderError::ChecksumMismatch,
              ProviderError::Refused("x".into()), ProviderError::Permanent("x".into())] {
        assert!(!e.is_retryable(), "{e:?}");
    }
}
// entry.rs
#[test] fn checksum_normalizes_and_validates() {
    let c = Checksum::sha256_hex("B952DEDC8ACB32567C6A1409077036C7D0F371C85A4D75BCD667E4740B75F991").unwrap();
    assert_eq!(c.hex, "b952dedc8acb32567c6a1409077036c7d0f371c85a4d75bcd667e4740b75f991");
    assert!(Checksum::sha256_hex("abc").is_err());
}
// mock.rs
#[tokio::test] async fn mock_lists_direct_children_only() {
    let m = MockProvider::new(Capabilities::default())
        .with_file("a/b.txt", b"hi").with_file("a/c/d.txt", b"x").with_file("e.txt", b"y");
    let l = m.list(&RemotePath::parse("a").unwrap()).await.unwrap();
    let got: Vec<_> = l.entries.iter().map(|e| (e.name().to_string(), e.is_folder)).collect();
    assert_eq!(got, vec![("b.txt".to_string(), false), ("c".to_string(), true)]);
}
#[tokio::test] async fn mock_upload_then_download_roundtrip() { /* upload Bytes "hello" to "x/y.txt", download, collect == b"hello" */ }
#[tokio::test] async fn mock_fail_next_injects_errors() {
    let m = MockProvider::new(Capabilities::default());
    m.fail_next(MockOp::List, ProviderError::Auth, 1);
    assert_eq!(m.list(&RemotePath::root()).await.unwrap_err(), ProviderError::Auth);
    assert!(m.list(&RemotePath::root()).await.is_ok());
    assert_eq!(m.calls(MockOp::List), 2);
}
#[tokio::test] async fn mock_delete_folder_is_recursive() { /* with_file("a/b/c.txt"), delete_folder("a"), list root is empty */ }
```

- [ ] **Step 3: Run to see them fail**, `cargo test -p httporter-core` → compile errors.
- [ ] **Step 4: Implement the modules** (mock state in `Mutex<BTreeMap<RemotePath, Node>>`, where `Node` is a file's bytes or a folder; folders are implied by file paths and by `with_folder`).
- [ ] **Step 5: Run to see them pass**, `cargo test -p httporter-core` → all pass.
- [ ] **Step 6: Commit**, `git commit -m "feat(core): provider contracts, entries, errors and mock provider"`

---

### Task 3: Retry policy

**Files:**
- Create: `crates/httporter-core/src/retry.rs`

**Interfaces:**
- Consumes: `ProviderError` (Task 2).
- Produces: `struct RetryPolicy { pub delays: Vec<Duration> }` with `Default` = `[1, 2, 4, 8, 10]` s and `RetryPolicy::immediate(retries: usize)` (zero delays, for tests); `async fn with_retry<T, F, Fut>(policy: &RetryPolicy, op: F) -> Result<T, ProviderError> where F: FnMut(u32) -> Fut, Fut: Future<Output = Result<T, ProviderError>>` (the argument is the 0-based attempt number). It waits `max(delay, retry_after)` before retrying a `RateLimited` error and returns non-retryable errors immediately.

- [ ] **Step 1: Write the failing tests** (`#[tokio::test(start_paused = true)]`)

```rust
#[tokio::test(start_paused = true)] async fn gives_up_after_six_attempts() {
    let calls = AtomicU32::new(0);
    let start = tokio::time::Instant::now();
    let r: Result<(), _> = with_retry(&RetryPolicy::default(), |_| { calls.fetch_add(1, SeqCst);
        async { Err(ProviderError::Transient("reset".into())) } }).await;
    assert!(matches!(r, Err(ProviderError::Transient(_))));
    assert_eq!(calls.load(SeqCst), 6);
    assert_eq!(start.elapsed(), Duration::from_secs(25));
}
#[tokio::test(start_paused = true)] async fn non_retryable_returns_immediately() { /* Auth → 1 call, elapsed 0 */ }
#[tokio::test(start_paused = true)] async fn honors_retry_after_when_longer() {
    /* attempt 0 → RateLimited { retry_after: Some(30s) }, attempt 1 → Ok(7); result Ok(7); elapsed == 30 s */
}
#[tokio::test(start_paused = true)] async fn succeeds_after_transient() { /* attempt 0 Transient, 1 Ok → elapsed 1 s */ }
```

- [ ] **Step 2: Run to see them fail**, `cargo test -p httporter-core retry` → compile error.
- [ ] **Step 3: Implement `retry.rs`** with `tokio::time::sleep` (add `tokio` with feature `time` as a normal dependency).
- [ ] **Step 4: Run to see them pass**, `cargo test -p httporter-core` → all pass.
- [ ] **Step 5: Commit**, `git commit -m "feat(core): retry policy with backoff and Retry-After"`

---

### Task 4: Bunny regions and listing model *(Track Bunny)*

**Files:**
- Create: `crates/httporter-bunny/Cargo.toml`, `src/lib.rs`, `src/regions.rs`, `src/model.rs`, `tests/fixtures/list_root.json`

**Interfaces:**
- Consumes: `RemotePath`, `Entry`, `Checksum`, `ProviderError` (core).
- Produces: `pub const REGIONS: [(&str, &str); 9]` = `("de","Frankfurt"), ("uk","London"), ("ny","New York"), ("la","Los Angeles"), ("sg","Singapore"), ("se","Stockholm"), ("br","São Paulo"), ("jh","Johannesburg"), ("syd","Sydney")`; `pub fn storage_host(code: &str) -> Option<String>`; `#[derive(Deserialize)] #[serde(rename_all = "PascalCase")] pub struct BunnyObject { pub object_name: String, pub length: u64, pub is_directory: bool, pub last_changed: String, pub checksum: Option<String>, pub content_type: Option<String>, pub replicated_zones: Option<String> }`; `pub fn to_entry(obj: BunnyObject, dir: &RemotePath) -> Result<Entry, ProviderError>`.

- [ ] **Step 1: Create the crate and the fixture**

`cargo new --lib crates/httporter-bunny`; add `httporter-core` (path), `serde` (derive), `serde_json`, `chrono`. Fixture `tests/fixtures/list_root.json`. These are real field values from `exampleshop`, with the account's IDs replaced by zeros:

```json
[
 {"Guid":"00000000-0000-0000-0000-000000000001","StorageZoneName":"testzone","Path":"/testzone/","ObjectName":"author-sitemap.xml","Length":688,"Checksum":"B952DEDC8ACB32567C6A1409077036C7D0F371C85A4D75BCD667E4740B75F991","ReplicatedZones":"LA,DE,BR","LastChanged":"2026-10-28T10:02:51.089","ServerId":0,"ArrayNumber":0,"IsDirectory":false,"UserId":"00000000-0000-0000-0000-000000000000","ContentType":"","DateCreated":"2026-10-28T10:02:51.089","StorageZoneId":1},
 {"Guid":"00000000-0000-0000-0000-000000000002","StorageZoneName":"testzone","Path":"/testzone/","ObjectName":"wp-content","Length":0,"Checksum":null,"ReplicatedZones":null,"LastChanged":"2026-10-28T10:02:56.824","ServerId":0,"ArrayNumber":0,"IsDirectory":true,"UserId":"00000000-0000-0000-0000-000000000000","ContentType":"","DateCreated":"2026-10-28T10:02:56.824","StorageZoneId":1}
]
```

- [ ] **Step 2: Write the failing tests** (in `model.rs` and `regions.rs`)

```rust
#[test] fn hosts_per_region() {
    assert_eq!(storage_host("de").as_deref(), Some("storage.bunnycdn.com"));
    assert_eq!(storage_host("sg").as_deref(), Some("sg.storage.bunnycdn.com"));
    assert_eq!(storage_host("syd").as_deref(), Some("syd.storage.bunnycdn.com"));
    assert_eq!(storage_host("xx"), None);
}
#[test] fn maps_listing_fixture() {
    let objs: Vec<BunnyObject> = serde_json::from_str(include_str!("../tests/fixtures/list_root.json")).unwrap();
    let e: Vec<Entry> = objs.into_iter().map(|o| to_entry(o, &RemotePath::root())).collect::<Result<_, _>>().unwrap();
    assert_eq!(e[0].name(), "author-sitemap.xml");
    assert_eq!(e[0].size, 688);
    assert_eq!(e[0].checksum.as_ref().unwrap().hex, "b952dedc8acb32567c6a1409077036c7d0f371c85a4d75bcd667e4740b75f991");
    assert_eq!(e[0].modified.unwrap().to_rfc3339(), "2026-10-28T10:02:51.089+00:00");
    assert_eq!(e[0].replicated_regions, vec!["LA", "DE", "BR"]);
    assert_eq!(e[0].content_type, None);
    assert!(e[1].is_folder && e[1].checksum.is_none() && e[1].name() == "wp-content");
}
#[test] fn rejects_object_names_with_slash() {
    /* ObjectName "a/b" → Err(ProviderError::Permanent(_)) */
}
```

- [ ] **Step 3: Run to see them fail**, `cargo test -p httporter-bunny` → compile errors.
- [ ] **Step 4: Implement**, `LastChanged` has no zone: parse with `%Y-%m-%dT%H:%M:%S%.f` as UTC. An empty `ContentType` becomes `None`. Split `ReplicatedZones` on `,`, trim, and drop empty parts.
- [ ] **Step 5: Run to see them pass**, `cargo test -p httporter-bunny`.
- [ ] **Step 6: Commit**, `git commit -m "feat(bunny): regions and listing model"`

---

### Task 5: `BunnyStorage` listing, limits and error mapping *(Track Bunny)*

**Files:**
- Create: `crates/httporter-bunny/src/storage.rs`, `crates/httporter-bunny/tests/storage_list.rs`

**Interfaces:**
- Consumes: Task 2 contracts, Task 3 `with_retry`, Task 4 `to_entry`, `storage_host`.
- Produces: `pub struct BunnyConfig { pub zone: String, pub host: String, pub password: String, pub transfer_concurrency: usize, pub delete_concurrency: usize, pub retry: RetryPolicy, pub base_url: Option<String> }` with `BunnyConfig::new(zone, host, password)` (defaults 32, 16, `RetryPolicy::default()`, `None`); `pub struct BunnyStorage` with `BunnyStorage::new(BunnyConfig) -> Result<Self, ProviderError>` implementing `StorageProvider` (list in this task; other methods return `Err(ProviderError::Permanent("not implemented"))` until Task 6). `endpoint()` = `"https://{host}/{zone}/"` (or `"{base_url}/{zone}/"`).
- Capabilities: checksum `Sha256`, `server_side_copy: false`, `recursive_delete: true`, `batch_delete_max: 1`, `empty_folders: Unknown`, `list_concurrency: 5`, `delete_concurrency: min(cfg, 30)`, `transfer_concurrency_max: 96`, `presign: false`.

- [ ] **Step 1: Add dependencies**, `reqwest` (`default-features = false`, features `rustls-tls, http2, stream, json`), `tokio` (`sync, fs, time`), `async-trait`, `bytes`, `futures-util`, `tokio-util` (`io`); dev: `wiremock`, `tokio` (`macros, rt-multi-thread`), `tempfile`.

- [ ] **Step 2: Write the failing tests** (`tests/storage_list.rs`; helper `fn storage(server: &MockServer) -> BunnyStorage` with zone `testzone`, password `secret`, `retry: RetryPolicy::immediate(5)`, `base_url: Some(server.uri())`)

```rust
#[tokio::test] async fn list_root_requests_zone_with_trailing_slash_and_access_key() {
    let s = MockServer::start().await;
    Mock::given(method("GET")).and(path("/testzone/")).and(header("AccessKey", "secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string(include_str!("fixtures/list_root.json")))
        .expect(1).mount(&s).await;
    let l = storage(&s).list(&RemotePath::root()).await.unwrap();
    assert_eq!(l.entries.len(), 2);
    assert_eq!(storage(&s).endpoint(), format!("{}/testzone/", s.uri()));
}
#[tokio::test] async fn list_encodes_segments() { /* GET "/testzone/x%20y%23/" → "[]"; list("x y#") ok, 0 entries */ }
#[tokio::test] async fn auth_failure_is_single_request() {
    /* 401 with .expect(1) → Err(ProviderError::Auth) */
}
#[tokio::test] async fn server_error_then_success_retries() { /* 500 up_to_n_times(1), then 200 "[]" → Ok */ }
#[tokio::test] async fn busy_400_is_transient_and_retried() { /* 400 "Unable to process request" once, then 200 → Ok */ }
#[tokio::test] async fn non_json_body_is_permanent_error() {
    /* 200 body "<html>Login</html>", .expect(1) → Err(Permanent(m)) where m.contains("not JSON") */
}
#[tokio::test] async fn rate_limited_maps_retry_after() {
    /* retry: RetryPolicy::immediate(0); 429 with header Retry-After: 2 → Err(RateLimited { retry_after: Some(2 s) }) */
}
#[tokio::test] async fn listing_reports_latency_and_http_version() {
    /* 200 "[]" → info.http_version == "HTTP/1.1" (wiremock speaks 1.1), info.latency > 0 */
}
#[tokio::test] async fn concurrent_lists_never_exceed_five() {
    /* responder with set_delay(200 ms); 12 lists via join_all; elapsed >= 590 ms and < 1500 ms */
}
```

- [ ] **Step 3: Run to see them fail**, `cargo test -p httporter-bunny --test storage_list`.
- [ ] **Step 4: Implement listing**
  - Client: `connect_timeout(10 s)`, `timeout(120 s)`, `user_agent("HTTPorter/0.1")`, `pool_max_idle_per_host(transfer_concurrency)`.
  - A `tokio::sync::Semaphore` of 5 for listings; send `Accept: application/json`.
  - Status mapping in `fn map_status(status: StatusCode, retry_after: Option<Duration>, op: Op) -> ProviderError`:

    | Status | Error |
    |---|---|
    | 401, 403 | `Auth` |
    | 404 | `NotFound` |
    | 429 | `RateLimited` |
    | 400 on list or delete | `Transient("server busy")` |
    | 400 on upload with a checksum | `ChecksumMismatch` |
    | other 400 | `Permanent` |
    | 5xx | `Transient` |
    | anything else | `Permanent` |

  - Network errors become `Transient`. A body that isn't JSON becomes `Permanent("unexpected response from server (not JSON)")`.
  - `http_version` is `"HTTP/2"` or `"HTTP/1.1"` from `response.version()`; `latency` is send-to-body time.
- [ ] **Step 5: Run to see them pass.**
- [ ] **Step 6: Commit**, `git commit -m "feat(bunny): storage listing with limits, retries and error mapping"`

---

### Task 6: `BunnyStorage` upload, download and delete *(Track Bunny)*

**Files:**
- Modify: `crates/httporter-bunny/src/storage.rs`
- Create: `crates/httporter-bunny/tests/storage_ops.rs`

**Interfaces:**
- Consumes: Task 5 `BunnyStorage`.
- Produces: full `StorageProvider` implementation for `BunnyStorage`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn upload_sends_checksum_content_type_and_body() {
    let sha = sha2::Sha256::digest(b"hello");
    Mock::given(method("PUT")).and(path("/testzone/a/b.txt")).and(header("AccessKey", "secret"))
        .and(header("Checksum", hex::encode_upper(sha).as_str())).and(header("Content-Type", "text/plain"))
        .and(body_bytes(b"hello".to_vec()))
        .respond_with(ResponseTemplate::new(201)).expect(1).mount(&s).await;
    storage(&s).upload(&RemotePath::parse("a/b.txt").unwrap(), UploadSource::Bytes("hello".into()),
        &UploadOptions { content_type: "text/plain".into(), sha256: Some(sha.into()) }).await.unwrap();
}
#[tokio::test] async fn upload_file_source_streams_file() { /* 1 MiB temp file, body_bytes equals file contents, Content-Length 1048576 */ }
#[tokio::test] async fn upload_400_with_checksum_is_checksum_mismatch() { /* 400, .expect(1) → Err(ChecksumMismatch) */ }
#[tokio::test] async fn download_streams_body() { /* GET "/testzone/a/b.txt" → "hello"; collected stream == b"hello" */ }
#[tokio::test] async fn delete_file_and_folder_urls() { /* DELETE "/testzone/a/b.txt" 200; DELETE "/testzone/a/" 200; both expect(1) */ }
#[tokio::test] async fn delete_root_is_refused_without_request() {
    /* delete_folder(root) → Err(Refused(_)); server receives 0 requests */
}
#[tokio::test] async fn delete_missing_file_is_not_found() { /* 404 → Err(NotFound) */ }
```

- [ ] **Step 2: Run to see them fail.**
- [ ] **Step 3: Implement**
  - **Upload:** `PUT` with headers `AccessKey`, `Content-Type`, and `Checksum` = uppercase hex when `sha256` is set. A file source is read through `tokio::fs::File` + `ReaderStream` with `Content-Length` = `len`, and reopened on every retry. 200 and 201 mean success.
  - **Deletes** use their own semaphore (`delete_concurrency`, capped at 30); **transfers** use theirs (`transfer_concurrency`, capped at 96).
  - **`delete_folder(root)`** returns `Refused("Deleting the whole zone isn't allowed")` without sending a request.
- [ ] **Step 4: Run to see them pass**, `cargo test -p httporter-bunny`.
- [ ] **Step 5: Commit**, `git commit -m "feat(bunny): upload with checksum, download and delete"`

---

### Task 7: Live behavior probe on a test zone *(Track Bunny)*

**Files:**
- Create: `crates/httporter-bunny/tests/live.rs`, `docs/notes/bunny-behavior.md`
- Modify: `crates/httporter-bunny/src/storage.rs` (set `empty_folders` from the finding)

**Interfaces:**
- Consumes: Task 6 `BunnyStorage`.
- Produces: documented answers to spec items **B1** (empty folders) and **B2** (case sensitivity); `Capabilities.empty_folders` set accordingly.

- [ ] **Step 1: User step (blocking): create the test zone.** The user creates storage zone `httporter-test-sg` (region Singapore) in the Bunny dashboard. They then create `D:\Projects\httporter\.env.test` with `HTTPORTER_TEST_ZONE`, `HTTPORTER_TEST_REGION=sg` and `HTTPORTER_TEST_PASSWORD`. The file is git-ignored. The agent never opens or prints it.
- [ ] **Step 2: Write the probe**, `#[ignore]` tests load `.env.test` with `dotenvy` and refuse to run (`panic!`) unless the zone starts with `httporter-test`. Each test name is the finding it reports:
  - `probe_upload_list_download_delete`: round-trip of `probe/hello.txt` with checksum.
  - `probe_empty_folder_put`: `PUT /{zone}/probe/empty/` with an empty body, then list `probe/` and record whether `empty` appears.
  - `probe_case_sensitivity`: upload `probe/Case.txt`, then `GET probe/case.txt`; record 200 vs 404.

  Every test deletes `probe/` at the end.
- [ ] **Step 3: Run it**, `cargo test -p httporter-bunny --test live -- --ignored --nocapture`. Expected: each test prints its finding; all pass.
- [ ] **Step 4: Record the findings** in `docs/notes/bunny-behavior.md` (date, zone region, exact HTTP statuses). Set `empty_folders` to `Supported` or `Unsupported` to match, and update the Task 5 capability test.
- [ ] **Step 5: Commit**, `git commit -m "test(bunny): live behavior probe; document empty folders and case sensitivity"`

---

### Task 8: Credential store *(Track Platform)*

**Files:**
- Create: `crates/httporter-platform/Cargo.toml`, `src/lib.rs`, `src/creds.rs`

**Interfaces:**
- Consumes: `SiteId` (core).
- Produces: `pub struct CredentialKey(String)` with `CredentialKey::site(&SiteId)` → `"HTTPorter/site/{id}"` and `as_str()`; `pub trait CredentialStore: Send + Sync { fn set(&self, key: &CredentialKey, secret: &str) -> Result<(), CredError>; fn get(&self, key: &CredentialKey) -> Result<Option<String>, CredError>; fn delete(&self, key: &CredentialKey) -> Result<(), CredError>; }`; `KeyringStore` (keyring 3, `Entry::new_with_target(key, "HTTPorter", key)` so the Windows target name is exactly the key); `MemoryStore`. `delete` on a missing key is `Ok`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn site_key_format() { assert_eq!(CredentialKey::site(&SiteId("abc".into())).as_str(), "HTTPorter/site/abc"); }
#[test] fn memory_roundtrip_and_delete() { /* set, get == Some, delete, get == None, delete again Ok */ }
#[test] fn keyring_roundtrip_on_windows() {
    let key = CredentialKey::site(&SiteId::new());
    let s = KeyringStore;
    s.set(&key, "pw-test").unwrap();
    assert_eq!(s.get(&key).unwrap().as_deref(), Some("pw-test"));
    s.delete(&key).unwrap();
    assert_eq!(s.get(&key).unwrap(), None);
}
```

- [ ] **Step 2: Run to see them fail**, `cargo test -p httporter-platform`.
- [ ] **Step 3: Implement** (`keyring` with feature `windows-native`; map `keyring::Error::NoEntry` to `Ok(None)` / `Ok(())`).
- [ ] **Step 4: Run to see them pass.**
- [ ] **Step 5: Commit**, `git commit -m "feat(platform): credential store on Windows Credential Manager"`

---

### Task 9: SQLite store for sites *(Track Platform)*

**Files:**
- Create: `crates/httporter-platform/src/store.rs`

**Interfaces:**
- Consumes: `SiteRecord`, `SiteId`, `ProviderKind` (core).
- Produces: `pub struct Store` with `open(&Path) -> Result<Store, StoreError>` (creates parent folders, runs migrations tracked by `PRAGMA user_version`; version 1 creates `sites(id TEXT PRIMARY KEY, name TEXT NOT NULL, provider TEXT NOT NULL, zone TEXT NOT NULL, region TEXT NOT NULL, host TEXT NOT NULL, created_at TEXT NOT NULL)`), `open_in_memory()`, `add_site(&SiteRecord)`, `list_sites() -> Vec<SiteRecord>` (ordered by `name COLLATE NOCASE`), `remove_site(&SiteId)`; `pub fn default_db_path() -> PathBuf` = `dirs::config_dir()/HTTPorter/httporter.db`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn add_list_sorted_and_remove() { /* add "zeta" then "Alpha" → list names ["Alpha","zeta"]; remove Alpha → ["zeta"] */ }
#[test] fn reopening_keeps_data_and_schema_version() { /* tempdir db: open, add, drop, open → 1 site; user_version == 1 */ }
#[test] fn no_secret_columns() {
    let st = Store::open_in_memory().unwrap();
    let cols = st.column_names("sites").unwrap();
    assert!(cols.iter().all(|c| !c.contains("pass") && !c.contains("key") && !c.contains("secret")));
}
#[test] fn default_path_is_under_appdata() { assert!(default_db_path().ends_with("HTTPorter\\httporter.db")); }
```
(`column_names` is a `#[cfg(test)]` helper using `PRAGMA table_info`.)

- [ ] **Step 2: Run to see them fail.**
- [ ] **Step 3: Implement** (`rusqlite` with `bundled`, `chrono` RFC 3339 strings, `dirs`).
- [ ] **Step 4: Run to see them pass.**
- [ ] **Step 5: Commit**, `git commit -m "feat(platform): SQLite store for sites"`

---

### Task 10: App shell, theme, fonts and icons *(Track UI)*

**Files:**
- Create: `crates/httporter-app/Cargo.toml` (`[[bin]] name = "httporter"`), `build.rs`, `ui/app.slint`, `ui/theme.slint`, `ui/components/{toolbar,sidebar,statusbar,chips}.slint`, `ui/icons/*.svg`, `ui/fonts/Inter-{Regular,Medium,SemiBold,Bold}.ttf`, `ui/fonts/OFL.txt`, `src/main.rs`

**Interfaces:**
- Produces (Slint, exported from `app.slint`):
  - `enum FileKind { folder, code, image, doc }`
  - `struct FileRow { name: string, kind: FileKind, size: string, modified: string, folder: bool }`
  - `struct SiteItem { id: string, name: string, subtitle: string, mono: string, healthy: bool }`
  - `component AppWindow` with properties `sites: [SiteItem]`, `selected-site: int`, `connection-text: string`, `connection-ok: bool`, `endpoint-text: string`, `status-left: string`, `status-right: string`, `error-text: string`. Pane, sheet and drop properties are added by Tasks 12–14.
  - `global Theme` holding the tokens below.

- [ ] **Step 1: Assets**
  - **Fonts:** download Inter 4.1 from the official release `https://github.com/rsms/inter/releases/tag/v4.1`. Copy `extras/ttf/Inter-{Regular,Medium,SemiBold,Bold}.ttf` and `LICENSE.txt` (saved as `OFL.txt`) into `ui/fonts/`.
  - **Icons:** create one SVG per `<symbol>` in `docs/design/mockups/Main.dc.html` and `History.dc.html` (folder, code, doc, image, chev-r, chev-l, chev-u, sort, search, up, down, trash, newfolder, plus, sidebar, columns, inspector, cloud, pc, send, star, pencil, gear, lock, compare, refresh, x, check, warn, eye, globe, key, open, pause, clock, filter). Use the same `viewBox` and paths, with `fill="none" stroke="#000" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"`. Tint at use with `colorize`.
- [ ] **Step 2: Theme tokens**, `ui/theme.slint`, `export global Theme { out property <bool> dark: Palette.color-scheme == ColorScheme.dark; … }`, values copied exactly from `docs/design/mockups/app.css`:

  | Token | Light | Dark |
  |---|---|---|
  | accent (fills) | #0071e3 | #0071e3 |
  | accent-text | #0066cc | #5aa9ff |
  | red / green / orange / purple | #c4001a / #1d7a35 / #b03000 / #7a3aa0 | #ff7a72 / #4cd964 / #ffad33 / #d39cff |
  | folder | #2f8fe8 | #5aa9ff |
  | label / label2 / label3 | #1d1d1f / #5c5c62 / #8e8e93 | #f5f5f7 / #a8a8ad / #6e6e73 |
  | win / chrome / content / sidebar / row-alt | #f2f2f5 / #f7f7f9 / #ffffff / #eaeaee / #f7f7f9 | #1b1b1d / #232325 / #1e1e20 / #2a2a2d / #232326 |
  | sep / sep-strong | #00000017 / #00000029 | #ffffff17 / #ffffff29 |
  | control / field | #ffffff / #ffffff | #3a3a3d / #2c2c2f |
  | sel-quiet | #00000012 | #ffffff1a |

  Sizes: body 13 px, small 11 px, title 15 px semibold, row height 28 px, sidebar item 28 px, corner radii 8 / 12 / 14 / 18 px, capsule buttons fully rounded.
- [ ] **Step 3: Write the failing smoke test** (`src/main.rs` `#[cfg(test)]`, using `i-slint-backend-testing`)

```rust
#[test] fn window_builds_with_defaults() {
    i_slint_backend_testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    assert_eq!(w.get_connection_text(), "");
    assert_eq!(w.get_sites().row_count(), 0);
}
```

- [ ] **Step 4: Run to see it fail**, `cargo test -p httporter-app`.
- [ ] **Step 5: Implement the shell**
  - **Build:** `build.rs` compiles with `slint_build::CompilerConfiguration::new().with_style("cupertino".into())`.
  - **`main.rs`:** select the software renderer before creating the window (`slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select()?`; check the exact API against docs.rs for slint 1.18). Window title "HTTPorter", default 1280×800, minimum 960×600.
  - **`app.slint`:** layout as in the mockup, with the toolbar row (sidebar toggle, back/forward capsule, title + subtitle, connection pill, Publish disabled), the sidebar, the main area and the status bar.
  - **Font:** `import "fonts/Inter-Regular.ttf";` (and the other weights), with `default-font-family: "Inter"`.
- [ ] **Step 6: Run to see it pass**, then `cargo run -p httporter-app --release`, and compare the window with `Main.dc.html` by eye (shell only).
- [ ] **Step 7: Commit**, `git commit -m "feat(app): Cupertino window shell, theme tokens, Inter and icons"`

---

### Task 11: Formatting, sorting and local listing *(Track UI)*

**Files:**
- Create: `crates/httporter-app/src/{format.rs,sort.rs,local_fs.rs}`

**Interfaces:**
- Produces:
  - `pub fn format_size(bytes: u64) -> String` (1024 base: `B`, `KB`, `MB`, `GB`, one decimal above bytes).
  - `pub fn format_date(t: NaiveDateTime, now: NaiveDateTime) -> String` → `"Today HH:MM"` / `"Yesterday HH:MM"` / `"D Mon YYYY"`.
  - `pub enum SortKey { Name, Size, Modified }`; `pub fn sort_rows<T: Sortable>(rows: &mut [T], key: SortKey, ascending: bool)`, where `trait Sortable { fn is_folder(&self) -> bool; fn name(&self) -> &str; fn size(&self) -> u64; fn modified(&self) -> Option<NaiveDateTime>; }`. Folders always come first; names compare case-insensitively.
  - `pub struct LocalEntry { pub name: String, pub is_folder: bool, pub size: u64, pub modified: Option<NaiveDateTime> }`; `pub struct LocalListing { pub entries: Vec<LocalEntry>, pub skipped: usize }`; `pub fn list_local(dir: &Path) -> io::Result<LocalListing>` (entries whose metadata fails are counted in `skipped`, not fatal); `pub fn list_drives() -> Vec<PathBuf>` (via `GetLogicalDrives`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn sizes() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(857), "857 B");
    assert_eq!(format_size(392_641), "383.4 KB");
    assert_eq!(format_size(6_725_584), "6.4 MB");
    assert_eq!(format_size(2_147_483_648), "2.0 GB");
}
#[test] fn dates() {
    let now = dt("2026-10-29 12:00");
    assert_eq!(format_date(dt("2026-10-29 10:02"), now), "Today 10:02");
    assert_eq!(format_date(dt("2026-10-28 17:02"), now), "Yesterday 17:02");
    assert_eq!(format_date(dt("2026-03-12 09:00"), now), "12 Mar 2026");
}
#[test] fn folders_first_then_case_insensitive_names() { /* ["index.html","Wp-content"(folder),"about"(folder),"B.txt"] → ["about","Wp-content","B.txt","index.html"] */ }
#[test] fn descending_keeps_folders_first() { /* by Size descending: folders first, then files largest first */ }
#[test] fn lists_temp_folder() { /* tempdir with a.txt (3 bytes) and sub/ → 2 entries, sub is_folder, a.txt size 3, skipped 0 */ }
#[test] fn drives_include_c() { assert!(list_drives().iter().any(|d| d.as_os_str() == "C:\\")); }
```

- [ ] **Step 2: Run to see them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to see them pass.**
- [ ] **Step 5: Commit**, `git commit -m "feat(app): size/date formatting, sorting and local listing"`

---

### Task 12: Pane component and view model *(Track UI)*

**Files:**
- Create: `crates/httporter-app/src/viewmodel.rs`, `crates/httporter-app/ui/components/pane.slint`
- Modify: `crates/httporter-app/ui/app.slint`

**Interfaces:**
- Consumes: Task 11 `format_*`, `sort_rows`, `LocalEntry`; core `Entry`, `RemotePath`.
- Produces:
  - Rust: `pub enum FileKindData { Folder, Code, Image, Doc }`; `pub struct RowData { pub name: String, pub kind: FileKindData, pub size: String, pub modified: String, pub folder: bool }`; `pub fn kind_for(name: &str, folder: bool) -> FileKindData` (Code: html, htm, css, js, mjs, json, xml, xsl; Image: png, jpg, jpeg, webp, gif, svg, avif, ico; everything else Doc); `pub fn rows_from_remote(entries: &[Entry], now: NaiveDateTime) -> Vec<RowData>`; `pub fn rows_from_local(entries: &[LocalEntry], now: NaiveDateTime) -> Vec<RowData>`; `pub fn remote_crumbs(zone: &str, dir: &RemotePath) -> Vec<String>`; `pub fn local_crumbs(dir: &Path) -> Vec<String>`; `pub fn to_slint_rows(&[RowData]) -> ModelRc<FileRow>`.
  - Slint `component Pane` with `in property <string> title`, `icon`, `<[string]> crumbs`, `<[FileRow]> rows`, `<string> header-chip`, `in-out property <int> selected`, `callback open(int)`, `callback up()`, `callback sort(int /*0 name,1 size,2 modified*/)`. Columns are Name, Size, Modified; the Status column arrives in I3. Rows sit in a `ListView` (virtualized), with alternating row background `row-alt`, selection fill `accent` with white text, and a folder disclosure chevron. Enter or double-click opens; Backspace goes up.
  - `AppWindow` gains `local-rows`, `remote-rows`, `local-crumbs`, `remote-crumbs`, `out property <length> pane-split` (absolute x of the remote pane's left edge, logical px), and callbacks `open-local(int)`, `open-remote(int)`, `local-up()`, `remote-up()`, `sort-local(int)`, `sort-remote(int)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn kinds() {
    assert!(matches!(kind_for("index.html", false), FileKindData::Code));
    assert!(matches!(kind_for("og-cover.webp", false), FileKindData::Image));
    assert!(matches!(kind_for("robots.txt", false), FileKindData::Doc));
    assert!(matches!(kind_for("wp-content", true), FileKindData::Folder));
}
#[test] fn remote_rows_format_and_order() { /* entries [index.html 392641, wp-content folder] → rows[0] wp-content size "-", rows[1] "383.4 KB" */ }
#[test] fn crumbs() {
    assert_eq!(remote_crumbs("exampleshop", &RemotePath::parse("wp-content/uploads").unwrap()), vec!["exampleshop", "wp-content", "uploads"]);
    assert_eq!(local_crumbs(Path::new("D:\\site-export")), vec!["D:", "site-export"]);
}
#[test] fn converts_50k_entries_quickly() {
    let entries = synthetic_entries(50_000);
    let t = Instant::now();
    let rows = rows_from_remote(&entries, now());
    assert_eq!(rows.len(), 50_000);
    assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
}
#[test] fn window_accepts_rows() { /* testing backend: set_remote_rows(3 rows) → get_remote_rows().row_count() == 3 */ }
```

- [ ] **Step 2: Run to see them fail.**
- [ ] **Step 3: Implement** `viewmodel.rs` and `pane.slint`; place two `Pane`s in a wrapping row in `app.slint`, as in the mockup (focus ring on the active pane).
- [ ] **Step 4: Run to see them pass**, then run the app with fake rows and compare with `Main.dc.html`.
- [ ] **Step 5: Commit**, `git commit -m "feat(app): file pane with virtualized list and view model"`

---

### Task 13: Sidebar sites and Add Site sheet *(Track UI)*

**Files:**
- Create: `crates/httporter-app/src/add_site.rs`, `crates/httporter-app/ui/components/add_site.slint`
- Modify: `crates/httporter-app/ui/components/sidebar.slint`, `ui/app.slint`

**Interfaces:**
- Produces:
  - Rust: `pub struct AddSiteForm { pub name: String, pub zone: String, pub region_index: i32, pub password: String }`; `pub struct ValidSite { pub name: String, pub zone: String, pub region_code: String, pub password: String }`; `pub enum FieldError { Zone(String), Region(String), Password(String) }`; `impl AddSiteForm { pub fn validate(&self, region_codes: &[&str]) -> Result<ValidSite, FieldError> }`.
  - Slint: `AddSiteSheet` (scrim + sheet as in `overlay=addsite`, Bunny only for I1). It has provider tiles with the S3 tiles disabled and labelled "Coming soon"; a password field (`input-type: password`); a read-only endpoint row showing `https://{host}/{zone}/` with a lock icon; a test result box; and Test Connection / Cancel / Add Site buttons.
  - `AppWindow` gains `add-site-open: bool`, `region-names: [string]`, `add-site-endpoint: string`, `add-site-result: string`, `add-site-result-ok: bool`, `add-site-busy: bool`, and callbacks `add-site-clicked()`, `add-site-changed(string /*zone*/, int /*region*/)`, `add-site-test(string, string, int, string)`, `add-site-save(string, string, int, string)`, `add-site-cancel()`, `select-site(int)`.

- [ ] **Step 1: Write the failing tests**

```rust
const REG: &[&str] = &["de", "sg"];
#[test] fn empty_zone() { assert_eq!(form("", 1, "pw").validate(REG), Err(FieldError::Zone("Enter the storage zone name.".into()))); }
#[test] fn zone_with_spaces() { assert_eq!(form("my zone", 1, "pw").validate(REG), Err(FieldError::Zone("Zone names can't contain spaces.".into()))); }
#[test] fn bad_region() { assert_eq!(form("z", 5, "pw").validate(REG), Err(FieldError::Region("Choose a region.".into()))); }
#[test] fn empty_password() { assert_eq!(form("z", 1, "").validate(REG), Err(FieldError::Password("Enter the zone's password.".into()))); }
#[test] fn name_defaults_to_zone_and_trims() {
    let v = AddSiteForm { name: "  ".into(), zone: " exampleshop ".into(), region_index: 1, password: "pw".into() }.validate(REG).unwrap();
    assert_eq!((v.name.as_str(), v.zone.as_str(), v.region_code.as_str()), ("exampleshop", "exampleshop", "sg"));
}
```

- [ ] **Step 2: Run to see them fail.**
- [ ] **Step 3: Implement** the form and the Slint components (sidebar "Sites" list with mono tile `B`, subtitle `Bunny · {region name}`, health dot, and "Add Site" button at the bottom). Field errors show under their field in `red`.
- [ ] **Step 4: Run to see them pass**, then open the sheet in the running app and compare with `AddSite.dc.html`.
- [ ] **Step 5: Commit**, `git commit -m "feat(app): sidebar sites and Add Site sheet"`

---

### Task 14: Explorer drop hook *(Track UI)*

**Files:**
- Create: `crates/httporter-app/src/drop_hook.rs`

**Interfaces:**
- Produces: `pub enum PaneSide { Local, Remote }`; `pub fn pane_at(x: f32, split_x: f32) -> PaneSide`; `pub struct DropTracker` with `hovered(&mut self)`, `cancelled(&mut self)`, `dropped(&mut self, path: PathBuf, x: f32) -> bool` (true for the first file of a new batch), `take(&mut self, split_x: f32) -> Option<DropBatch>`; `pub struct DropBatch { pub side: PaneSide, pub paths: Vec<PathBuf> }`; `pub fn install_drop_hook(window: &slint::Window, on_hover: impl Fn(bool) + 'static, on_drop: impl Fn(DropBatch) + 'static, split_x: impl Fn() -> f32 + 'static)`. The hook follows the spike (`D:\Projects\burrow-spike-slint\src\main.rs`): winit `HoveredFile` / `HoveredFileCancelled` / `DroppedFile` through `slint::winit_030::WinitWindowAccessor::on_winit_window_event`. The cursor x comes from `GetCursorPos` minus the window's inner position, divided by the scale factor. On the first file of a batch it schedules `slint::Timer::single_shot(Duration::ZERO, …)` to call `take`, because winit sends one event per file with no end marker. Slint feature `unstable-winit-030`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn hover_then_drop_collects_one_batch() {
    let mut t = DropTracker::default();
    t.hovered();
    assert!(t.dropped("C:\\a.txt".into(), 900.0));
    assert!(!t.dropped("C:\\b".into(), 900.0));
    let b = t.take(640.0).unwrap();
    assert!(matches!(b.side, PaneSide::Remote));
    assert_eq!(b.paths, vec![PathBuf::from("C:\\a.txt"), PathBuf::from("C:\\b")]);
    assert!(t.take(640.0).is_none());
}
#[test] fn cancel_clears_and_next_drop_starts_fresh() { /* hovered, cancelled, dropped(x=100) → take side Local, 1 path */ }
#[test] fn pane_at_split() { assert!(matches!(pane_at(639.0, 640.0), PaneSide::Local)); assert!(matches!(pane_at(640.0, 640.0), PaneSide::Remote)); }
```

- [ ] **Step 2: Run to see them fail.**
- [ ] **Step 3: Implement** `DropTracker`, `pane_at` and `install_drop_hook`. While hovering, the remote pane shows the accent ring and "Release to upload" chip.
- [ ] **Step 4: Run to see them pass.**
- [ ] **Step 5: Commit**, `git commit -m "feat(app): Explorer file-drop hook with pane targeting"`

---

### Task 15: Session controller and wiring *(Integration)*

**Files:**
- Create: `crates/httporter-app/src/session.rs`
- Modify: `crates/httporter-app/src/main.rs`, `crates/httporter-app/Cargo.toml` (depend on `httporter-bunny`, `httporter-platform`)

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `pub trait ProviderFactory: Send + Sync { fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError>; }` with `RealFactory`.
  - `pub struct RemoteView { pub rows: Vec<RowData>, pub crumbs: Vec<String>, pub connection: String, pub endpoint: String, pub status_right: String, pub error: Option<String> }`.
  - `pub struct TestResult { pub ok: bool, pub text: String }`.
  - `pub struct SiteItemData { pub id: String, pub name: String, pub subtitle: String, pub mono: String, pub healthy: bool }`, subtitle `"Bunny · {region name}"` from `REGIONS`, mono `"B"`, `healthy` = the last request to that site succeeded (unknown counts as healthy until the first request).
  - `pub struct Session` with `Session::new(store: Store, creds: Arc<dyn CredentialStore>, factory: Arc<dyn ProviderFactory>, now: fn() -> NaiveDateTime)`, `sites(&self) -> Vec<SiteRecord>`, `site_items(&self) -> Vec<SiteItemData>`, `async fn select_site(&mut self, index: usize) -> RemoteView`, `async fn open_remote(&mut self, row: usize) -> RemoteView`, `async fn remote_up(&mut self) -> RemoteView`, `async fn test_connection(&self, v: &ValidSite) -> TestResult`, `async fn save_site(&mut self, v: &ValidSite) -> Result<SiteRecord, String>`, and local navigation `open_local(row)`, `local_up()`, `local_view()`.
  - Exact copy:
    - `connection` = `"HTTPS · {ms} ms"`.
    - `status_right` = `"{http_version} · {host}"`.
    - Auth error: `"Couldn't sign in to {zone}. Check the password, zone name and region."`.
    - Test OK: `"Connected over HTTPS · {http_version} · {ms} ms · {n} items in /"`.
    - Other errors: `"Couldn't reach {host}: {error}"`.

- [ ] **Step 1: Write the failing tests** (`#[tokio::test]` with `MockProvider` via a `FakeFactory`, `MemoryStore`, `Store::open_in_memory()`, fixed `now`)

```rust
#[tokio::test] async fn save_site_keeps_password_out_of_the_database() {
    let (mut s, creds) = session_with_mock(MockProvider::new(Default::default()));
    let rec = s.save_site(&valid("testzone", "sg", "pw")).await.unwrap();
    assert_eq!(creds.get(&CredentialKey::site(&rec.id)).unwrap().as_deref(), Some("pw"));
    assert_eq!(s.sites()[0].zone, "testzone");
    assert_eq!(s.sites()[0].host, "sg.storage.bunnycdn.com");
}
#[tokio::test] async fn select_site_lists_root() {
    /* mock with "index.html" and "wp-content/x.txt" → rows ["wp-content","index.html"], crumbs ["testzone"], connection "HTTPS · 0 ms" */
}
#[tokio::test] async fn auth_error_shows_exact_message() {
    /* mock.fail_next(List, Auth, 1) → view.error == Some("Couldn't sign in to testzone. Check the password, zone name and region.") */
}
#[tokio::test] async fn navigate_into_folder_and_back() { /* open_remote(0) → crumbs ["testzone","wp-content"]; remote_up → ["testzone"] */ }
#[tokio::test] async fn failed_site_turns_unhealthy() { /* after auth_error: site_items()[0].healthy == false; subtitle == "Bunny · Singapore" */ }
#[tokio::test] async fn test_connection_text() { /* → ok, "Connected over HTTPS · HTTP/2 · 0 ms · 2 items in /" */ }
```

- [ ] **Step 2: Run to see them fail.**
- [ ] **Step 3: Implement `Session`**
  - **Site password:** read from `creds` on `select_site`. A missing password produces an error view: "No password saved for {name}. Edit the site to add it."
  - **Binding in `main.rs`:**
    - Build a multi-thread tokio runtime and `Arc<tokio::sync::Mutex<Session>>`.
    - Each UI callback spawns a task and applies the view with `ui_weak.upgrade_in_event_loop(...)`. Listing never runs on the UI thread.
    - Local listings use `spawn_blocking`.
  - **Startup:** the local pane starts at the drive list; the first site, if any, is selected.
  - **Explorer drops:** a drop onto the remote pane sets `status-left` to "Uploading arrives in the next build (I2): {n} items dropped on {site}." A drop onto the local pane sets "Drop files on the {site} pane to upload them."
- [ ] **Step 4: Run to see them pass**, `cargo test --workspace`.
- [ ] **Step 5: Commit**, `git commit -m "feat(app): session controller wiring sites, browsing and drops"`

---

### Task 16: I1 acceptance on the user's PC

**Files:**
- Create: `docs/acceptance/I1.md`

- [ ] **Step 1: Release build and size**, `cargo build --release -p httporter-app`; record the size of `target\release\httporter.exe`. Expected: ≤ 25 MB.
- [ ] **Step 2: Cold start and idle memory**, `tools\measure.ps1 -Exe target\release\httporter.exe -Label i1-idle -OutDir docs\acceptance`. Expected: window within 1 s; private memory ≤ 40 MB.
- [ ] **Step 3: User step: add the real site.** The user opens Add Site and enters zone `exampleshop`, region Singapore, and their password themselves (the agent never types or sees it), clicks Test Connection, then Add Site. Expected: "Connected over HTTPS · … · 282 items in /".
- [ ] **Step 4: Browse checks**
  - Remote root shows 282 items.
  - `wp-content/uploads/2026/10` shows 1,829 files and scrolls smoothly.
  - The local pane opens `D:\site-export`.
  - Dark mode follows Windows.
  - Wrong-password check on a duplicate site shows the exact message within 2 s.
  - Memory after browsing the 1,829-file folder ≤ 120 MB.
- [ ] **Step 5: Screenshot comparison** with `Main.dc.html` (light and dark); list visible differences and their reasons in `docs/acceptance/I1.md`.
- [ ] **Step 6: Commit**, `git commit -m "docs: I1 acceptance results"`
