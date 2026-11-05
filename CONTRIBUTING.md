# Contributing to HTTPorter

Thank you for helping. This page covers the setup, the checks every change passes, how
commits are written, and how to add a storage provider. [docs/architecture.md](docs/architecture.md)
explains how the code fits together; read it first if you are new here.

By taking part you agree to the [code of conduct](CODE_OF_CONDUCT.md). Security problems are
reported privately: see [SECURITY.md](SECURITY.md).

## Setup

- Windows 10 or 11, x64.
- [Rust](https://rustup.rs), stable, with the `x86_64-pc-windows-msvc` toolchain, and the
  `rustfmt` and `clippy` components (`rustup component add rustfmt clippy`).
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/), with
  the **Desktop development with C++** workload.
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny): `cargo install cargo-deny --locked`.
- Python 3 (standard library only) for the scripts in `tools/`.
- For release builds only: [Inno Setup](https://jrsoftware.org/isinfo.php) 6.7 or later.

```powershell
cargo build -p httporter-app      # debug build: target\debug\httporter.exe
cargo run -p httporter-app        # build and start it
```

Debug builds keep a console window for `println!` and panics; release builds have none.

## Checks

Run the local gate before every commit:

```powershell
powershell -NoProfile -File tools/check.ps1
```

It stops at the first failing step and exits with its code. The steps, which
`.github/workflows/ci.yml` runs too (keep the two in sync):

1. `cargo fmt --check`
2. `cargo clippy --locked --workspace --all-targets -- -D warnings`
3. `cargo test --locked --workspace`
4. `cargo deny --locked check --hide-inclusion-graph licenses bans`: every dependency's licence
   is on the list in [deny.toml](deny.toml) (GPL, AGPL and LGPL are not), and no wildcard
   versions. Duplicate versions are warnings.
5. `python -I tools/spdx_headers.py --check`: every `.rs` and `.slint` file under `crates/`
   and `tools/` starts with `// SPDX-License-Identifier: MIT OR Apache-2.0`. Run the script
   without `--check` to add the line to new files.
6. `python -I tools/third_party.py --check`: every crate in the exe has a licence text for
   `THIRD-PARTY-NOTICES.txt`. A crate that ships none needs an entry in the script's
   `FALLBACKS`, after you have read its upstream licence.

`Cargo.lock` is committed and every cargo command runs with `--locked`: a dependency changes
only in a commit that says so. Never run a plain `cargo update`.

### Tests

- Unit tests sit next to the code; integration tests in each crate's `tests/`.
- The transfer engine, the planner and the publish runner are tested against
  `httporter_core::MockProvider` (see below), with no network.
- The Bunny and S3 clients are tested against local HTTP servers: `wiremock`, and the small
  connection-counting server in `crates/httporter-bunny/tests/support/mod.rs`.
- The window is tested headless with Slint's testing backend (`i-slint-backend-testing`),
  finding elements by accessible role and label.
- `crates/*/tests/live.rs` talk to real services and are `#[ignore]`d. They read credentials
  from a git-ignored `.env.test` at the repository root, only for test zones and buckets whose
  names start with `httporter-test`, and refuse anything else. Run them with
  `cargo test --locked -p httporter-bunny --test live -- --ignored --nocapture` (or
  `-p httporter-s3`). Never point them at a production zone or bucket.

### Conventions the tests enforce

- **Words the user sees** live in a `copy.rs` file (`crates/httporter-app/src/copy.rs` and the
  ones in `manage/`, `publish/` and the other module folders). Text in `.slint` files goes
  through `@tr(...)`, so the app can be translated. `src/lint_tests.rs` fails on a string
  written anywhere else.
- **Secrets** (passwords, API keys, S3 secrets) live only in Windows Credential Manager. They
  never go into SQLite, settings, exports, logs (`httporter_core::redact`), `Debug` output,
  error text or the UI.
- **Size:** `httporter.exe` stays at or under 25 MB (`tools/package.ps1` refuses a bigger
  one). Say how much a new dependency adds; prefer the standard library and what is already
  linked.
- **No `panic = "abort"`** and no exe packers: the app relies on unwinding.

## Commits

- One logical change per commit, with the checks passing.
- Subject: [Conventional Commits](https://www.conventionalcommits.org) style,
  `type(scope): what changed`, in the imperative, under about 72 characters. Types in use:
  `feat`, `fix`, `perf`, `build`, `test`, `docs`, `refactor`, `chore`. Scopes are a crate or
  an area: `transfer`, `bunny`, `s3`, `publish`, `manage`, `settings`, `app`, `size`.
  For example: `fix(transfer): retry a delete whose request may have reached the server`.
- Body: why the change was needed and anything a reviewer can't see from the diff (a measured
  size or speed, a service's behaviour). Wrap at 72.

## Adding a storage provider

Everything above the provider (the panes, the transfer engine, the planner, publish) talks to
storage through one trait, `StorageProvider`, and decides what to do from the provider's
`Capabilities`. It never asks which service it is talking to. A new service (WebDAV, Azure
Blob Storage, Google Cloud Storage, ...) is a new crate that implements the trait, plus the
code that lets a user add such a site.

### 1. A crate

Create `crates/httporter-<service>` (the workspace picks up every folder in `crates/`), whose
`[package]` takes its edition, version, licence and `publish = false` from the workspace, as
the other crates' do. It depends on
`httporter-core` and takes shared dependencies from `[workspace.dependencies]`: `reqwest` with
`rustls` on `ring` (see how `httporter-bunny` sets up its client, and install ring's provider
before building one), `tokio`, `async-trait`, `bytes`, `futures-util`. `httporter-bunny` and
`httporter-s3` are the two worked examples.

### 2. The trait

`StorageProvider` is in `crates/httporter-core/src/provider.rs`. Read its doc comment: it
states the rules every provider follows. The methods:

| Method | Must implement | What it does |
|---|---|---|
| `capabilities()` | yes | The provider's `Capabilities` (step 3). |
| `endpoint()` | yes | A readable address for the pane header, e.g. `https://storage.example.com/zone`. |
| `list(dir)` | yes | The direct children of `dir`, as `Entry`s, plus `RequestInfo` (latency, HTTP version). |
| `upload(path, source, opts)` | yes | Store `source` (`UploadSource::Bytes` or `File { path, len }`) at `path`, overwriting. |
| `download(path)` | yes | The file's bytes as a `ByteStream`, chunk by chunk. |
| `delete_file(path)` | yes | Delete one file. |
| `delete_folder(dir)` | yes | Delete a folder and what is under it, or refuse a non-empty one without `recursive_delete`. |
| `create_folder(dir)` | yes | Make an empty folder; refuse if the service can't keep one. |
| `delete_batch(paths)` | no | Many deletes in fewer requests; default: one `delete_file` each. |
| `list_prefix(dir, prefix, limit)` | no | A cheap probe; default lists the folder. |
| `reachable()` | no | Is the service answering (used while offline); default lists the root. |
| `stat(path)` | no | `ObjectInfo` beyond a listing (created, content type, metadata); default finds it in the parent's listing. |
| `copy(from, to)` | no | A server-side copy; default `Permanent` (the engine then downloads and uploads again). |
| `presign(path, expires)` | no | A link that works without credentials; default `Permanent`. |
| `abort_multipart(path, upload_id)` | no | Give up an unfinished multipart upload; default does nothing. |

The rules that matter most:

- **One attempt per call.** Never retry inside the provider: the caller owns retries (the
  engine with its policy, a pane with a short one). Build the HTTP clients once, in the
  constructor, and reuse their connection pools; never a TLS handshake per file.
- **Errors** map to `ProviderError` (`crates/httporter-core/src/error.rs`), because the engine
  acts on the variant: `Auth { hint }` stops the batch and points at the wrong field;
  `RateLimited { retry_after }` (clamped to `MAX_RETRY_AFTER`) and `Server` / `Network` are
  retried with backoff; `NotFound` for a missing path; `Refused` for a write that collides
  with what is there; `ChecksumMismatch` when the service rejects the content's digest;
  `Permanent` for anything retrying will not fix. Error text never quotes a secret or a server
  answer that could hold one.
- **Uploads:** honour `UploadOptions`. When `sha256` or `md5` is set, have the service verify
  it, or reject the upload yourself. Call `opts.meter` for every piece of the body before
  sending it (progress and the bandwidth limit depend on it). A provider with multipart
  uploads records its progress through `opts.parts` (`PartJournal`), so an interrupted upload
  resumes; see `crates/httporter-s3/src/multipart.rs`.
- **Listings** fill `Entry::checksum` with the digest the service lists (`ChecksumKind::Sha256`,
  `Md5`, or `MultipartEtag` for one that is not a digest), so a compare can tell changed files
  from unchanged ones without downloading them.
- **Paths** are `RemotePath`s: `/`-separated, never `..`. Percent-encode each segment on the
  wire (`httporter_core::encode_segment`).

### 3. Capabilities

`Capabilities` (`crates/httporter-core/src/capabilities.rs`) tells the engine how to treat the
service. `Capabilities::default()` is the most cautious answer; set only what the service
really does:

- `checksum_in_listing`: the digest its listings carry, if any; the compare uses it.
- `server_side_copy`, `recursive_delete`, `batch_delete_max`: how moves and deletes are done.
- `empty_folders`: `Supported`, `Markers` (zero-byte `name/` objects), `Unsupported` or `Unknown`.
- `list_concurrency`, `delete_concurrency`, `transfer_concurrency_max`: the service's limits
  per site; the engine never goes above them, and halves its concurrency on 429 / 503.
- `presign`: whether `presign` works.
- `read_only`: the site's credentials can't write; every write action is disabled.
- `cut_uploads_delete_late`: an upload cut off part way can still delete its path later (Bunny
  does); the engine then checks such uploads again after a delay.

### 4. Tests

- **HTTP level:** test the client against a local server that plays the service, including its
  failures (401 / 403, 404, 429 with Retry-After, 5xx, dropped connections, a wrong checksum).
  `crates/httporter-bunny/tests/storage_ops.rs` and `crates/httporter-s3/tests/storage.rs` show
  the pattern with `wiremock`.
- **Live (optional):** an `#[ignore]`d `tests/live.rs` against a test account, reading
  `.env.test`, refusing any container whose name does not start with `httporter-test`.
- **Everything above the provider** is tested with `MockProvider`
  (`crates/httporter-core/src/mock.rs`), an in-memory provider that follows the trait's rules
  and its own `Capabilities`, with fault injection:

  ```rust
  use std::sync::Arc;
  use std::time::Duration;
  use httporter_core::{Capabilities, ChecksumKind, MockOp, MockProvider, ProviderError};

  let caps = Capabilities { checksum_in_listing: Some(ChecksumKind::Sha256), ..Capabilities::default() };
  let mock = Arc::new(MockProvider::new(caps).with_file("site/index.html", b"<h1>hi</h1>"));
  mock.fail_next(MockOp::Upload, ProviderError::Network("reset".into()), 2); // next 2 uploads fail
  mock.set_latency(MockOp::List, Duration::from_millis(50));                   // virtual time
  // ... run the engine or a planner against `mock`, then:
  assert_eq!(mock.calls(MockOp::Upload), 3);
  assert!(mock.max_in_flight(MockOp::Upload) <= 32);
  ```

  If the new service behaves in a way the engine must handle (as Bunny's late deletes do), add
  that behaviour to the mock behind a capability, and test the engine with it.

### 5. Wiring it into the app

- `httporter-core/src/site.rs`: a `ProviderKind` variant, and any connection details the site
  record needs (`SiteRecord`, `SiteExtras`); `sites_file.rs` exports them (never a secret).
- `httporter-platform/src/creds.rs`: the `SiteSecret` variant the site signs in with, and how
  it is stored as one Credential Manager value (`HTTPorter/site/<id>`).
- `httporter-app/src/session/mod.rs`: a `ProviderFactory` method and its `RealFactory` body
  (tests use the fakes in `session/fakes.rs`); `httporter-app/src/site_hub.rs`: the arm of
  `make_client` that builds the client from the site and its secret.
- The Add Site sheet: `ui/components/provider_grid.slint` and `add_site.slint`, with
  `src/add_site.rs` behind them, and every new word in `copy.rs`.
- A CDN in front of the service implements `CdnPurger` (`httporter-core/src/cdn.rs`), as
  `httporter-bunny`'s `BunnyCdn` does.
- New dependencies must pass `cargo deny`, and `python -I tools/third_party.py --check`.

## Releases

Maintainers: set `version` in the workspace `Cargo.toml`, add the version's section to
[CHANGELOG.md](CHANGELOG.md), commit, and push a tag `v<version>`.
`.github/workflows/release.yml` builds and packages it (`tools/package.ps1`) and puts the zip,
the setup and `SHA256SUMS.txt` on a draft GitHub release for review. Signing is described in
`tools/sign.ps1`.
