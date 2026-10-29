# HTTPorter, design

Date: 2026-10-29 (revision 2)
Status: draft for review
Owner: messeppe

Every decision is tagged with whose it is:

- **[user]** the user decided or asked for it.
- **[my call]** decided during design; not yet confirmed by the owner, who can overturn it.
- **[builder]** deliberately left to whoever builds it.
- **[open]** only the user can answer; collected in §17.

Revision 1 (a Bunny-only app on Go + Wails/WebView2) is superseded in full by this document.

---

## 1. What HTTPorter is

HTTPorter is a **free, native desktop file manager and uploader for cloud storage over HTTP APIs, "a universal HTTP uploader"**. **[user]** an FTP client made FTP easy; HTTPorter does the same for the storage services people actually use today, which speak HTTP APIs rather than FTP.

It runs as a normal program on the user's own computer: its own window, no server, nothing hosted, no web browser or WebView inside. **[user]** It ships as one main executable `httporter.exe` plus a tiny console companion `httporter.com` used only when HTTPorter is run from a terminal (§8.8.1). **[my call]** Its only network traffic is to the storage services the user connects it to.

It does two jobs:

1. **Publish**, mirror a local folder to a bucket/zone fast: upload only what changed, optionally remove what was deleted locally, then clear the CDN cache. The motivating case is a WordPress site exported to static files with Simply Static.
2. **Manage**, browse, upload, download, delete, rename/move, preview and edit remote files, two panes side by side. **[user]**

