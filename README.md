# HTTPorter

A free, native Windows file manager and uploader for cloud storage that speaks HTTP APIs:
**Bunny Storage** and **S3-compatible** services (Amazon S3, Cloudflare R2, Backblaze B2,
Wasabi, DigitalOcean Spaces, MinIO and others), the storage services people use today,
without FTP.

It does two jobs:

- **Publish:** mirror a local folder (a static site export, for example) to a bucket or
  storage zone. Only what changed is uploaded; optionally, files you deleted locally are
  removed; then the CDN cache is purged.
- **Manage:** browse, upload, download, delete, rename and move, preview and edit remote
  files, in two panes side by side.

HTTPorter is its own window: no server, nothing hosted, no web view inside. It talks only to
the storage services and CDNs you add.

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)

## Features

**Sites**
- Bunny Storage zones, added with the zone password or by signing in once with the account
  API key and ticking zones (the key is forgotten afterwards unless you keep it for CDN purges).
- S3-compatible buckets from a preset (Amazon S3, Cloudflare R2, Backblaze B2, Wasabi,
  DigitalOcean Spaces, MinIO) or a custom endpoint; bucket listing when the key allows it.
- Import from a `.env` file or an AWS credentials profile; export and import sites without
  their secrets.
- Test Connection before saving; read-only sites (read-only password or keys) disable every
  write.

**Browsing**
- Two panes (This PC and the site), or one pane with an inspector; tabs; favorites;
  synchronized browsing; list or thumbnail grid.
- Sorting, filter as you type, recursive search of a site by name.
- Quick Look (Space) for images and text or code with syntax colours, and a diff against the
  local copy; Open / Edit downloads, opens in the default app, and re-uploads on every save.
- Copy public (CDN) links, storage links and, on S3, presigned links.
- Command palette (Ctrl+K) and keyboard access throughout.

**Transfers**
- Drag and drop from Explorer onto the site pane or a folder row, and between the panes.
- Many transfers at once on pooled HTTP/2 connections, with adaptive concurrency on 429 / 503,
  retries with backoff, and an optional bandwidth limit.
- Every upload is checksummed (SHA-256 on Bunny; Content-MD5 and a recorded SHA-256 on S3).
  Large S3 files go up in parallel parts and resume after an interruption.
- One sheet per batch for files that already exist (Replace if different, Replace, Skip,
  Keep both).
- A journal: after a crash or a restart, unfinished transfers are offered again. Transfers
  pause when the connection drops and resume when it is back.
- Rename and move: a server-side copy on S3; copy, verify, then delete on Bunny.
- Taskbar progress, and a Windows notification when a long run finishes in the background.

**Publish**
- Publish profiles: a local folder, a site folder, skip rules (`.env`, keys, `.git/` and the
  like by default), whether to remove remote files that are not in the folder, the CDN purge
  and a delete guard.
- Compare first: new, changed, only on the site, and the same, by checksum where the service
  lists one. The preview lists every file before anything happens; a warning names files that
  look like secrets.
- Uploads run before deletes, deletes before the purge, so the live site never links to a
  missing file. Bunny pull zones are purged by URL or as a whole zone.
- History of every publish, with its files; a failed publish can be retried.

**Comfort**
- Light and dark, following Windows; an activity log with filters and export.

## Install

HTTPorter runs on Windows 10 and 11, 64-bit. Each release on the project's GitHub Releases
page has:

- `HTTPorter-<version>-setup.exe`: installs for your Windows account, without admin rights, in
  `%LOCALAPPDATA%\Programs\HTTPorter`, with a Start menu shortcut.
- `HTTPorter-<version>-windows-x64-portable.zip`: unzip anywhere and run `httporter.exe`.
- `SHA256SUMS.txt`: the SHA-256 of both, to check a download with
  `Get-FileHash <file> -Algorithm SHA256`.

HTTPorter 1.0 is not code-signed yet, so Windows SmartScreen may warn the first time you run
the setup: choose **More info**, then **Run anyway**.

### Your data

| What | Where |
|---|---|
| Passwords and keys | Windows Credential Manager only, as `HTTPorter/site/<id>` and `HTTPorter/account/<id>` |
| Sites, profiles, settings, history, the transfer journal | `%APPDATA%\HTTPorter\httporter.db` |
| Activity log (secrets redacted) | `%APPDATA%\HTTPorter\logs` |
| Previews, thumbnails, files open for editing | `%LOCALAPPDATA%\HTTPorter\cache` (cleared of items older than a week) |

Uninstalling keeps your data; **Settings › Security › Reset HTTPorter** removes all of it,
Credential Manager entries included. HTTPorter has no telemetry.

## Building from source

You need:

- Windows 10 or 11, x64;
- [Rust](https://rustup.rs) (stable, the default `x86_64-pc-windows-msvc` toolchain);
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with
  the **Desktop development with C++** workload (the MSVC linker and a Windows SDK).

Then, from the repository root:

```powershell
cargo build --release -p httporter-app
```

The program is `target\release\httporter.exe`, a single file. The update check is compiled in
only when `HTTPORTER_RELEASE_REPO` (`owner/repo` on GitHub) is set at build time; without it
the setting is off.

To build the release files as well (the zip, the installer and their checksums, in `dist\`),
install [Inno Setup](https://jrsoftware.org/isinfo.php) 6.7 or later and Python 3, then run:

```powershell
powershell -NoProfile -File tools/package.ps1
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development setup and checks, and
[docs/architecture.md](docs/architecture.md) for how the code is laid out.

## Licence

HTTPorter is free software, licensed under either of

- the MIT License ([LICENSE-MIT](LICENSE-MIT)), or
- the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)),

at your option. Unless you say otherwise, any contribution you submit for inclusion in
HTTPorter is dual-licensed as above, without any additional terms or conditions.

The user interface is made with [Slint](https://slint.dev), used under the Slint Royalty-free
Desktop, Mobile, and Web Applications License 2.0, which asks for the "Made with Slint" credit
(the badge above, and AboutSlint in Settings › About). The Inter font is under the SIL Open
Font License 1.1. Every component in `httporter.exe` and its licence is listed in
`THIRD-PARTY-NOTICES.txt`, which ships with each release (`python -I tools/third_party.py`
writes it).
