# Mockup feature checklist

**Rule (user, 2026-11-02):** the 13 mockup screens in `docs/design/mockups/` are the required standard. Every feature on them must exist in the shipped app. Each milestone plan takes its tasks from this list and ticks items off when they are reviewed and merged. A visible control that isn't wired yet must be disabled with a "later version" note, never silently inert.

Status: ✅ done (reviewed) · 🔧 in progress · ⬜ planned. Milestone = where it is built.

## Main window: two panes (`Main.dc.html`, `MainDark.dc.html`)

| Feature | Milestone | Status |
|---|---|---|
| Native window, Windows caption buttons, Inter font, Cupertino style | I1 | ✅ |
| Toolbar row is the title bar (no separate Windows title bar), caption buttons at its right end, drag / snap / Snap Layouts | I5 | ⬜ |
| App icon (blue rounded square, white tray-and-arrow mark) in the exe, taskbar and window | I5 | ⬜ |
| Light and dark theme following Windows | I1 | ✅ |
| Toolbar: sidebar toggle | I1 | ✅ |
| Toolbar: Back / Forward (browsing history, Alt+←/→) | I1 | ✅ |
| Refresh (F5 / Ctrl+R) on each pane | I1 | 🔧 (final fix wave) |
| Toolbar: site title + subtitle (provider · zone) | I1 | ✅ |
| Toolbar: green "HTTPS · {ms} ms" connection pill | I1 | ✅ |
| Toolbar: view toggle: two panes / single pane + inspector | I4 | ⬜ |
| Toolbar: Compare toggle | I3 | ⬜ |
| Toolbar: Upload, Download, New folder, Delete buttons | I2 | ✅ |
| Toolbar: Publish button | I3 | ⬜ (disabled now) |
| Toolbar: search field "Search {site}" (recursive remote search) | I4 | ⬜ |
| Sidebar: Sites with provider tile, subtitle, health dot | I1 (Bunny) / I2 (S3 sites) | ✅ / ✅ |
| Sidebar: Publish profiles with "last published" time | I3 | ⬜ |
| Sidebar: Favorites (local and remote folders) | I4 | ⬜ |
| Sidebar: Editing (files open for edit, auto re-upload) | I4 | ⬜ |
| Sidebar: Add Site button | I1 | ✅ |
| Sidebar: Settings button → Settings window | I2 | ✅ |
| Pane header: icon, title, clickable breadcrumbs | I1 | ✅ |
| Pane header: storage endpoint chip (https://…) | I1 | ✅ |
| Pane header: "Comparing" chip | I3 | ⬜ |
| Columns Name / Size / Modified with sorting | I1 | ✅ |
| Status column with Changed / New / Only on Bunny / Same chips | I3 | ⬜ |
| Folders first, disclosure chevrons, kind icons, selection highlight | I1 | ✅ |
| Keyboard browsing (arrows, Enter, Backspace, Tab, focus ring) | I1 | ✅ |
| Drag files from Explorer onto the cloud pane (hover ring + "Release to upload") | I1 (hook) / I2 (upload) | ✅ / ✅ |
| Drag between panes (upload / download) | I2 | ✅ |
| Transfer drawer, collapsed: "No transfers running · Last publish …" | I2 | ✅ |
| Status bar: selection summary ("index.html selected · 383.4 KB") | I2 | ✅ |
| Status bar: compare summary chips (34 changed · 2 new · 1 only on Bunny) | I3 | ⬜ |
| Status bar: "HTTP/2 · host · N connections ready" | I1 | ✅ (connections count I2) |

## Single pane + inspector (`Inspector.dc.html`)

| Feature | Milestone | Status |
|---|---|---|
| Single-pane layout with inspector panel | I4 | ⬜ |
| List / thumbnail grid toggle | I4 | ⬜ |
| Inspector preview (image) | I4 | ⬜ |
| Inspector: name, kind, dimensions, size | I4 | ⬜ |
| Inspector actions: Quick Look, Download, Delete | I4 | ⬜ |
| Inspector metadata: modified, created, content type, replicated regions, SHA-256, "matches local" | I4 | ⬜ |
| Links: public CDN URL with copy, storage URL with copy | I4 | ⬜ |
| Purge from CDN | I3 (purge) / I4 (button) | ⬜ |
| Refresh button | I4 | ⬜ |

## Moving files

| Feature | Screen | Milestone | Status |
|---|---|---|---|
| Transfer drawer expanded: progress ring, "Uploading to {site}", file and MB counts, files/s, MB/s, time left | `Transfers.dc.html` | I2 | ✅ |
| Tabs Active / Queued / Failed / Done with counts | `Transfers.dc.html` | I2 | ✅ |
| Pause / Cancel all; per-row progress bar and cancel | `Transfers.dc.html` | I2 | ✅ |
| Batch "file exists" sheet: default action (Replace if different / Replace all / Skip all / Keep both), per-row action, "Remember for uploads to this site", Cancel Upload / Continue | `Conflicts.dc.html` | I2 | ✅ |
| Delete alert: counts and size, "can't be undone", "Also purge these URLs from the CDN", Delete N Files / Cancel (default) | `DeleteAlert.dc.html` | I2 (alert) / I3 (purge option) | ✅ / ⬜ |

## Publish (`PublishPreview.dc.html`, `History.dc.html`)

| Feature | Milestone | Status |
|---|---|---|
| Publish sheet: folder row with Change Folder…, HTTPS destination row | I3 | ⬜ |
| Change summary chips and bytes to upload | I3 | ⬜ |
| Plan list with checkboxes (changed / new / will be kept / will be deleted) | I3 | ⬜ |
| "Remove files from Bunny that aren't in the folder" option | I3 | ⬜ |
| Purge CDN: Off / Changed files / Whole zone | I3 | ⬜ |
| Export Plan…, Cancel, Publish N Files | I3 | ⬜ |
| Profile page: folder → destination, chips (skip rules, remove extras, purge, delete guard), Change Folder…, Edit Profile, Publish | I3 | ⬜ |
| Stat cards: last publish, on Bunny now, schedule | I3 (schedule card links to I6) | ⬜ |
| History table: when, result, uploaded, deleted, size, time, CDN purge | I3 | ⬜ |
| Failed-publish detail with "Retry N Failed" and "Show All Files" | I3 | ⬜ |
| Search history | I3 | ⬜ |
| Publish on a schedule… | I6 | ⬜ |

## Preview and speed

| Feature | Screen | Milestone | Status |
|---|---|---|---|
| Quick Look (Space): code with line numbers and syntax colours | `QuickLook.dc.html` | I4 | ⬜ |
| Quick Look: Source / Changes vs Bunny (diff) | `QuickLook.dc.html` | I4 | ⬜ |
| Quick Look: Open in Browser, Open With…, close, ← → next file | `QuickLook.dc.html` | I4 | ⬜ |
| Command palette (Ctrl+K): actions, sites, folders, shortcuts | `CommandPalette.dc.html` | I4 | ⬜ |

## Connect and set up

| Feature | Screen | Milestone | Status |
|---|---|---|---|
| Welcome (first run): icon, wordmark, three feature rows, Add Your First Site | `Welcome.dc.html` | I2 | ✅ |
| Welcome: Import from .env or AWS profile… | `Welcome.dc.html` | I2 | ✅ |
| Add Site: Bunny with storage zone password, endpoint preview, Test Connection | `AddSite.dc.html` | I1 | ✅ |
| Add Site: Bunny account key sign-in (pick zones) | `AddSite.dc.html` | I2 | ✅ |
| Add Site: S3 providers (Amazon S3, R2, B2, Wasabi, DigitalOcean Spaces, MinIO, Other S3) | `AddSite.dc.html` | I2 | ✅ |
| Settings window with tabs General / Sites / Transfers / Security / Publish / Advanced | `Settings.dc.html` | I2 (window + Transfers) / I4–I5 (other tabs) | ✅ / ⬜ |
| Settings › Transfers: simultaneous transfers, slow down on 429/503, retries, bandwidth limit | `Settings.dc.html` | I2 | ✅ |
| Settings › Transfers: HTTP protocol, keep connections open, timeout, proxy | `Settings.dc.html` | I2 | ✅ |
| Settings › Transfers: verify checksums, "file exists" defaults, Restore Defaults | `Settings.dc.html` | I2 | ✅ |

## Spec features without a mockup screen

The design document (§8) also requires these; they have no dedicated mockup but are equally mandatory.

| Feature | Spec | Milestone | Status |
|---|---|---|---|
| Test connection on save (refuse to save a site that fails) | §8.1.2 | I1 | 🔧 (final fix wave) |
| Edit, duplicate and remove sites (remove deletes its Credential Manager entry) | §8.1.6 | I2 | ✅ |
| Read-only sites (read-only password / keys disable writes) | §8.1.4 | I4 | ⬜ |
| Export / import sites without secrets | §8.1.8 | I4 | ⬜ |
| Multi-select (Ctrl/Shift, Ctrl+A, rubber band) and selection summary | §8.2.9 | I2 | ✅ |
| Drop onto a specific remote folder row | §8.3.1 | I2 | ✅ |
| Transfer journal and resume after crash or restart | §8.3.11 | I2 | ✅ |
| Pause on offline / sleep, resume automatically | §8.3.12 | I2 | ✅ |
| Adaptive concurrency on 429/503, bandwidth cap | §8.3.6, §8.3.8 | I2 | ✅ |
| Taskbar progress and Windows notification when a long batch finishes | §8.3.13 | I2 | ✅ |
| Rename / move (Bunny: copy-then-delete with warning; S3: server-side copy) | §8.4.3 | I4 | ⬜ |
| Copy public link / presigned link | §8.4.4 | I4 | ⬜ |
| Edit with auto re-upload, "Editing" list | §8.5.2 | I4 | ⬜ |
| Tabs, synchronized browsing | §8.2.3, §8.2.8 | I4 | ⬜ |
| Activity log with filters and export | §8.7.4 | I4 | ⬜ |
| Undo for local UI actions | §8.7.7 | I5 | ⬜ |
| System tray progress | §8.7.8 | I5 | ⬜ |
| CLI (`httporter.com`), scheduled publish, watch folder | §8.8.1–8.8.3 | I6 | ⬜ |
| Create zones / buckets, zone and bucket settings | §8.8.4–8.8.5 | I6 | ⬜ |

## Changes to the milestone split

Two items move earlier than the design document's §15 so every mockup screen has a home:

- **Welcome screen** and **Settings window (Transfers tab)** move into **I2**. Settings › Transfers controls exactly what I2 builds.
- **Command palette** moves from I5 into **I4**, next to the other productivity features.
