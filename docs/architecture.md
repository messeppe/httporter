# HTTPorter architecture

How the code is laid out and how it runs, as built. The design and its reasons are in
`docs/superpowers/specs/2026-10-29-httporter-design.md` (§10 architecture, §11 data); this page
describes the code that exists. Module doc comments (`//!`) go into more detail, file by file.

## Shape

```
+------------------------------- httporter.exe -------------------------------+
|  UI: Slint (Cupertino style, software or GPU renderer, winit) main thread    |
|    window, sidebar, panes, tabs, transfer drawer, sheets, Quick Look,        |
|    Settings; the Explorer drop hook                                          |
|        ^ views, at most ~10 a second              | callbacks                |
|        | upgrade_in_event_loop                    v                          |
|  httporter-app: binding, session, transfers, manage, publish  (tokio tasks)  |
|        |                                                                     |
|  httporter-transfer: engine (lanes, limits, journal), planner, compare,      |
|                      publish runner, moves, search                           |
|        | StorageProvider trait                    | CdnPurger trait          |
|  httporter-bunny          httporter-s3            httporter-bunny (BunnyCdn) |
|  httporter-platform: SQLite (sites, settings, profiles, history, hash cache, |
|                      journal), Credential Manager, logs, cache, Windows APIs |
|  httporter-core: the types and traits all of the above share                 |
+-----------------------------------------------------------------------------+
```

## Crates

All in `crates/`, one workspace (`Cargo.toml` at the root), edition 2024.