First release supports **Bunny Storage** and **S3-compatible storage** (AWS S3, Cloudflare R2, Backblaze B2, Wasabi, DigitalOcean Spaces, MinIO, Bunny's S3 endpoint). **[user]** WebDAV, Azure Blob Storage, Google Cloud Storage and Supabase Storage follow in later releases through the same adapter design. **[my call]** on that later order.

### 1.1 Why it exists

- The user uploaded a 13,793-file, 865 MB static export to Bunny Storage with an FTP client over FTP at **2 files/s** (~2 hours). FTP pays several round trips per file, and an FTP client runs 2 transfers at once by default (10 at most), each needing its own login.
- A Python script using Bunny's HTTP API with 32 parallel requests uploaded the same export in **~147 s (94 files/s)** with 0 failures.
- Research found no open-source GUI that uploads through Bunny's HTTP API. Existing options are command-line sync tools, Simply Static Pro's paid deploy, or S3 GUIs that only work with S3-enabled zones (which can't be enabled on an existing Bunny zone).
- The user wants their own program: beautiful (Apple/Google-quality UI/UX), following best practices, native and light on memory, and released as free software. **[user]**

### 1.2 What success looks like

- The user re-exports their site, opens HTTPorter, picks the export folder, clicks Publish, sees "34 changed · 2 new · 1 only on Bunny", confirms, and the live site shows the changes within a minute.
- A stranger downloads HTTPorter, connects their R2 bucket in under two minutes, drags a folder in, and never opens an FTP client or a provider dashboard for routine file work again.
- HTTPorter sits at a few tens of MB of memory, opens instantly, and looks like it belongs next to the best Apple apps.

---

## 2. Users and setting

- **Primary audience [user]:** anyone who stores files on Bunny or S3-compatible services and wants a desktop client: people deploying static sites, developers and small teams managing buckets, people moving files between their PC and object storage.
- **First user:** the owner of example-shop.com on Windows 11. Their facts are the reference workload:
  - Zone `exampleshop` (Bunny, Singapore), 13,793 files in 1,010 folders, 865 MB. 93% of files are under 1 MB (webp, jpg, png, js, css, svg, html); the largest is 6.4 MB.
  - `example-shop.com` and `www.example-shop.com` are served directly by Bunny Pull Zone 7654321 (`www` is a CNAME to `exampleshop.b-cdn.net`), with no other CDN in front, so purging through Bunny's API is enough.
  - Exports come from Simply Static as a ZIP the user unzips; each export lands in a newly named folder.
- **Platform:** Windows 10/11 x64 first. **[user]** Slint and Rust also support macOS and Linux; those builds are a later goal, not designed or tested in the first release. **[my call]**
- **Language:** English UI, built translation-ready (Slint's `@tr()` with gettext) so Indonesian and others can be added. **[my call]**

---

## 3. Goals, non-goals, anti-goals

### Goals

- G1. Publish a local folder to a remote folder, uploading only new or changed files, with an editable preview before anything happens.
- G2. an FTP client-level file management on every supported service: browse, upload, download, delete, new folder, rename/move, preview, open/edit with auto re-upload.
- G3. Fast: at least the Python script's 94 files/s on the reference workload at default settings; faster where the service's limits allow.
- G4. Safe: nothing is deleted remotely without the user seeing what and how many. Assume deletes are permanent.
- G5. Beautiful and pleasant: Apple-inspired Cupertino look, calm layout, purposeful motion, fully keyboard-operable. **[user]**
- G6. Native and light: no web engine, low memory (§4). **[user]**
- G7. Robust: survives crashes, closing, sleep and network loss mid-transfer, and resumes.
- G8. Universal by design: one provider-adapter interface; adding a service means writing an adapter, not changing the app. **[user]**
- G9. Releasable as free software: license, installer, signed binaries, update notifications, contributor-friendly code. **[user]**
- G10. Every feature in §8 is in scope, including the last milestone's (CLI, scheduled publish, watched folder, creating buckets/zones, zone settings). **[user]**

### Non-goals (first release)

- FTP/SFTP/FTPS (an FTP client already does them well).
- WebDAV, Azure, GCS, Supabase adapters (later releases).
- macOS and Linux builds.
- Multi-user features (shared accounts, roles, cross-user audit).
- Versioning or rollback of remote files; HTTPorter keeps no copies of what it overwrites.
- Telemetry or analytics of any kind.
- Rendering HTML pages, PDFs or video inside HTTPorter (no web engine; §8.5 opens them in the default app instead).

### Anti-goals (failures even if it "works")

- A "file exists" dialog per file, or an "apply to all" that doesn't stick.
- Any remote delete the user didn't explicitly see and confirm, with counts.
- The window freezing or stuttering with thousands of items.
- Secrets in plain text, in logs, or uploaded by accident (e.g. a `.env` in the publish folder).
- A dense, dated, toolbar-heavy look's.
- Modal dialogs that block the whole app while work runs.
- Showing buttons for operations a service can't do (e.g. a fast rename on Bunny) and then failing.

---

## 4. Success criteria (measurable, on the user's PC)

| # | Criterion | Target |
|---|---|---|
| S1 | Full upload of the reference export to an empty Bunny zone, default settings | ≥ 94 files/s; stretch ≥ 150 files/s |
| S2 | Re-publish of a fresh re-export with zero changes (all files re-hashed) | Scan + compare + "nothing to upload" in ≤ 20 s |
| S3 | Full remote listing of a zone the size of `exampleshop` | ≤ 10 s on Bunny (measured 5.6 s in Python) |
| S4 | Lists of 50,000 rows (browser, queue, publish preview, search) | Smooth scrolling; no frame over 50 ms |
| S5 | Kill the process mid-publish, restart | Offers to resume; finishes without re-uploading completed files |
| S6 | Cold start to usable window | ≤ 1 s |
| S7 | Memory, software renderer | Idle ≤ 40 MB private; ≤ 120 MB private during a 14k-file publish |
| S8 | Executable | `httporter.exe` ≤ 25 MB (spike: 13 MB); `httporter.com` console companion ≤ 1 MB |
| S9 | Wrong credentials | Clear message within 2 s; no retry storm |
| S10 | Same reference publish to an S3-compatible target (MinIO locally, R2 when available) | Correct content types, compare-by-checksum works, ≥ the Bunny throughput where the service allows |

---

## 5. Constraints

### 5.1 Platform and toolchain

- Windows 10/11 x64. The user's PC has Rust (stable, MSVC host; Rust 1.99 at the spike), Visual Studio Build Tools 2026 with the C++ workload, and Windows SDK 10.0.26100.
- No WebView, no browser engine, no Electron/Tauri/Wails. **[user]**

### 5.2 Bunny Storage HTTP API (verified in docs and against the live zone)

Host: the zone's `StorageHostname` from the account API, or `{region}.storage.bunnycdn.com`, except Frankfurt, which is plain `storage.bunnycdn.com`. Auth: `AccessKey: <storage zone password>`.

| Operation | Request | Notes |
|---|---|---|
| List folder | `GET /{zone}/{path}/` | JSON array; entries have `ObjectName`, `Path`, `Length`, `IsDirectory`, `LastChanged`, `DateCreated`, `Checksum` (uppercase SHA-256; `null` for folders), `ReplicatedZones`, `ContentType`, `Guid`. No pagination. |
| Download | `GET /{zone}/{path}/{file}` | Served `Content-Type` follows the extension (verified). |
| Upload | `PUT /{zone}/{path}/{file}` | Raw body. Optional `Checksum` (uppercase SHA-256) → server rejects with 400 on mismatch. Optional `Content-Type`. Parent folders are created automatically. 201 on success. |
| Delete file | `DELETE /{zone}/{path}/{file}` | Permanent. |
| Delete folder | `DELETE /{zone}/{path}/` | **Recursive**, permanent. |
| Delete root | `DELETE /{zone}/` | Blocked unless `allowRootDelete=true`. HTTPorter never sends it. |
| Rename / move / copy | none | Official workaround: upload under the new name, then delete the original. |
| Empty folder | not documented | See builder item B1. |

Limits: 250 concurrent connections per zone per storage server; **100 per client IP per server** (429 above). **5 concurrent listings, 30 concurrent deletes** (400 "Unable to process request" above). Paths up to 6,000 characters.

Account API (`https://api.bunny.net`, `AccessKey: <account API key>`):

| Purpose | Request | Notes |
|---|---|---|
| List storage zones | `GET /storagezone` | Includes `Password`, `ReadOnlyPassword`, `Region`, `ReplicationRegions`, `StorageHostname`, `PullZones[]`. `page=0` → plain array; `page>0` → `Items` + `HasMoreItems`. |
| Regions | `GET /storagezone/regions` | `Code`, `Endpoint`, `S3Endpoint`. |
| List pull zones | `GET /pullzone` | Storage origin = `OriginType 2` + `StorageZoneId`. `Hostnames[]`: `Value`, `IsSystemHostname`, `HasCertificate`, `ForceSSL`. |
| Purge pull zone | `POST /pullzone/{id}/purgeCache` | Optional `CacheTag`. |
| Purge URL/prefix | `POST /purge?url=…&async=…&exactPath=…` | `*` or trailing `/` = prefix purge. |
| Create zones | `POST /storagezone` (`Name`, `Region`), `POST /pullzone` (`Name`, `OriginType 2`, `StorageZoneId`) | |

Purge rate limits (per account, token bucket): exact URLs 120 burst, refill 5/s; prefix and whole-zone 20 burst, refill 0.5/s. **The account key has full account access**; Bunny has no scoped keys yet. Bunny's OpenAPI spec omits `Checksum`, `ContentType` and `ReplicatedZones` although live responses include them, code against observed responses.

Measured from the user's PC to Singapore: listing latency median 21 ms (p95 34 ms); a full listing of `exampleshop` (1,011 listings, 5 at a time) takes 5.6 s; each upload costs ~340 ms server-side, so throughput scales with concurrency, not bandwidth.

### 5.3 S3-compatible API

Auth: AWS Signature Version 4 with access key ID + secret (+ optional session token), a region (`auto` for R2), an endpoint URL, and path-style vs virtual-hosted addressing (MinIO and some others need path-style).

| Operation | Request | Notes |
|---|---|---|
| List | `ListObjectsV2` with `prefix` + `delimiter=/` | ≤ 1,000 keys per page (continuation token). Returns `Key`, `Size`, `LastModified`, `ETag`. "Folders" are common prefixes. |
| Upload | `PutObject`; multipart (`CreateMultipartUpload`/`UploadPart`/`CompleteMultipartUpload`) for large files | Multipart parts 5 MiB–5 GiB, ≤ 10,000 parts. **S3 does not infer `Content-Type`**: HTTPorter must send it or the object is served as `binary/octet-stream` and static sites break. |
| Download | `GetObject` (supports ranges) | |
| Delete | `DeleteObject`; `DeleteObjects` up to 1,000 keys per request | No recursive folder delete; list then batch-delete. |
| Copy | `CopyObject` (≤ 5 GB); `UploadPartCopy` for larger | Rename/move = server-side copy + delete (no download). |
| Metadata | `HeadObject` | Returns user metadata (`x-amz-meta-*`), which listing does not. |
| Buckets | `ListBuckets`, `CreateBucket` | Used for "pick a bucket" and creating buckets. |
| Presigned URLs | SigV4 query signing | Computed locally, no request. |

Checksums: for objects uploaded in a single `PutObject` without SSE-KMS/SSE-C, `ETag` is the hex MD5 of the content; multipart ETags are `md5(concat(part md5s))-N`. Newer additional checksums (`x-amz-checksum-*`: CRC32, CRC32C, SHA-1, SHA-256, CRC64NVME) are supported unevenly across S3-compatible services. HTTPorter's compare strategy is in §8.6.

Service presets (endpoint templates and quirks) **[my call]**: AWS S3 (`s3.{region}.amazonaws.com`), Cloudflare R2 (`{account}.r2.cloudflarestorage.com`, region `auto`), Backblaze B2 (`s3.{region}.backblazeb2.com`), Wasabi (`s3.{region}.wasabisys.com`), DigitalOcean Spaces (`{region}.digitaloceanspaces.com`), MinIO (custom URL, path-style), Bunny S3 (`{region}-s3.storage.bunnycdn.com`, S3-enabled zones only, public preview). **[builder]** verifies each preset's quirks during implementation.

### 5.4 Slint (UI toolkit), verified in the spike

- Slint 1.18.1, Rust API. Release exe of the spike: **13 MB**. With all 13,793 reference files in a table: **~15 MB private / ~32 MB working set** with the software renderer; ~45–50 MB private / ~100 MB working set with the GPU (femtovg/OpenGL) renderer. Folder scan 63–73 ms; table fill 8–11 ms.
- **Explorer file drops:** Slint's `DropArea` does not receive files dropped from other apps (Slint 1.18.1's winit backend never forwards winit's `HoveredFile`/`DroppedFile`). **Working workaround, confirmed by the user:** hook winit window events via `slint::winit_030::WinitWindowAccessor::on_winit_window_event` (feature `unstable-winit-030`), and get the drop position from Win32 `GetCursorPos` minus the window's inner position, divided by the scale factor, to know which pane received it. Keep it in one module so it can be replaced when Slint supports external drops.
- **Dragging out to Explorer** (download by dragging a remote file onto the desktop) is **not supported by Slint/winit** and needs Win32 OLE code (`DoDragDrop` with a data object). See builder item B4.
- Slint provides: Cupertino/Material/Fluent widget styles (compile-time choice), `StandardTableView`/`ListView` with virtualization, context menus, menu bars, system tray icons (1.17+), tooltips, multiple windows, translations (`@tr`), and accessibility through AccessKit (Windows UI Automation).
- **Licensing:** Slint is available under GPLv3, a royalty-free license (permits any app license for desktop apps, requires a "Made with Slint" attribution), or a commercial license. HTTPorter's license choice determines which applies (Q1).

### 5.5 Security

- Secrets (Bunny zone passwords, Bunny account keys, S3 secret keys, CDN API tokens) live only in the OS credential store (Windows Credential Manager on Windows), never in HTTPorter's database, settings, logs or crash reports.
- Destructive live tests never touch the user's production zone `exampleshop`. A dedicated test zone is approved. **[user]**

---

## 6. Decisions

| Decision | Whose | Rationale |
|---|---|---|
| Build our own app rather than adopting existing tools | [user] | No existing GUI fits. |
| Free software, universal HTTP uploader | [user] | |
| Native, low memory, no WebView | [user] | |
| Bunny + S3-compatible first; others later | [user] | |
| Publish source = a folder the user browses to | [user] | Remembered per profile; changeable any time. |
| Cupertino visual style | [user] | Chosen from the running spike. |
| Build in parallel tracks so results show fast | [user] | §15. |
| A dedicated test zone in the user's Bunny account may be created | [user] | §14. |
| Rust + Slint, software renderer by default | [my call] | Spike: 13 MB exe, ~15 MB private memory, Cupertino built in, Explorer drops solved. Rust's async I/O (tokio) handles hundreds of parallel transfers in little memory. GPU renderer selectable in Settings. |
| Name **HTTPorter** (HTTP + porter: carries your files over HTTP) | [user] | Chosen 2026-10-29 to emphasize the HTTP connection. Checked free the same day: no exact web results, 0 GitHub repositories, not on crates.io, npm or winget; httporter.com/.app/.dev/.org have no DNS records (possibly registrable). Not a legal trademark search. |
| Repo at `D:\Projects\httporter` | [my call] | Private until 1.0. |
| Local data in SQLite (`rusqlite`, bundled) under the per-user app-data folder | [my call] | Hash cache, journal and history need durable, queryable storage. |
| Credentials via the OS credential store (`keyring` crate or equivalent) | [my call] | Windows Credential Manager now; macOS Keychain/Secret Service later for free. |
| Update notifications only (no silent auto-update), opt-in check at startup | [my call] | Free software without telemetry; checks the public release feed. |
| Every number in §8 is a default | [my call] | Adjustable in Settings unless stated. |

---

## 7. Concepts

- **Provider**: a kind of storage service HTTPorter speaks to (Bunny Storage, S3-compatible). Implemented as an adapter (§10.2).
- **Site**: a saved connection, provider, endpoint/region, bucket or zone, credentials reference, optional CDN link and public base URL, optional default remote folder. Like an FTP client's Site Manager entry.
- **Account**: optional saved account-level credentials used to discover sites (Bunny account key → zones; S3 keys → buckets) and to manage zones/caches.
- **CDN link**: how a site's files are served publicly, used for "Copy public link" and purging: a Bunny Pull Zone (purge supported), or a plain public base URL (no purge in the first release).
- **Publish profile**: a local folder + site + remote folder + skip rules + publish options. A site can have several.
- **Publish**: scan both sides, compare, preview, then upload/delete/purge after confirmation.
- **Transfer**: one upload, download, delete, copy or purge, tracked in the queue and journal.
- **Skip rule**: a glob pattern for paths a publish never uploads or deletes.
- **Capabilities**: what a provider can do (§10.2); the UI shows only what the current site supports.

---

## 8. Features

### 8.1 Sites, accounts, credentials

1. **Add site:**
   - **Bunny via account key:** paste a Bunny account API key; HTTPorter lists every storage zone (name, region, linked pull zones and hostnames); the user ticks which to add. Zone passwords are fetched and stored in the credential store.
   - **Bunny manually:** zone name, region (picker), password; hostname derived (Frankfurt special case) or typed.
   - **S3-compatible:** pick a preset (§5.3) or "Custom", enter endpoint/region/account ID as the preset needs, access key ID and secret; HTTPorter lists buckets (`ListBuckets`) for the user to pick, or the user types a bucket name when listing buckets isn't permitted.
   - **Import:** from a `.env` like the user's existing script (`BUNNY_ZONE`, `BUNNY_KEY`, `BUNNY_REGION`, `BUNNY_LOCAL_DIR`, `BUNNY_PREFIX`) and from `~/.aws/credentials` profiles. **[my call]**
2. **Test connection** on save (one listing). Auth errors name the likely wrong field.
3. **Account key warning (Bunny):** the account key can do anything in the Bunny account. Offer "Use it once to fetch zone passwords, then forget it" (default) or "Keep it for cache purging and zone tools". **[my call]**
4. **Read-only sites:** Bunny `ReadOnlyPassword` or read-only S3 keys; every write action is disabled and the site shows "Read-only".
5. **Site list** in the sidebar with provider icon, region, linked CDN, and a health dot.
6. **Edit, duplicate, remove**; removing deletes the site's credential-store entries and profiles after confirmation, never remote files.
7. **CDN link:** Bunny pull zone (from the account API or typed), or a public base URL (e.g. `https://cdn.example.com/` or an R2 custom domain).
8. **Export/import sites** to a file without secrets (for moving to another PC); secrets are re-entered or re-fetched with the account key. **[my call]**

### 8.2 Browser

1. **Two panes:** local (left) and remote (right), each with breadcrumb path bar, back/forward/up, refresh, and a details list (name, size, modified, type; remote also shows checksum and, on Bunny, replication regions in the inspector).
2. **Single-pane mode** with an **inspector** panel (details + preview of the selection), Finder-style.
3. **Tabs**, restored on restart.
4. **Favorites (Places)** for local and remote folders in the sidebar.
5. **Sort** by any column; **filter-as-you-type** in the current folder.
6. **Recursive remote search** by name (text or glob), size range, date range, type; results stream in and are a normal selectable list (preview, download, delete, copy link, reveal).
7. **Difference highlighting** when both panes show a publish profile's folders (or "Compare" is on): New / Changed / Only remote / Same (§8.6 compare rules).
8. **Synchronized browsing** lock.
9. **Selection:** click, Ctrl/Shift, Ctrl+A, rubber band; summary in the status bar.
10. **Thumbnail grid** for image folders, generated lazily, cached on disk.
11. **Virtualized lists** for every large list (S4).

### 8.3 Transfers

1. **Upload:** drop from Explorer onto the remote pane or onto a specific remote folder row (drop position decides the target; §5.4), drag from the local pane, toolbar, context menu, paste. Folders recurse.
2. **Download:** drag from the remote pane to the local pane, toolbar, context menu, "Download to…". Dragging out to Explorer per B4.
3. **Integrity:** Bunny uploads send `Checksum` (SHA-256). S3 uploads send `Content-MD5` for single-part uploads (or an `x-amz-checksum-*` header where the preset supports it) and store `x-amz-meta-httporter-sha256`. Mismatch → retry once → fail with an explanation.
4. **Content-Type** from the file extension on every upload (S3 needs it; Bunny benefits). Optional per-profile `Cache-Control`.
5. **Large files on S3:** multipart above **64 MB** with **16 MB** parts, uploaded in parallel, resumable via the journal; abandoned multipart uploads aborted on cancel. **[my call]** on sizes.
6. **Concurrency**, per site, by request type:
   - Uploads + downloads: default **32**; max **96** on Bunny (under 100 per IP per server), **128** on S3. **[my call]**
   - Listings: Bunny **5** (hard cap); S3 **8**.
   - Deletes: Bunny default **16**, hard cap **30**; S3 uses `DeleteObjects` batches of up to 1,000.
   - Purges: token buckets matching Bunny's limits.
   - **Adaptive:** on 429/503 SlowDown, halve active concurrency and back off; add 1 slot per 20 consecutive successes up to the configured value.
7. **Retries:** network errors and 5xx retry with exponential backoff (1, 2, 4, 8, 10 s; 5 attempts) on a fresh connection. Auth failures stop the batch (S9).
8. **Bandwidth limit:** optional global cap (total, not per transfer).
9. **Queue drawer** (bottom, collapsible, never a separate window): aggregate header (done/total, files/s, MB/s, ETA, failed, pause/resume all, cancel all); tabs Active / Queued / Failed / Done; failure reason per file; Retry failed / Retry selected; reorder; pause/cancel items.
10. **Conflicts** with per-direction defaults: Ask once per batch (default), Replace, Skip, Replace if different (by checksum, else size), Keep both (`name (1).ext`). When asking, **one sheet per batch** lists every conflict with both sides' size, date and whether content differs, with an action per row and "apply to all".
11. **Journal and resume:** each transfer is recorded before it starts and marked done when the server confirms; after a crash or restart, "Resume 1,204 remaining transfers". Quitting during transfers asks: Pause and quit / Cancel and quit / Keep running.
12. **Offline and sleep:** queue pauses automatically and resumes when connectivity returns, with a banner.
13. **Taskbar progress** on HTTPorter's taskbar button; a Windows notification when a long batch or publish finishes while HTTPorter isn't focused.

### 8.4 Remote file operations

1. **Delete** files and folders. The confirmation sheet shows counts and total size from a recursive listing ("Delete 3 folders and 12 files, 1,284 files, 54 MB in total? This can't be undone."). Destructive button, never the default. The remote root can't be deleted from the UI.
2. **New folder:** S3, a zero-byte `name/` marker object (console convention). Bunny, per B1.
3. **Rename / move** (F2, drag within the remote pane, "Move to…"):
   - S3: server-side `CopyObject` (or `UploadPartCopy` > 5 GB) then delete; fast.
   - Bunny: stream download → upload at the new path with checksum, verify, then delete; before a folder rename a sheet states file count, size and an estimated time. Originals are deleted only after every copy succeeds; on failure nothing is deleted and the user sees which copies exist.
4. **Copy public link:** from the site's CDN link or public base URL (submenu when several hostnames), plus "Copy path". S3 also offers **"Copy presigned link…"** with an expiry (1 hour default). Multi-select copies one link per line.
5. **Purge from CDN** (Bunny pull zone with a kept account key): exact URLs for files, prefix for folders.
6. **Inspector / properties:** size, modified, created (where available), checksum/ETag, content type, metadata (S3 via `HeadObject` on demand), replication (Bunny), links.

### 8.5 Preview and edit (no web engine)

1. **Quick Look (Space):** in-app preview for images (png, jpg, jpeg, webp, gif, svg; avif if the image decoder supports it **[builder]**), and text/code (html, css, js, json, xml, txt, md, svg source, and other text by content sniffing) with syntax highlighting and line numbers. HTML is shown **as source**. PDF, audio, video and anything else show a large icon, details and **"Open in default app"**; HTML also offers **"Open in browser"**. Arrow keys move through the selection. Remote files download to a temp cache first; files over 50 MB ask before previewing. **[my call]**
2. **Open / Edit** (Enter or double-click): downloads to a temp folder, opens in the default app, watches the file; each save re-uploads it (with checksum) and shows a toast. Before re-uploading, HTTPorter compares the remote checksum/ETag with the one it downloaded; if the remote changed meanwhile: Overwrite / Save as copy / Discard my edit. An "Editing" list in the sidebar shows active edits with Stop; on quit HTTPorter waits for pending re-uploads after confirming.
3. **Open with…** and **Reveal in Explorer** for downloaded copies.

### 8.6 Compare and publish

1. **Publish profiles:** the local folder is chosen with the folder picker, remembered, and shown prominently with a "Change folder…" button on every publish. **[user]** Options:
   - "Remove remote files that aren't in the folder": **off by default** **[my call]**; the preview still lists them.
   - Purge after publish: Off / Changed files (default when purging is available) / Whole pull zone.
   - Delete guard thresholds; `Cache-Control` header (optional).
2. **Start** from the profile card, the command palette, or the CLI/scheduler (§8.8).
3. **Scan** (local and remote in parallel, with progress):
   - Local: walk, apply skip rules, hash files not in the hash cache (key: absolute path + size + modified time). Each file is read once to compute **SHA-256 and MD5 together**. A fresh export rewrites every file, so the cache usually misses; hashing 865 MB in parallel takes a few seconds.
   - Remote: full listing (5 at a time on Bunny, paged on S3), apply skip rules. Always fresh; no remote cache is trusted.
4. **Compare rules (per provider):**
   - Bunny: local SHA-256 vs listing `Checksum`.
   - S3: local MD5 vs listing `ETag` when the ETag is a plain MD5 (no `-N` suffix). For multipart ETags: HTTPorter recomputes the multipart ETag locally when it knows the part size (HTTPorter-uploaded objects record it in metadata); otherwise `HeadObject` for `x-amz-meta-httporter-sha256`; otherwise size + modified time, flagged "compared by size/date" in the preview.
5. **Diff:** New, Changed, Only remote (delete candidates), Same. Skip-rule matches are never uploaded or deleted on either side.
6. **Preview sheet** (nothing has happened yet): summary chips with counts and bytes; grouped, virtualized, filterable list with a checkbox per item and group; tree or flat view; "Only remote" marked "Will be deleted" or "Will be kept"; Publish (default) / Cancel; "Export plan as text".
7. **Delete guard:** above **100 files or 10% of remote files** (whichever is smaller), Publish becomes "Review deletions…" and requires typing the bucket/zone name. **[my call]**
8. **Run order:** uploads → deletes → purge, so the live site never references missing files mid-publish.
9. **Purge (Bunny pull zone):** "Changed files" purges exact URLs when ≤ **300** changed/deleted files (~36 s at Bunny's rate), otherwise one whole-pull-zone purge. **[my call]** Purge failures don't fail the publish; they show a warning with "Retry purge".
10. **History:** per publish, time, profile, counts (uploaded, deleted, skipped, failed), bytes, duration, purge result, full list of changed paths; searchable; failed publishes can be resumed or re-run. Last 200 per profile. **[my call]**
11. **Skip rules:** per profile, editable during a scan; glob on relative paths; presets. Defaults: `.env`, `.env.*`, `*.key`, `*.pem`, `.git/`, `.svn/`, `node_modules/`, `Thumbs.db`, `desktop.ini`, `.DS_Store`, `*.tmp`, `~$*`; optional "Source maps" (`*.map`). **[my call]**
12. **Secrets check:** non-skipped files that look like secrets (`.env*`, `*.pem`, `*.key`, `id_rsa*`, `*.p12`, `wp-config.php`) trigger a warning listing them before publishing.

### 8.7 Comfort

1. **Light and dark** following Windows, with override; Cupertino style throughout.
2. **Command palette** (Ctrl+K): every action, site, profile and favorite.
3. **Keyboard:** everything reachable. Core: Space preview, Enter open, Backspace up, F2 rename, Delete delete, Ctrl+Shift+N new folder, Ctrl+F search, F5/Ctrl+R refresh, Ctrl+T/W tabs, Ctrl+1/2 focus pane, Ctrl+Shift+C copy public link, Ctrl+P publish current profile, `?` shortcut sheet.
4. **Activity log** with filters and export; secrets redacted; rotating files.
5. **Toasts** for non-blocking feedback; **banners** for persistent states (offline, paused, read-only).
6. **Persisted UI state:** window, tabs, pane paths, column widths, sort, theme, panels.
7. **Undo** for local UI actions (closing a tab, removing a favorite); remote deletes say plainly they can't be undone.
8. **System tray icon** (optional) showing transfer progress when the window is closed during a long publish.

### 8.8 Last milestone (in scope, built last)

1. **CLI:** `httporter publish --profile "<name>" [--dry-run] [--yes] [--no-delete] [--purge changed|all|none]`, `httporter ls`, `httporter upload`, `httporter download`. Same profiles, credentials, journal and history. Exit codes: 0 ok, 1 transfer failures, 2 config error, 3 auth error, 4 delete guard tripped (needs `--yes`). **Binary layout [my call]:** `httporter.exe` is a Windows GUI-subsystem program (no console flash on normal launch); `httporter.com` is a tiny console-subsystem companion beside it. Typing `httporter …` in cmd/PowerShell resolves to `httporter.com` first (`.com` precedes `.exe` in `PATHEXT`), which runs the command with proper console output and exit codes, either by sharing the same core library or by starting `httporter.exe` and relaying its output **[builder]**. Task Scheduler entries call `httporter.com`. Flags and exit codes are a stable promise from the release they first ship in.
2. **Scheduled publish:** creates a Windows Task Scheduler task running the CLI; the profile card shows next run and last result.
3. **Watch folder:** optional auto-publish of uploads (and purges) when the local folder changes, debounced 10 s; deletes still need confirmation unless the profile explicitly allows unattended deletes.
4. **Create zones/buckets:** Bunny storage zone (+ optional linked pull zone) with a kept account key; S3 `CreateBucket`.
5. **Zone/bucket settings:** Bunny storage zone 404 handling and replication regions via the account API (**[builder]** confirms fields); S3 bucket CORS and static-website settings where the preset supports them **[builder]**.

---

## 9. UX and visual design

### 9.1 Principles

- **Calm by default:** content first, few chrome elements, generous spacing, clear hierarchy.
- **Never block:** long work lives in the queue drawer; sheets are for decisions only and attach to the window.
- **Say what will happen:** counts, sizes and time estimates before long or destructive actions.
- **Destructive actions are never the default button** and are worded specifically ("Delete 1,284 files").
- **Show only what's possible:** capabilities hide or explain unsupported actions per site.
- **Progressive disclosure:** concurrency, thresholds and renderer live in Settings › Advanced.
- **Motion with purpose:** 150–250 ms transitions for panels and sheets; honors Windows "reduce animations".
- **Accessible:** WCAG AA contrast, visible focus, full keyboard use, accessible names on icon buttons (AccessKit/UI Automation), follows Windows text scaling.

### 9.2 Look **[user]**: Cupertino

Slint's `cupertino` style is the base. HTTPorter adds:

- **Sidebar** (Sites, Profiles, Favorites, Editing) with subtle material-like background; **unified toolbar** with back/forward, path bar, search, Publish.
- **Inspector** on the right in single-pane mode; **Quick Look** as a floating panel.
- **Queue drawer** with an overall progress ring and compact rows.
- **Sheets** sliding from the toolbar for confirmations, conflicts and the publish preview.
- Segoe UI Variable text on Windows, SF-like proportions and spacing, hairline separators, one accent color, rounded corners matching Windows 11.
- An app icon of a stylized httporter entrance. **[builder]** with the user's review.

### 9.3 Patterns borrowed from research

- Transmit: Replace / Keep Both / Skip defaults per direction; progress inside the window; transfers start without pre-scanning sizes; simulate, then exclude items; edit skip rules mid-operation; drop onto a subfolder to put items inside it.
- WinSCP: overwrite prompt with both files' size and date; sync checklist with colored action icons.
- ForkLift: command palette; reorderable queue; saved syncs as favorites.
- Cyberduck: never delete unless asked; Copy URL submenu with several link types.
- rclone: a cap on how much a sync may delete.

---

## 10. Architecture (outline; details [builder])

### 10.1 Shape

```
+------------------------------ httporter.exe ------------------------------+
|  UI (Slint, Cupertino, software renderer)        main thread              |
|    Sidebar · Panes/Tabs · Queue drawer · Sheets · Quick Look · Settings   |
|    winit hook: Explorer drops + drop position                             |
|          ▲ view models / events (throttled ~10 Hz)                        |
|          │ commands                                                       |
|  App core (Rust, tokio worker threads)                                    |
|    sites & creds · localfs (walk, skip rules, SHA-256+MD5, hash cache)    |
|    planner (diff, guards) · transfer engine (queue, limiters, journal)    |
|    publish (scan → plan → run → purge → history) · preview/edit (watch)   |
|          │ Provider trait                         │ CDN trait            |
|    providers/bunny   providers/s3   (later: webdav, azure, gcs, supabase)  |
|    cdn/bunny-pullzone               (later: cloudflare, cloudfront)        |
|  Storage: SQLite (settings, sites, profiles, hash cache, journal, history)|
|  Secrets: OS credential store                                             |
+---------------------------------------------------------------------------+
```

- Slint's event loop owns the main thread; network and disk work runs on a tokio runtime; results return through channels and `slint::invoke_from_event_loop`. Progress reaches the UI at most ~10 times per second.
- One HTTP client (connection pool, keep-alive, rustls) per site and request type, sized to its limit; never a TLS handshake per file.
- Files stream from disk; hashes compute in the same read where possible; Bunny rename streams download → upload without touching disk.
- Long local paths use the `\\?\` prefix; remote paths are percent-encoded per segment.
- Suggested crates (all **[builder]**): `tokio`, `reqwest` (rustls), `object_store` or `aws-sdk-s3`/`rusty-s3` for S3, `sha2`, `md-5`, `rusqlite` (bundled), `keyring`, `notify` (file watching), `rfd` (native file/folder pickers), `mime_guess`, `globset`, `windows` (taskbar progress, OLE drag-out, notifications), `syntect` or similar for highlighting.

### 10.2 Provider adapter interface

Each provider implements one trait; the rest of the app never branches on provider type except through capabilities.

- **Operations:** list(prefix, recursive?) as a stream of entries; stat; upload(path, stream, size, content-type, checksums, metadata); download(path, range?) as a stream; delete(paths) (batched where supported); copy(src, dst) where supported; create_folder; presign(path, expiry) where supported; list_containers (zones/buckets) where supported.
- **Entry:** path, is_folder, size, modified, checksum (kind + value), content type (if known), provider extras (replication regions, metadata).
- **Capabilities:** checksum kind in listings (SHA-256 / MD5-ETag / none); server-side copy; recursive delete; batch delete size; empty folders (yes / marker objects / no); max single-request upload size and multipart; listing page size and concurrency cap; delete concurrency cap; presigned links; read-only mode.
- **Errors** mapped to a small shared set: Auth, NotFound, Conflict, RateLimited(retry-after), ChecksumMismatch, Transient, Permanent(message).
- A **mock provider** implementing the trait (in-memory, with fault injection) is part of the codebase, so the UI, transfer engine and planner can be built and tested without any network.

### 10.3 CDN interface

`purge_urls(urls)`, `purge_prefix(prefix)`, `purge_all()`, `public_urls(path) -> [url]`, plus rate-limit hints. First implementation: Bunny Pull Zone. Plain "public base URL" implements only `public_urls`.

---

## 11. Data and persistence

| What | Where |
|---|---|
| Secrets | OS credential store, target names `HTTPorter/site/<id>`, `HTTPorter/account/<id>`, `HTTPorter/cdn/<id>` |
| Sites, profiles, skip rules, settings, UI state | `%APPDATA%\HTTPorter\httporter.db` (SQLite) |
| Hash cache (absolute path, size, mtime → SHA-256, MD5) | same DB; pruned when files disappear |
| Transfer journal (incl. multipart upload IDs and completed parts) | same DB |
| Publish history | same DB; last 200 per profile |
| Thumbnails, preview/edit temp files | `%LOCALAPPDATA%\HTTPorter\cache`; items older than 7 days removed at start |
| Logs | `%APPDATA%\HTTPorter\logs`, rotating 10 × 5 MB, secrets redacted |

"Reset HTTPorter" in Settings removes all of it, including credential-store entries, after confirmation.

---

## 12. Errors and edge cases

| Case | Behavior |
|---|---|
| Auth failure (401/403, S3 `InvalidAccessKeyId`/`SignatureDoesNotMatch`) | Stop the batch; mark site unhealthy; name the likely wrong field (key, secret, region, endpoint, clock skew). |
| S3 `RequestTimeTooSkewed` | Tell the user their PC clock is off. |
| 429 / S3 `SlowDown` / 503 | Adaptive backoff (§8.3.6). |
| Bunny 400 over listing/delete limits | Shouldn't happen (limiters); retry with backoff. |
| Checksum mismatch | Re-hash local file, retry once, then fail with explanation. |
| 5xx / timeouts / resets | Retry policy; then Failed tab. |
| Local file changes during publish | Detected by size/mtime before sending; re-hash and send the new content; logged. |
| Local file locked/unreadable | That item fails with the OS reason; the rest continue. |
| Offline / sleep | Pause, banner, auto-resume. |
| Disk full on download | Pause queue; say which drive and how much space is needed. |
| Names with Unicode, spaces, `#`, `%`, `?`, `+` | Percent-encode per segment (S3 SigV4 canonical encoding rules); display decoded. |
| Names valid remotely but not on Windows (`:`, trailing dot, `CON`, case-only duplicates) | Offer a safe local name on download; warn about case-only duplicates. |
| Abandoned S3 multipart uploads | Aborted on cancel; on resume, continued from recorded parts. |
| App killed mid-publish | Journal resume (S5); the next scan simply sees partial results as changed/same. |
| Delete and upload under the same path at once | Queue orders operations per path; a delete waits for in-flight uploads under it. |

---

## 13. Security

- Secrets only in the OS credential store; masked in the UI with a per-click reveal.
- Bunny account key optional and forgotten after use by default.
- Default skip rules and the secrets check prevent publishing `.env`-style files.
- No telemetry. Network destinations are only the user's configured services, their CDN APIs, and (opt-in) the release feed for update checks.
- Release binaries are code-signed (B7) so Windows SmartScreen doesn't warn users.

---

## 14. Testing approach (details [builder])

- **Unit tests:** skip rules, planner/diff and compare rules per provider, delete guard, retry/backoff/adaptive concurrency, purge batching, path encoding (incl. SigV4 canonicalization), multipart ETag computation, journal recovery.
- **Mock providers:** in-process mock (§10.2) plus HTTP-level mocks: a Bunny mock server (list/upload/download/delete with the real JSON shape, `Checksum` verification, limits and fault injection, 429, 5xx, latency, dropped connections, >5 listings, >30 deletes) and **MinIO** running locally for S3 behavior.
- **Live tests:** a dedicated Bunny test zone (approved; created by the builder with credentials the user provides through a git-ignored local file, never pasted into chat), MinIO locally, and optionally a Cloudflare R2 bucket (Q3). Never write to `exampleshop` and **never purge production Pull Zone 7654321** from tests; read-only checks against them are allowed. Purge behavior (exact vs prefix, multiple hostnames, rate limits) is verified on a test pull zone linked to the test storage zone (Q5).
- **UI tests:** Slint's testing backend for view logic and keyboard navigation; manual checklist for drag-and-drop, sheets and accessibility (Windows Narrator).
- **Performance:** S1–S8 measured on the user's PC with release builds and reported.

---

## 15. Delivery: parallel tracks **[user]** (track split is **[my call]**)

The interfaces in §10.2–10.3 and the view-model contract between UI and core are fixed first (a short "contracts" step), then these tracks run in parallel, each independently testable:

| Track | Scope | Depends on |
|---|---|---|
| T1 Contracts & skeleton | Workspace layout, provider/CDN traits, mock provider, error types, view-model types, CI |, (first, short) |
| T2 Bunny provider + Bunny CDN | Storage + account API clients, limits, retries, purge, zone discovery | T1 |
| T3 S3 provider | SigV4 client or library, presets, multipart, compare data, presign | T1 |
| T4 Transfer engine | Queue, per-type limiters, adaptive concurrency, bandwidth cap, journal/resume, conflicts | T1 (uses mock provider) |
| T5 Publish engine | Local scan + hash cache, skip rules, compare rules, planner, guards, history | T1 (uses mock provider) |
| T6 UI shell | Cupertino app shell, sidebar, panes, tabs, queue drawer, sheets, Quick Look, settings, winit drop hook, driven by the mock provider | T1 |
| T7 Platform services | Credential store, SQLite store, Windows taskbar progress, notifications, tray, OLE drag-out (B4) | T1 |

Integration milestones, each a usable build on the user's PC:

1. **I1 "See it":** UI shell browsing a real Bunny zone (T2 + T6 + T7 basics). Earliest visible result.
2. **I2 "Move files":** uploads/downloads/deletes with queue and resume on Bunny and S3 (T3, T4).
3. **I3 "Publish":** full publish flow with preview, guard, purge, history (T5).
4. **I4 "Manage+":** rename/move, edit with auto re-upload, search, tabs, favorites, compare highlighting, sync browsing, thumbnails, presigned links, read-only sites.
5. **I5 "Polish & release":** command palette, full shortcuts, accessibility pass, performance pass against S1–S10, icon, installer, signing, update check, docs, license files. **The end of I5 is the public 1.0 release.** **[my call]**
6. **I6 "Later":** CLI, scheduled publish, watch folder, create zones/buckets, zone/bucket settings. Ships as 1.x updates and meets the same accessibility, performance and polish bar as I5. **[my call]**

Release scaffolding (LICENSE, license headers, allowed-dependency-license list, Slint license option) goes into T1 if the repository is public from the first commit (Q2), otherwise into I5.

---

## 16. Release as free software

- **License:** Q1. If HTTPorter is GPL-3.0-or-later, Slint is used under its GPLv3 option. If HTTPorter is MIT/Apache-2.0, Slint is used under its royalty-free license, which requires a "Made with Slint" attribution (About window and website).
- **Hosting and distribution:** public repository (Q2), GitHub Releases with a portable `.zip` and an installer (B5), later a winget manifest.
- **Signing:** B7.
- **Updates:** opt-in check of the release feed at startup; shows "HTTPorter 1.2 is available" with release notes and a download link; no silent install. **[my call]**
- **Contributor docs:** README, CONTRIBUTING (how to add a provider adapter), architecture overview, code of conduct.

---

## 17. Open items

### Answers received 2026-10-29 **[user]**

- **Q2:** the repository stays **private until the 1.0 release**. So Q1 (license) is decided at I5, and release scaffolding goes into I5.
- **Q4:** the name must **emphasize the HTTP connection**, the user chose **HTTPorter** (HTTP + porter, someone who carries your things). The earlier working name "Burrow" is retired everywhere, including file and folder names.
- **Q5:** yes, the builder may create a test pull zone and create/delete temporary `httporter-test-*` zones (rename the prefix with the product).
- **Q3:** yes, an S3-compatible account (R2) is available for live S3 testing.
- **Design rule:** the UI follows **Apple's Human Interface Guidelines** (macOS app conventions: sidebar, unified toolbar, sheets, alerts, inspector, Quick Look), adapted to Windows (Windows caption buttons, no Apple fonts or SF Symbols, which Apple licenses only for its platforms). Full visual mockups come first as the reference for the build.

### Questions only the user can answer [open]

- **Q1. License** (decided at I5, before the repository goes public): GPL-3.0-or-later (strong copyleft uses Slint's GPLv3 option) or a permissive license such as MIT/Apache-2.0 (uses Slint's royalty-free license with a "Made with Slint" attribution)? Until then the private repository carries no license and avoids dependencies whose licenses would rule out either choice.

Nothing else blocks planning: Q2–Q5 are answered above.

### Visual reference

The approved-for-review mockups live on the design canvas "HTTPorter, UI mockups": main window light and dark, single pane with inspector, transfers, the batch conflict sheet, the delete alert, publish preview, publish history, Quick Look, command palette, welcome, Add Site and Settings › Transfers. Builders match these screens; differences need a reason (e.g. a Slint limitation) noted in the plan.

### Left to the builder [builder]

- B1. How Bunny handles empty folders (try `PUT /{zone}/{path}/` with an empty body on the test zone); if unsupported, "New folder" on Bunny creates a virtual folder in the UI that becomes real with the first file, explained in the UI.
- B2. Whether Bunny paths are case-sensitive (test zone).
- B3. Library choices (§10.1), syntax highlighter, image decoders (WebP/AVIF), virtual list strategy.
- B4. Dragging remote files out to Explorer: implement with Win32 OLE `DoDragDrop` (download to temp, then offer `CF_HDROP`; or virtual files via `CFSTR_FILEDESCRIPTOR`/`CFSTR_FILECONTENTS`). If not feasible in I4, ship "Download to…" and in-app pane drags, and say so in the UI.
- B5. Installer technology (MSI vs NSIS vs MSIX) and portable build layout.
- B6. Exact account-API fields for zone settings and preset-specific S3 bucket settings.
- B7. Code-signing route for a free project (e.g. SignPath Foundation's free OSS signing, or Azure Trusted Signing).
- B8. Each S3 preset's quirks (path-style, checksum headers supported, `DeleteObjects` support, listing behavior).

---

## Appendix A. Sources

- Bunny Storage HTTP API: https://bunny.net/docs/storage/http.md
- Bunny limits: https://bunny.net/docs/storage/limits.md
- Bunny upload with checksum: https://bunny.net/docs/api-reference/storage/manage-files/upload-file
- Bunny S3 compatibility: https://bunny.net/docs/storage/s3.md
- Bunny storage zones / pull zones / purge: https://bunny.net/docs/api-reference/core/storage-zone/list-storage-zones.md, https://bunny.net/docs/api-reference/core/pull-zone/list-pull-zones.md, https://bunny.net/docs/api-reference/core/pull-zone/purge-cache.md, https://bunny.net/docs/api-reference/core/purge/purge-url.md, https://bunny.net/docs/cdn/purge-cache.md
- Bunny API keys: https://bunny.net/docs/account/api-keys.md
- AWS S3 API: https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html, https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObjects.html, https://docs.aws.amazon.com/AmazonS3/latest/API/API_CopyObject.html, https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity.html, https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html
- Slint: https://slint.dev/blog/slint-1.17-released, https://docs.slint.dev/latest/docs/slint/guide/development/drag-and-drop/, https://docs.slint.dev/latest/docs/slint/reference/std-widgets/style/, https://docs.rs/slint/latest/slint/struct.DataTransfer.html, https://github.com/slint-ui/slint/blob/master/LICENSE.md
- Comparable apps: https://help.panic.com/transmit/transmit5/features/, https://winscp.net/eng/docs/ui_synchronize_checklist, https://winscp.net/eng/docs/ui_overwrite, https://docs.cyberduck.io/cyberduck/sync, https://binarynights.com/, https://rclone.org/docs/
- Memory comparisons: https://dev.to/trystan_sarrade/how-i-took-my-rust-gui-from-135-mb-to-30-mb-by-ditching-the-gpu-2i7d, https://www.maibornwolff.de/en/know-how/qt-vs-flutter/

## Appendix B. Measurements (2026-10-28/29, user's PC)

| Measurement | Result |
|---|---|
| an FTP client FTP upload to Bunny | ~2 files/s |
| Python HTTP script, 32 parallel | 13,793 files / 865 MB in ~147 s = 94 files/s, 0 failures |
| Python script vs local mock server | ~1,300–1,400 files/s (client not the bottleneck) |
| Bunny full listing, 5 parallel | 1,011 listings in 5.6 s; median 21 ms, p95 34 ms |
| Bunny served content types | `.html` text/html, `.xml` text/xml, `.png` image/png |
| Slint spike, software renderer, 13,793-row table | ~15 MB private / ~32 MB working set; exe 13 MB |
| Slint spike, GPU renderer | ~45–50 MB private / ~100 MB working set |
| Slint spike, scan + table fill | 63–73 ms + 8–11 ms |
| Explorer drop via winit hook | Works (user-confirmed), with drop position |
