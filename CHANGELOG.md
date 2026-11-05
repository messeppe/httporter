# Changelog

All notable changes to HTTPorter are written down here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - Unreleased

The first public release.

### Added

**Sites**
- Bunny Storage zones, by zone password or by signing in once with the account API key and
  picking zones; the key is forgotten afterwards unless it is kept for CDN purges.
- S3-compatible buckets: Amazon S3, Cloudflare R2, Backblaze B2, Wasabi, DigitalOcean Spaces,
  MinIO, or any other endpoint; bucket listing where the key allows it.
- Import from a `.env` file or an AWS credentials profile; export and import of sites without
  secrets.
- Test Connection; edit, duplicate and remove sites; read-only sites.
- Secrets kept only in Windows Credential Manager.

**Browsing and managing**
- Two panes (This PC and the site) or a single pane with an inspector; tabs; favorites;
  synchronized browsing; list and thumbnail grid.
- Sorting, filter as you type, recursive search of a site.
- Quick Look for images and code, with a diff against the local file; Open / Edit with
  automatic re-upload on save.
- Rename and move (server-side copy on S3, copy-verify-delete on Bunny); new folders; deletes
  with a confirmation that counts files and bytes.
- Public, storage and presigned links; CDN purge of files and folders.
- Command palette (Ctrl+K) and keyboard shortcuts throughout, with a shortcut sheet.

**Transfers**
- Drag and drop from Explorer and between panes; uploads into a folder row.
- Parallel transfers with adaptive concurrency, retries, a bandwidth limit, checksums on every
  upload, and resumable multipart uploads on S3.
- One conflict sheet per batch; a journal that resumes unfinished transfers after a crash or
  restart; automatic pause and resume when the connection drops.
- Taskbar progress, a notification when a long run ends in the background, and a system tray
  icon showing progress when the window is closed during transfers.

**Publish**
- Publish profiles with skip rules, a delete guard, remove-extras and CDN purge options.
- A compare by checksum, a preview of every file before anything happens, a secrets check,
  and uploads before deletes before the purge.
- History of every publish; retry of a failed one.

**The app**
- Light and dark appearance following Windows; Cupertino-style design; the toolbar as the
  title bar, with Snap Layouts; the window's size and place remembered.
- Toasts with Undo for closing a tab or removing a favorite.
- Settings: General, Sites, Transfers, Security, Publish and Advanced. Security › Reset
  HTTPorter removes every setting, site, log and HTTPorter's Credential Manager entries.
- An opt-in update check that says when a new version is out, with its notes and a download
  link; it never installs anything.
- About, with the "Made with Slint" credit.
- Accessibility: screen-reader names for controls, visible focus, full keyboard use; follows
  Windows text size and "Animation effects".
- Resizable, remembered columns.
- The activity log, with filters and export; secrets are redacted.

**Release**
- An installer (per user, no admin rights) and a portable zip, with SHA-256 checksums.
- Licensed MIT OR Apache-2.0, with every third-party licence in `THIRD-PARTY-NOTICES.txt`.

### Known limitations
- Windows 10 and 11 (x64) only.
- English only; the interface is ready for translations.
- Not code-signed yet: SmartScreen may warn on first run.
- Chinese, Japanese, Thai, Lao, Khmer and Burmese text breaks lines by the Unicode rules,
  without dictionary word boundaries.
- Planned for 1.x: a command-line companion, scheduled publish, a watch folder, and creating
  zones and buckets.