| Crate | What it holds | Depends on |
|---|---|---|
| `httporter-core` | The shared vocabulary, no I/O of its own: `StorageProvider`, `Capabilities`, `Entry` / `Checksum`, `ProviderError`, `RemotePath` (with per-segment percent-encoding), the `Journal` trait and an in-memory one, `RetryPolicy` / `with_retry`, skip rules and the secrets check, site and profile records, the sites export file, settings, UI state, activity events and `redact`, the CDN types (`CdnPurger`, `purge_urls`), line diffs, and `MockProvider`. | -- |
| `httporter-bunny` | Bunny Storage (`BunnyStorage`: list, upload with SHA-256, download, delete, stat), the Bunny account API (`BunnyAccount`: storage zones, their passwords and pull zones), and pull-zone purging (`BunnyCdn`, under Bunny's rate limits). Regions and storage hosts. | core |
| `httporter-s3` | S3-compatible storage (`S3Storage`): AWS Signature V4 signing (pure, no clock or network), service presets and endpoint addressing (path-style or virtual-hosted), multipart uploads that resume, `CopyObject`, `HeadObject`, presigned links, bucket listing. Service quirks: `docs/notes/s3-presets.md`. | core |
| `httporter-transfer` | The transfer engine (`Engine`), the planner (`plan_upload`, `plan_download`, `plan_delete`, conflicts), the folder compare (`compare_trees`, with a hash cache), the publish runner (`run_publish`), moves and renames, recursive search, the bandwidth token bucket and the adaptive concurrency limiter. | core |
| `httporter-platform` | What touches the PC: the SQLite `Store` and `SqliteJournal`, the credential store (`KeyringStore` on Windows Credential Manager, `MemoryStore` for tests), the activity log files, the file cache, and `win/` (taskbar progress, toasts, file pickers, clipboard, shell open, folder watching). | core |
| `httporter-app` | The program: `ui/` (`.slint` files, icons, the Inter font), compiled by `build.rs`; `main.rs` (start-up and shut-down); `binding.rs` (window ↔ session); `session/` (sites, panes, profiles, settings); `site_hub.rs` (one storage client per site); `transfers.rs` (engine ↔ window); `manage/` (inspector, Quick Look, edit, favorites, palette, search, tabs, moves, activity, Settings tabs); `publish/` (compare, Publish sheet, profiles, purge); `drop_hook.rs` (files from Explorer); `single_instance.rs`. User-facing words live in the `copy.rs` files. | all of the above |

Only `httporter-app` and `httporter-platform` know they run on Windows. The provider crates
and the engine see only core's traits, so they are tested without a window, a network or a
credential store.

## Providers and capabilities

`StorageProvider` (`httporter-core/src/provider.rs`) is the one interface to storage. Above it,
nothing asks which service a site is; it reads the site's `Capabilities` instead: the digest
its listings carry, server-side copy, recursive and batched deletes, empty folders, its
concurrency limits, presigned links, read-only, and whether cut-off uploads can delete late.

A provider makes **one attempt per call**. Whoever calls it owns the retries and picks the
policy: a pane retries briefly, so a dead connection shows within a minute; the engine retries
transfers with exponential backoff. Failures come back as `ProviderError`, a small set the
engine acts on (stop the batch on `Auth`, back off on `RateLimited`, retry `Server` /
`Network`, give up on `Permanent`).

Each site has one client, built the first time it is needed and shared by the panes and the
engine (`SiteHub`). A client keeps two HTTP connection pools: short requests (listings,
deletes) with a total timeout, and transfers with none, abandoned only when they stop making
progress. TLS is rustls on ring.

`MockProvider` implements the trait in memory, follows its rules and its `Capabilities`, and
injects failures and latency. The engine, planner, compare and publish tests run on it.
Adding a provider: [CONTRIBUTING.md](../CONTRIBUTING.md#adding-a-storage-provider).

## Threads

- **Main thread:** Slint's event loop (winit backend; the software renderer, or the GPU one
  (femtovg/OpenGL) when chosen in Settings › Advanced, picked in `renderer.rs` unless
  `SLINT_BACKEND` says otherwise). It runs the callbacks and applies views, and does no
  network or disk work.
- **tokio runtime** (multi-threaded, built in `main.rs`): listings, connection tests, planning,
  compares and every transfer run as tasks on it. Tasks are spawned boxed (`rt.rs`), so the exe
  carries one copy of tokio's task code per result type (exe size).
- **tokio's blocking pool:** whatever waits on Windows: the credential store, SQLite, building
  an HTTP client, reading local files to hash them (8 at a time).
- **Small named threads:** `prune-cache` (cache items untouched for a week), `prune-hashes`
  (digests unused for 90 days), `save-ui-state`, and `dir-watcher` (files open for editing,
re-uploaded on save).

Results reach the window through `slint::Weak::upgrade_in_event_loop`. The session keeps its
parts (site database, site list, each pane) behind separate locks, never held across an
`.await`, so a listing that hangs holds up nothing else. Each pane's view carries a version and
the window applies only a newer one, so answers that arrive late or out of order are dropped.
The engine's changes reach the window at most ten times a second.

Only one HTTPorter runs per Windows session: the first holds the mutex
`Local\HTTPorter.Running`, and a second one brings the first one's window forward and exits.
The installer uses the same mutex to ask for HTTPorter to be closed first.

## The transfer engine

`httporter-transfer/src/engine.rs` and `lane.rs`:

- **One lane per site**: its queue, the work in flight and its limits (the provider's
  concurrency caps; listings, transfers and deletes counted separately).
- **Adaptive concurrency:** on 429 / 503 / SlowDown a lane halves what it runs at once and
  backs off, then adds a slot per run of successes, up to the configured number.
- **Bandwidth:** one token bucket shared by every transfer, fed through the upload meter.
- **Offline:** after repeated network errors a lane stops starting work and probes the site
  with `reachable()`; the first answer resumes it. A full disk pauses every lane.
- **Journal:** every job is written down before it starts and marked when it ends, by a single
  writer task, so the journal is always in order. After a crash or restart the unfinished jobs
  are offered again; an S3 multipart upload resumes from its recorded parts.
- **Batches:** the window submits work in batches (an upload of a selection, a publish's
  uploads); a batch reports its outcome when its last job ends.

A publish (`compare.rs`, `publish.rs`) walks the local folder and the site folder in parallel,
applies the skip rules on both sides, hashes local files only where the site lists a digest to
compare with (SHA-256 and MD5 from one read; a persistent hash cache keyed by path, size and
modified time), plans New / Changed / Only remote / Same, and runs uploads, then deletes, then
the CDN purge, so the live site never links to a missing file.

## Data

| What | Where |
|---|---|
| Sites, provider accounts, publish profiles and their history, CDN links, settings, UI state, favorites, the hash cache | `%APPDATA%\HTTPorter\httporter.db` (SQLite, `Store`) |
| The transfer journal, with multipart upload ids and finished parts | the same file, `journal` table (`SqliteJournal`, its own connection) |
| Activity log | `%APPDATA%\HTTPorter\logs\activity.log`, rotated over 10 files of 5 MB |
| Previews, thumbnails, files open for editing, move staging | `%LOCALAPPDATA%\HTTPorter\cache\{preview,thumbs,edit,move}`, items older than 7 days removed at start |
| Secrets | Windows Credential Manager (below) |

The database runs in WAL mode with a 5 s busy timeout, so the store and the journal each keep
a connection to it and the engine's many small writes never wait on the window's reads. The
schema is versioned (`PRAGMA user_version`) and migrated forward in `store.rs`. If the file
cannot be opened, the app runs on an in-memory database and the banner says so.

Settings, each site's extras and the window's UI state are stored as JSON in the database.
Exported sites (Settings › Sites) are a separate JSON file that never holds a secret.

## Secrets

- Passwords, account API keys and S3 secret keys live only in **Windows Credential Manager**,
  through the `keyring` crate (`httporter-platform/src/creds.rs`). Targets:
  `HTTPorter/site/<site id>` and `HTTPorter/account/<account id>`.
- A site's secret is read only when its client is first built, and goes straight into the
  client's configuration; it is never kept in the session, written to SQLite or a settings
  export, logged, put in a view, or shown in an error. `CredError` deliberately drops
  `keyring`'s payloads, which can hold the secret.
- The activity log passes every line through `redact`. `Debug` on configurations and clients
  prints no secret.
- The Bunny account key can do anything in the account, so by default it is used once to fetch
  zone passwords and then forgotten; keeping it (for CDN purges) is opt-in.
- HTTPorter has no telemetry. It connects only to the storage services and CDN APIs of the
  sites the user adds, and, when the user turns the update check on, to the release feed.

## Building and releasing

- `cargo build --release -p httporter-app` builds `httporter.exe`. The release profile is
  built for size (`opt-level = "s"`) except the busy code, which gets `opt-level = 3`
  (hashing, TLS crypto, the renderers); fat LTO, one codegen unit, stripped. There is no exe-size
  budget: no feature or behaviour is dropped for size.
  Never `panic = "abort"`: the app relies on unwinding.
- `tools/check.ps1` is the local gate and `.github/workflows/ci.yml` its CI twin: format,
  clippy, tests, `cargo deny` (licences and bans, `deny.toml`), SPDX headers
  (`tools/spdx_headers.py`) and the third-party notices check (`tools/third_party.py`).
- `tools/package.ps1` builds, stages, signs (`tools/sign.ps1`, a no-op without a certificate),
  zips, runs Inno Setup (`installer/httporter.iss`) and writes `SHA256SUMS.txt` in `dist\`.
  `.github/workflows/release.yml` runs it for a `v*` tag and drafts a GitHub release.
