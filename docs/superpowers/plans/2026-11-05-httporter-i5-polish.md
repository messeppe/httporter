# HTTPorter I5 "Polish & release": implementation plan (lean)

> **For agentic workers:** speed mode (see below). Builders work in parallel worktrees; the controller merges.

**Goal:** the I5 rows of `docs/design/mockup-feature-checklist.md` and spec §15 I5 work, and the repository is ready
for a public 1.0:
- **Window:** the toolbar row is the title bar (caption buttons at its right end, drag, double-click, Snap Layouts);
  the app icon in the exe, window and taskbar; window placement remembered.
- **Comfort:** system tray progress (§8.7.8), toasts (§8.7.5), undo (§8.7.7), the remaining shortcuts (§8.7.3).
- **Settings:** the Security tab, "Reset HTTPorter…" (§11), opt-in update check (§16), About with the Slint credit.
- **Polish:** the accessibility pass (§9.1), Windows text scaling and "reduce animations", resizable and remembered
  columns (§8.7.6), the translation pass (M9, Q36), the I1/I2 ledger items marked I5.
- **Release:** licence files, third-party notices, installer + portable zip, signing hook, docs, hardened CI.

**Spec:** `docs/superpowers/specs/2026-10-29-httporter-design.md` (§8.7, §9, §11, §13, §15, §16, B5, B7).

**User decisions (2026-11-05):**
- **Q1 licence:** `MIT OR Apache-2.0`. Slint is used under its royalty-free licence 2.0, which needs the
  "Made with Slint" credit: `AboutSlint` in About, and the badge in the README.
- **Installer:** Inno Setup 6.7.3 (installed per-user: `%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe`), plus a
  portable zip.
- **Release repository:** not chosen yet. The update check reads `option_env!("HTTPORTER_RELEASE_REPO")`
  (`owner/repo`), and is off when that is unset.
- **Signing:** 1.0 ships unsigned. `tools/sign.ps1` signs once a certificate is configured, and otherwise skips.

**Architecture:**
- **Win32 window work** goes in `httporter-platform/src/win/` (new `titlebar.rs`, `placement.rs`, `tray.rs`,
  `a11y.rs`), on the existing `windows` crate.
- **The app gets three module folders**, each owned by one track:
  - `src/window/` (title bar, icon, placement);
  - `src/comfort/` (tray, toasts, undo, shortcuts);
  - `src/system/` (security, reset, update, about).
- **Each folder has its own `copy.rs`.**
- **No new heavy dependencies:** `embed-resource` (build only), and `reqwest` (already linked) for the update check.
- **No exe-size budget** (user, 2026-11-05: "if you're losing features or function then why make it less than 25 mb"). S8 is dropped; the earlier size trims that cost function are undone (patched parley without ICU4X dictionaries, opt-level "z", the Inter subset), and the GPU renderer comes back as an opt-in Advanced setting.

**Speed mode (user rule: build everything first, test at the end):**
- **No per-task tests or reviews.**
- **A task is done** when all of these hold:
  - `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` pass;
  - `cargo test --workspace --no-run` compiles;
  - the app starts: `httporter.exe` from the debug build is still running 5 s after `Start-Process`, then
    `Stop-Process`;
  - the work is committed.
- **A task that breaks an existing test on purpose** (new fields, changed copy) fixes that test. All tests run in T10.

**Rules:**
- **Look:**
  - Reuse `Theme`, `CapsuleButton`, `CheckRow`, `AlertCard`, `Modal`, `chips.slint`, `form_controls.slint` and the
    Settings `Section` / `Group` / `GroupRow`.
  - Follow the Apple HIG, adapted to Windows: Windows caption buttons and Segoe Fluent-like glyphs drawn as SVG paths.
- **Strings:** Slint text goes through `@tr()`. Rust user-facing strings only in `copy.rs` files. `lint_tests.rs`
  checks both, and T1 makes it accept the three new `copy.rs` files.
- **Secrets:**
  - Only in Credential Manager: never in SQLite, settings, exports, logs (`redact`), the UI or error text.
  - Reset and "Remove Unused Keys" delete only targets HTTPorter wrote, never another app's.
- **Safety:**
  - No `panic = "abort"`, no UPX or exe packing.
  - No `.env`, except `.env.test` via dotenvy in live tests.
  - Tests never write to `exampleshop` and never purge pull zone 7654321.
- **Unwired controls:** disabled, with "Arrives in a later version".
- **Builds:**
  - One worktree per builder. `CARGO_BUILD_JOBS=2`.
  - `TMP` / `TEMP` = `D:\Temp\httporter-build`.
  - `CARGO_TARGET_DIR=D:\Temp\httporter-target\<track>`. D:\Temp is outside iCloud sync, and the controller seeds
    each track's folder from T1's.
- **`Cargo.lock` conflict:** take either side, then `cargo metadata --format-version 1 >NUL`. Never `cargo update`.
- **Agents never drive the user's desktop.** The 5 s start check is the only GUI run.

## Global constraints (verbatim from the spec)
- S6 cold start ≤ 1 s; S7 idle ≤ 40 MB private; S8 dropped by the user (no exe-size budget).
- §9.1: "WCAG AA contrast, visible focus, full keyboard use, accessible names on icon buttons (AccessKit/UI
  Automation), follows Windows text scaling"; motion "150–250 ms … honors Windows 'reduce animations'".
- §16: "opt-in check of the release feed at startup; shows 'HTTPorter 1.2 is available' with release notes and a
  download link; no silent install". §13: "No telemetry."
- §11: "'Reset HTTPorter' in Settings removes all of it, including credential-store entries, after confirmation."
- §8.7.7: "Undo for local UI actions (closing a tab, removing a favorite); remote deletes say plainly they can't be
  undone."
- §8.7.8: "System tray icon (optional) showing transfer progress when the window is closed during a long publish."

## Review focus (no task test covers these; T10 does, in this order)
1. **Placement:** restoring a window saved on a monitor that is now unplugged. The window opens on a visible monitor,
   at its saved size if that fits, otherwise centred at its default size.
2. **Title bar and DPI:**
   - At 150 % and 200 %, and after dragging between monitors with different DPI, the Snap Layouts flyout still opens
     over the maximize button.
   - The top-edge resize still works when the window is not maximized.
   - When maximized, nothing is cut off at the top.
3. **Tray:**
   - Closing during transfers into the tray, then quitting from the tray, shows the same quit choices.
   - The journal keeps every unfinished transfer.
   - When the last batch finishes with the window hidden, the process notifies, then exits.
4. **Reset:**
   - It is refused while transfers run.
   - It removes only HTTPorter's own credential targets.
   - A second instance never runs it at the same time.
   - The relaunch lands on Welcome.
5. **Update check:** offline, a GitHub 403 or 404, or a malformed answer is silent: logged to the activity log, with
   no toast and no modal. It never runs without opt-in, or more than once in 24 h.

---

## 0. Shared interfaces (T1 commits these; everyone codes against them)

- **Names and signatures below are fixed.** Ask the controller before changing one. Private items and extra fields
  are fine.
- **Startup paths are never `todo!()`:**
  - every `install`, `run_pending_reset`, `text_scale`, `animations_enabled` and placement restore get a real
    no-op or default body in T1;
  - other stubs are `todo!("T<n>")`, reached only from a click.

### 0.1 `httporter-core` (`places.rs`, real in T1)
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPlacement { pub x: i32, pub y: i32, pub width: u32, pub height: u32, pub maximized: bool } // physical px, normal (restored) outer rect
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ColumnWidths { pub size: u16, pub modified: u16, pub status: u16 }  // logical px; 0 = the default width
// UiState gains (serde default each):
pub window: Option<WindowPlacement>,
pub columns: ColumnWidths,
pub tray: bool,               // default true: "Keep running in the tray…"
pub check_updates: bool,      // default false (opt-in)
pub last_update_check: i64,   // unix seconds, 0 = never
```

### 0.2 `httporter-platform`
```rust
// creds.rs: added to the CredentialStore trait (MemoryStore real in T1; KeyringStore todo!("T4"))
fn keys(&self) -> Result<Vec<CredentialKey>, CredError>;   // every target this app wrote (service "HTTPorter"), none else
impl CredentialKey { pub fn kind(&self) -> CredentialKind; pub fn id(&self) -> &str; }
pub enum CredentialKind { Site, Account, Other }

// win/titlebar.rs (T2)
pub enum CaptionEvent { MaxHover(bool), MaxPressed(bool), MaxClicked }
pub struct CaptionRect { pub left: i32, pub top: i32, pub right: i32, pub bottom: i32 }   // physical client px
pub struct TitleBar { /* keeps the subclass; Drop restores the old wndproc */ }
impl TitleBar {
    /// Removes the system caption, keeps resize edges, shadow and Snap; top-edge resize when not maximized;
    /// HTMAXBUTTON over the max rect (Win11 Snap Layouts); WM_SETICON from exe icon resource 1.
    pub fn install(hwnd: isize, on_event: Box<dyn Fn(CaptionEvent)>) -> Result<TitleBar, String>;
    pub fn set_max_rect(&self, rect: Option<CaptionRect>);    // None = window has no max button (Settings)
}
pub fn is_maximized(hwnd: isize) -> bool;
pub fn toggle_maximize(hwnd: isize);
pub fn minimize(hwnd: isize);
pub fn show_system_menu(hwnd: isize, screen_x: i32, screen_y: i32);
pub fn start_drag(hwnd: isize);   // ReleaseCapture + WM_NCLBUTTONDOWN HTCAPTION (Aero Snap drag)

// win/placement.rs (T2)
pub fn placement(hwnd: isize) -> Option<WindowPlacement>;          // GetWindowPlacement
/// The saved rect if it intersects a monitor (MonitorFromRect, MONITOR_DEFAULTTONULL), else None.
pub fn visible_placement(saved: &WindowPlacement) -> Option<WindowPlacement>;

// win/tray.rs (T3)
pub enum TrayEvent { Show, PauseResume, Quit }
pub struct Tray { /* message-only window + NOTIFYICONDATAW; Drop deletes the icon */ }
impl Tray {
    pub fn new(tooltip: &str, labels: [&str; 3], on_event: Box<dyn Fn(TrayEvent)>) -> Result<Tray, String>;
    pub fn set_tooltip(&self, text: &str);          // cut to 127 UTF-16 units with "…"
    pub fn set_labels(&self, labels: [&str; 3]);    // Show HTTPorter / Pause All|Resume All / Quit
}

// win/a11y.rs (T5; T1 bodies return the defaults)
pub fn text_scale() -> f32;            // UISettings.TextScaleFactor (WinRT), clamped 1.0..=2.25; 1.0 on error
pub fn animations_enabled() -> bool;   // SPI_GETCLIENTAREAANIMATION; true on error

// store.rs: none new for I5 except what T5 fixes inside existing functions (ledger 118/138/144).
```

### 0.3 `httporter-app` Slint (`app.slint` / `types.slint`, wired by T1)
```slint
// types.slint
export struct ToastData { id: int, text: string, action: string /* "" = none */, kind: int /* 0 info 1 done 2 warning */ }

// theme.slint: Theme global gains (T5 uses them)
in property <float> text-scale: 1.0;    in property <bool> motion: true;

// AppWindow: window chrome (T2)
in property <bool> window-maximized;  in property <bool> window-active: true;
in property <bool> caption-max-hovered;  in property <bool> caption-max-pressed;
out property <length> caption-max-x;  out property <length> caption-max-y;           // absolute, logical
out property <length> caption-max-width;  out property <length> caption-max-height;
callback caption-minimize();  callback caption-toggle-maximize();  callback caption-close();
callback caption-drag();  callback caption-menu(length, length);  callback caption-layout-changed();
// AppWindow: comfort (T3)
in property <[ToastData]> toasts;  callback toast-action(int);  callback toast-dismissed(int);
callback undo();  callback reopen-tab();  callback switch-pane();  callback go-up();
// AppWindow: columns (T5)
in-out property <length> col-size-width;  in-out property <length> col-modified-width;
in-out property <length> col-status-width;  callback columns-resized();
```
**Placement in the window:**
- `CaptionButtons` (new `components/caption_buttons.slint`, T2) sits at the right end of `Toolbar`.
- A background `TouchArea` under the toolbar's controls calls these:
  - left press → `caption-drag`;
  - double-click → `caption-toggle-maximize`;
  - right-click → `caption-menu`.
- `ToastStack` (new `components/toast.slint`, T3) overlays the window bottom-centre, above the drawer.

**Settings window:**
- It gets the same `CaptionButtons` with Close only, and `caption-*` callbacks of its own, prefixed `settings-`.
- The Security tab's panel becomes a `SecurityTab` component (new `components/security_tab.slint`, T4). Its
  properties:
  ```slint
  in property <int> site-keys; in property <int> account-keys; in property <int> unused-keys;
  in property <string> security-message; in property <bool> reset-allowed; in property <string> reset-note;
  callback remove-unused-keys(); callback open-credential-manager(); callback reset-requested(); callback reset-confirmed();
  ```
- The General tab gains three things (T4):
  - a "Tray" row bound to `tray`;
  - an "Updates" section: `check-updates` (bool), `updates-available: bool` (false when the build has no release
    repo), `updates-note: string`, `callback check-now()`;
  - an "About" section: `app-version: string`, `licenses-available: bool`, `callback open-licenses()`, and
    `AboutSlint {}`.

**Keys (T1 adds them to the main `FocusScope`, outside text fields):**

| Key | Calls |
|---|---|
| Ctrl+Z | `undo()` |
| Ctrl+Shift+T | `reopen-tab()` |
| Ctrl+, | the existing `open-settings()` |
| Alt+↑ | `go-up()` |
| F6 | `switch-pane()` |

### 0.4 `httporter-app` Rust (T1 creates the files, with stubs)
```rust
// main.rs: system::reset::run_pending_reset() runs before the store opens; window::install, comfort::install,
//   system::install run after the existing installs. ui.run() becomes ui.show() + slint::run_event_loop_until_quit()
//   with today's close behaviour unchanged (T1), so T3 can hide the window into the tray.
// window/mod.rs (T2)
pub fn install(ui: &AppWindow, session: &Arc<Session>);   // title bar, icon, placement restore + save on close
// comfort/mod.rs (T3)
pub fn install(ui: &AppWindow, m: &Arc<Manager>, transfers: &Arc<Transfers>);
// comfort/toast.rs
pub enum ToastKind { Info, Done, Warning }
pub struct ToastAction { pub label: String, pub run: Box<dyn FnOnce()> }   // UI thread
pub fn show(ui: &AppWindow, kind: ToastKind, text: impl Into<String>, action: Option<ToastAction>); // 4 s, 8 s with action, ≤ 3 shown
// comfort/undo.rs
pub enum UndoAction { ReopenTab { tab: TabState, index: usize }, RestoreFavorite(Favorite) }
pub fn push(ui: &AppWindow, action: UndoAction);   // keeps the last 20; shows a toast with "Undo"
pub fn undo(ui: &AppWindow, m: &Arc<Manager>) -> bool;
// system/mod.rs (T4)
pub fn install(ui: &AppWindow, m: &Arc<Manager>, transfers: &Arc<Transfers>);
// system/reset.rs
pub fn run_pending_reset();   // marker %APPDATA%\HTTPorter\reset-pending → wipe, then start fresh; T1: no-op if no marker
// system/update.rs
pub const RELEASE_REPO: Option<&str> = option_env!("HTTPORTER_RELEASE_REPO");
pub struct Release { pub version: String, pub url: String, pub notes: String }
pub async fn latest(client: &reqwest::Client, repo: &str) -> Result<Release, String>;
pub fn is_newer(candidate: &str, current: &str) -> bool;
```

### 0.5 T1 as built
Every §0 name and signature above is as written, except where noted here.
- **Cargo:** the path dependencies' `version = "0.1.0"` became `"1.0.0"` (the workspace version). `embed-resource`
  is in `httporter-app`'s `[build-dependencies]` only, not yet called from `build.rs` (T2). `Cargo.lock` gained
  only `embed-resource`, `vswhom(-sys)` and `winreg` (all build-only).
- **`CredentialKey::id()`** is the text after `HTTPorter/site/` or `HTTPorter/account/`; for `Other`, the whole key.
  `CredentialKind` derives `Debug, Clone, Copy, PartialEq, Eq, Hash` and is re-exported from the crate root.
  `MemoryStore::keys` returns its `HTTPorter/…` keys sorted. The test `SpyStore` forwards `keys`.
- **`ColumnWidths`** also has `#[serde(default)]`; `WindowPlacement` has no `Default` (as written).
- **`CaptionButtons`** (Slint) names its outputs `max-button-x/-y/-width/-height`: `max-width` / `max-height` are
  built-in Slint properties. `Toolbar` and `AppWindow` keep the §0.3 names (`caption-max-*`). `AppWindow` raises
  `caption-layout-changed` from `changed` handlers on the four `caption-max-*`; it is wired to a no-op, since the
  first layout raises it.
- **`caption-close`** (and the Settings window's `settings-caption-close`) is wired for real in T1: it dispatches
  `WindowEvent::CloseRequested`, so it runs the window's own close handling. Minimize, maximize, drag and the
  menu are `todo!("T2")`.
- **Settings window:** `settings-caption-close()`, `settings-caption-drag()`, `settings-caption-menu(length,
  length)` and `in property <bool> settings-window-active`. No minimize, maximize or layout callback. Its tray,
  update, About and Security properties and callbacks have the same names as on `GeneralTab` / `SecurityTab`.
  The caption buttons and the drag area sit in the tabs row for now (T2 makes the title row).
- **`settings_tabs.slint`** now exports `Section`, `Group`, `GroupRow`, `Switch`, `Note` and `TabColumn`, so that
  `security_tab.slint` can import them.
- **Two hooks not in §0.4:** `window::wire_settings(&SettingsWindow)` (T2) and `system::wire_settings(&SettingsWindow)`
  (T4). `transfers::settings_window()` calls them each time the Settings window is made, so neither track edits
  that function. `system::wire_settings` wires the Security and General callbacks to `todo!("T4")`.
- **`Manager`'s** `session`, `transfers`, `publisher` and `link` fields are `pub(crate)`, for the three new folders.
- **`run_pending_reset()`** is the first line of `main()`, before the single-instance check, because it waits for
  the old instance's mutex. It is still before the store opens.
- **Main loop:** `ui.show()`, then `slint::run_event_loop_until_quit()`, then `ui.hide()` (what `ui.run()` did). To
  keep closing the same, `transfers::install_close` calls `slint::quit_event_loop()` where it returns `HideWindow`.
  T3: a hide into the tray must return `HideWindow` *without* that call.
- **The installs:** `main.rs` gets the `Arc<Manager>` from `manage::manager()` and passes `&manager.session` to
  `window::install`. `Theme.text-scale` / `motion` are set on the main window only. The Settings window's `Theme`
  is T5's to set when it pushes the live values.
- **`AppWindow.col-*-width`** default to the `Cols` global's widths (76, 128 and 96 px). Nothing binds them yet (T5).
- **Keys:** Ctrl+, opens Settings only while `settings-available`. F6 also fires with Shift. All five are inside
  the `!TextInputInterface.text-input-focused` block. The pane rejects Alt-modified keys, so Alt+↑ reaches the
  scope.

---

## Tasks

### T1: Skeleton (alone first; `CARGO_BUILD_JOBS=3`)
**Files:** `Cargo.toml` (workspace), the three crate `Cargo.toml`s, `crates/httporter-core/src/places.rs`,
`crates/httporter-platform/src/{creds.rs,win/mod.rs}`, new `win/{titlebar,placement,tray,a11y}.rs`, `ui/app.slint`,
`ui/types.slint`, `ui/theme.slint`, new `components/{caption_buttons,toast,security_tab}.slint`,
`components/{toolbar,settings_window,settings_tabs}.slint` (insertion points only), `src/main.rs`, new
`src/{window,comfort,system}/` with `mod.rs` + `copy.rs` + the files of §0.4, `src/lint_tests.rs`.

**Cargo:**
- Workspace: `version = "1.0.0"`, `license = "MIT OR Apache-2.0"`, `repository` left out until the user picks one.
- Each crate: `license.workspace = true`.
- `slint-build` and `i-slint-backend-testing` versions move into `[workspace.dependencies]` (ledger 84).
- `httporter-app`:
  - add `reqwest` (workspace, the same features as `httporter-bunny`, so nothing new links);
  - add `embed-resource = "3"` as a build-dependency;
  - add `slint` feature `raw-window-handle-06` (for picker owners).
- `windows` features: `Win32_Graphics_Dwm`, `Win32_Graphics_Gdi`, `Win32_Security_Credentials`,
  `Win32_UI_Accessibility`, `UI_ViewManagement`, `Win32_System_LibraryLoader`.

**Real in T1:**
- §0.1;
- `MemoryStore::keys`;
- the Slint interface, wired to the stub callbacks;
- the key handling;
- the main-loop change;
- `lint_tests` scope.

`Theme.text-scale` / `motion` are set from `a11y::text_scale()` / `animations_enabled()` at start.

**Done:**
- Speed-mode done.
- A one-page "T1 as built" note appended to §0 for any signature that had to bend.

### T2: Window chrome (worktree `i5-a`)
**Files:** `win/{titlebar,placement}.rs`, `components/{caption_buttons,toolbar}.slint`, the Settings-window title row
in `components/settings_window.slint`, `src/window/`, `build.rs`, new `assets/icon/`, `tools/icon/` (a throwaway
renderer project, excluded from the workspace), `win/pickers.rs`.

**Title bar** (Win32, on the winit HWND that `WinitWindowAccessor` already gives `transfers.rs`):
- **Hook:** swap the wndproc with `SetWindowLongPtrW(GWLP_WNDPROC)` and `CallWindowProcW` to winit's.
- **`WM_NCCALCSIZE` (wParam TRUE):** keep the left, right and bottom frame; set the top to the client top. When
  maximized, inset the top by the frame thickness (`GetSystemMetricsForDpi(SM_CYFRAME) + SM_CXPADDEDBORDER`).
- **Shadow:** `DwmExtendFrameIntoClientArea` with a top margin of 1.
- **`WM_NCHITTEST`:**
  - `HTTOP` / `HTTOPLEFT` / `HTTOPRIGHT` within the resize border when not maximized;
  - `HTMAXBUTTON` inside the max rect;
  - otherwise `DefWindowProc`'s answer, then `HTCLIENT`.
- **Max button by hand:** `WM_NCMOUSEMOVE` / `WM_NCMOUSELEAVE` / `WM_NCLBUTTONDOWN` / `WM_NCLBUTTONUP` on
  `HTMAXBUTTON` raise the `CaptionEvent`s and are swallowed, so Windows draws no classic button.
- **The app pushes the max rect** (logical px × `scale_factor()`) on `caption-layout-changed`, on maximize changes
  and on scale changes.
- **Window state:** `window-maximized` follows `WM_SIZE`, and `window-active` follows `WM_ACTIVATE` (captions dim
  when inactive).

**`CaptionButtons`:**
- Minimize, Maximize/Restore and Close, 46 × the toolbar height. Close's hover is red `#C42B1C` with a white glyph.
- Glyphs are 10 px SVG paths (Segoe Fluent-like).
- Each has an `accessible-role: button` and a label ("Minimize", "Maximize" / "Restore", "Close").
- Toolbar controls keep their own presses. Only the empty toolbar background drags.

**Settings window:** the same title row, Close only, title centred (mockup `Settings.dc.html`).

**Icon:**
- `assets/icon/httporter.svg`:
  - a rounded square (radius 22 % of the side);
  - the gradient `Theme.app-icon-top` → `app-icon-bottom` at 160°;
  - the white mark of `ui/icons/porter.svg` at 60 % of the side, centred.
- `tools/icon` (its own Cargo project) renders it with `resvg` at 16, 20, 24, 32, 40, 48, 64 and 256 px. It writes
  `assets/icon/httporter.ico` (256 as PNG, the rest as BMP) and `httporter-32.png`. Both are committed.
- `build.rs` compiles `assets/icon/httporter.rc` with `embed-resource`:
  - `1 ICON`;
  - `VERSIONINFO`: ProductName HTTPorter, FileVersion and ProductVersion from `CARGO_PKG_VERSION`, LegalCopyright
    "MIT OR Apache-2.0", FileDescription HTTPorter;
  - a manifest (Common Controls v6, PerMonitorV2, longPathAware, supportedOS Win10/11).
- The window icon comes from `WM_SETICON` (`LoadIconW` resource 1, small and big sizes).

**Placement:**
- On start, `UiState.window` → `visible_placement` → Slint `set_position` / `set_size` (+ `set_maximized`) before the
  window shows. None → today's size, centred.
- Saved on close, from `placement(hwnd)`.

**Pickers:** every `rfd` dialog gets the owner window (ledger 187).

**Ledger:** 131 (sidebar toggle `accessible-checked`; `ErrorBanner` role `alert`).

### T3: Comfort (worktree `i5-b`)
**Files:** `win/tray.rs`, `components/toast.slint`, `src/comfort/`, `src/manage/{tabs,places,palette_view,links,edit}.rs`,
the quit / close branch in `src/transfers.rs` (`on_close_requested` and `quit-keep-running` only).

**Tray:**
- Setting `UiState.tray`.
- When the window is closed while transfers run:
  - the quit sheet's "Keep Running" hides the window (`ui.hide()`) and shows the tray icon (exe icon resource 1);
  - when `tray` is off, "Keep Running" keeps the window, as today.
- Tooltip "HTTPorter: Uploading 1,204 of 13,793 · 43 files/s", updated ≤ 1 Hz from the same engine snapshot as
  the taskbar.
- Left-click shows the window.
- Menu: Show HTTPorter / Pause All (Resume All) / Quit. Quit shows the window and its quit sheet.
- When the queue empties with the window hidden: the finish notification (existing), then the tray icon goes, then
  `quit_event_loop()`.

**Toasts:**
- At most 3, newest at the bottom.
- A toast hides after 4 s, or after 8 s when it has an action. Hover pauses the timer.
- `Esc` with a toast focused dismisses it.
- Use them for:
  - "Link copied" / "N links copied";
  - edit re-upload done / failed (§8.5.2);
  - favorite removed and tab closed, each with Undo;
  - purge done.

  Other status-line messages stay where they are.

**Undo:**
- `tab-close` pushes `ReopenTab`; `favorite-remove` pushes `RestoreFavorite`.
- `Ctrl+Z` / the toast's Undo / `Ctrl+Shift+T` (reopen-tab = undo of the newest `ReopenTab`) restore them.
- Remote deletes are not undoable, and their alert already says so.

**Keys:**
- `switch-pane` toggles the focus between the two panes (F6). In single-pane mode it moves between the list and the
  inspector.
- `go-up` = the pane's Backspace action.
- The palette's shortcut list (`?`) gains the five new keys, and "Undo" and "Reopen Closed Tab" commands.

### T4: System: Security, Reset, Updates, About (worktree `i5-c`)
**Files:** `creds.rs` (`KeyringStore::keys`), `components/security_tab.slint`, the General-tab sections in
`components/settings_tabs.slint`, `src/system/`, `src/manage/settings_tabs.rs` (General rows only).

**`KeyringStore::keys`:**
- `CredEnumerateW` with the filter that matches keyring's target form for service "HTTPorter". Read keyring's
  Windows target format in its source in `~/.cargo/registry` and match it exactly.
- Parse each target back to a `CredentialKey`. Anything that doesn't parse is skipped, never deleted.

**Security tab:**
- **"Saved passwords and keys" section:**
  - "Stored in Windows Credential Manager, never in HTTPorter's files."
  - Counts "N sites · N account keys".
  - "Remove Unused Keys" removes keys whose site or account no longer exists (M7). It is disabled at 0, and its
    result is shown in `security-message`.
  - "Open Credential Manager" runs `control.exe /name Microsoft.CredentialManager`.
- **"Reset" section:** "Reset HTTPorter…" opens an `AlertCard`:
  - text "Reset HTTPorter? This removes all sites, publish profiles, history, settings, logs and cached previews, and
    the passwords and keys HTTPorter saved on this PC. Files on your sites are not touched.";
  - buttons "Reset HTTPorter" (destructive, not the default) and "Cancel" (default);
  - disabled while transfers or edits are pending, with `reset-note` "Finish or cancel the transfers first."

**Reset:**
- Confirming writes the marker, starts `httporter.exe --after-reset` and quits.
- `run_pending_reset` (before the store opens) then does this:
  1. waits ≤ 10 s for the single-instance mutex `Local\HTTPorter.Running` to be free;
  2. deletes every `keys()` target;
  3. deletes `%APPDATA%\HTTPorter` (DB, logs) and `%LOCALAPPDATA%\HTTPorter` (cache);
  4. removes the marker last.
- On any error the marker stays, and the next start retries.

**Updates:**
- General › Updates:
  - "Check for updates at start" (off by default);
  - "Check Now";
  - a note under it with the last result.
- The check: `GET https://api.github.com/repos/{repo}/releases/latest` with `User-Agent: HTTPorter/<version>` and
  `Accept: application/vnd.github+json`, 5 s timeout, no other headers.
- It reads `tag_name` (a leading `v` stripped), `html_url` and the first 600 characters of `body`.
- If `is_newer`: a toast "HTTPorter X is available" with the action "View Release" (opens `html_url` in the default
  browser), and the note shows the notes text.
- At start, it runs only if `check_updates` is on, `RELEASE_REPO` is set and `last_update_check` is more than 24 h
  ago.
- Failures go only to the activity log and the note.
- With no `RELEASE_REPO`, the rows are disabled, with "This build has no release address."

**About** (General, last section):
- the app icon, "HTTPorter 1.0.0", and "Free software under the MIT or Apache-2.0 license";
- "Third-Party Licenses…" opens `THIRD-PARTY-NOTICES.txt` beside the exe, and is disabled if the file is missing;
- `AboutSlint {}`.

The palette gains "About HTTPorter" (opens Settings › General) and "Check for Updates".

**Tray row:** "Keep running in the tray when the window is closed during transfers."

### T5: Accessibility, polish, translation, robustness (worktree `i5-d`)
**Files:** `win/a11y.rs`, `ui/theme.slint`, `components/{sidebar,pane,chips,drawer,statusbar,layout,inspector}.slint`,
`src/{copy,format,drawer_view,viewmodel,settings_view}.rs`, `src/publish/copy.rs` (M9 passthrough),
`src/drop_hook.rs` (the settings-change trigger only), `crates/httporter-platform/src/store.rs` (settings functions
only), the `resume_journal` part of `src/transfers.rs`. Not `src/manage/settings_tabs.rs` (T4's).

**Text scaling and motion:**
- `text_scale` and `animations_enabled` get real bodies.
- `Theme` font sizes × `text-scale`. Row heights and fixed sizes that hold text grow with it (ledger 198 sizes).
- Every `animate` has `duration: Theme.motion ? Nms : 0ms`.
- Both are read again on `WM_SETTINGCHANGE` (via `drop_hook`'s winit event hook: `ScaleFactorChanged` /
  `ThemeChanged` as the trigger) and pushed to `Theme`.

**Accessibility (ledger):**
- 128: tile colours into `Theme`; mono tile text AA.
- 130: health in the `SiteRow` label and a second cue (shape), not colour only.
- 153: a disabled button drops its focus ring.
- 154: dark sidebar focus ring ≥ 3:1 with margin.
- 155: the site list is one Tab stop with ↑/↓.
- 202: elided crumbs get a tooltip.
- 219: the Add Site outline ≥ 3:1.
- 123: the theme tint fallback.
- Audit:
  - every icon-only button has an `accessible-label`;
  - every toggle has `accessible-checked`;
  - every sheet focuses its default button.

  Fix what's missing in the files above. Report anything in T2–T4 files to the controller instead.

**Columns:**
- The Size, Modified and Status headers get a 6 px drag handle on their left edge. Widths are 60–400 px, and the
  Name column takes the rest.
- Double-clicking a handle restores the default.
- `columns-resized` saves `UiState.columns` (debounced 500 ms). The widths are applied at start.

**Translation pass:**
- M9: passthrough provider and planner text gets sentence case and goes through `copy.rs` functions.
- Q36: the `format.rs` word tables and viewmodel's ", " move into `copy.rs`.
- Add `translations/README.md`: how to add a language with Slint bundled translations and the `.po` workflow. No
  languages yet.

**Robustness:**
- M13: drawer state precedence. User-paused + offline reads Paused, and the button says "Resume".
- M14: `resume_journal` per site. One site's missing password doesn't block the others; that site's rows stay for
  later.
- M15: folder-delete rows get the folder icon.
- 118: one bad stored field falls back to that field's default, not all `TransferSettings`.
- 138 / 144: `Store::settings` returns transient errors. The first Settings edit writes only the edited field.

### T6: Release files (worktree `i5-r`; no app code, so it can run alongside T1)
**Files:** `LICENSE-MIT`, `LICENSE-APACHE`, `deny.toml`, `tools/{third_party.py,package.ps1,sign.ps1,spdx_headers.py,
check.ps1}`, `installer/httporter.iss`, `README.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`,
`CHANGELOG.md`, `docs/architecture.md`, `.github/workflows/{ci.yml,release.yml}`, `.gitignore` (`dist/`).

**Licences:**
- `LICENSE-MIT`: "Copyright (c) 2026 the HTTPorter contributors".
- `LICENSE-APACHE`: the full Apache-2.0 text.
- `deny.toml`:
  - drop the GPL exceptions;
  - the Slint crates are allowed through `LicenseRef-Slint-Royalty-free-2.0`, as an exception per Slint crate (read
    the exact expression from `cargo metadata`);
  - keep the MPL-2.0 / BSL-1.0 / Unlicense exceptions, with notes;
  - any GPL / AGPL / LGPL anywhere is an error;
  - dev-dependencies are licence-checked (90);
  - `bans`: wildcards denied, duplicate versions warn (85).

**`tools/third_party.py`** (stdlib only, run as `python -I`):
- `cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc`;
- the normal and build dependency closure of `httporter-app`;
- per crate: name, version, licence expression, repository, and the texts of `LICENSE*` / `COPYING*` / `NOTICE*` in
  its source folder (identical texts printed once with the crate list);
- also Inter (OFL-1.1, from `ui/fonts`), and the Slint royalty-free licence text;
- writes `THIRD-PARTY-NOTICES.txt`; `--check` fails if a crate has no licence text.

**`installer/httporter.iss`:**
- `AppId` = a fixed GUID (write it once). `PrivilegesRequired=lowest`. `DefaultDirName={autopf}\HTTPorter`, which is
  `%LOCALAPPDATA%\Programs\HTTPorter`, where it is installed today.
- `AppMutex=Local\HTTPorter.Running`, `CloseApplications=yes`.
- `SetupIconFile=..\assets\icon\httporter.ico`, `WizardStyle=modern`, `ArchitecturesAllowed=x64compatible`,
  `ArchitecturesInstallIn64BitMode=x64compatible`, `MinVersion=10.0`.
- Files: exe, both licences, notices, README.txt.
- A Start-menu shortcut; a desktop icon as an unchecked task; "Launch HTTPorter" on finish.
- Uninstall keeps user data, and the uninstaller says "Settings › Security › Reset HTTPorter removes it."
- `/DAppVersion` and `/DSourceDir` come from the command line. `SignTool` is used only when `/DSign` is given.

**`tools/package.ps1 [-TargetDir]`:**
1. `cargo build --release --locked -p httporter-app`;
2. report the exe size (no budget);
3. stage `dist\HTTPorter-<ver>\`;
4. `sign.ps1` on the exe;
5. `HTTPorter-<ver>-windows-x64-portable.zip`;
6. ISCC → `HTTPorter-<ver>-setup.exe`, then `sign.ps1` on it;
7. `SHA256SUMS.txt`;
8. print the sizes.

**`tools/sign.ps1 <file>`:**
- Uses the SDK `signtool.exe` with `/fd SHA256 /tr http://timestamp.digicert.com /td SHA256`.
- `HTTPORTER_SIGN_THUMBPRINT` → `/sha1`. Or `HTTPORTER_SIGN_DLIB` + `HTTPORTER_SIGN_METADATA` → Azure Trusted Signing
  `/dlib /dmdf`.
- Neither set → prints "unsigned: no signing certificate configured" and exits 0.

**`tools/spdx_headers.py [--check]`:** adds `// SPDX-License-Identifier: MIT OR Apache-2.0` as the first line of
every `.rs` and `.slint` under `crates/` and `tools/`, except `vendor/` and generated files. The controller runs it
after the merge (T7), so the tracks don't conflict.

**Docs:**
- `README.md`: what HTTPorter is, features, install (setup or zip), building from source (Rust MSVC, VS Build Tools,
  `cargo build --release -p httporter-app`), licence, and the "Made with Slint" badge linking to slint.dev.
- `CONTRIBUTING.md`: setup, `tools/check.ps1`, commit style, and how to add a provider adapter, walking through the
  `StorageProvider` trait, `Capabilities`, the mock provider and the tests.
- `docs/architecture.md`: crates, threads, data and secrets (from spec §10–§11, as built).
- `CODE_OF_CONDUCT.md`: Contributor Covenant 2.1.
- `SECURITY.md`: report privately (GitHub security advisories once the repository is public).
- `CHANGELOG.md`: 1.0.0.

**CI** (88):
- Actions pinned by commit SHA (with the tag in a comment); `permissions: contents: read`; `--locked` everywhere.
- `spdx_headers.py --check` and `third_party.py --check` steps.
- `release.yml` on tags `v*`: `package.ps1`, then upload to a draft release. Only that job gets
  `contents: write`. The signing step runs only when its secrets exist.
- `HTTPORTER_RELEASE_REPO` is set from `github.repository`.

**`check.ps1`** (86, 101): `$ErrorActionPreference` handled; native stderr no longer marks PS 5.1 runs failed.

### T7: Merge, headers, size, package, install (controller)
1. **Merge into `feat/i5-polish`**, in this order: T1, then T6, T2, T3, T4, T5. Fix the compile after each merge.
2. **Headers:** `python -I tools/spdx_headers.py`, then commit.
3. **Checks:** release build in `D:\Temp\httporter-target\merge`; `third_party.py`; `package.ps1`.
4. **Size:** reported only; no budget.
5. **Install** the exe the way it's installed today. The setup exe is left in `dist\` for the user to try. A launch
   smoke runs, and the ledger and memory are updated.

### T10: Tests, gate, measurements (**only after the user has used the build**; also covers the skipped I3 and I4 T10)
- **Unit tests:**
  - `is_newer` (1.0.0 < 1.0.1 < 1.1.0; v prefix; pre-release never newer; garbage false);
  - `latest` against wiremock (200, 403, 404, malformed, timeout);
  - `visible_placement`;
  - the `UiState` round trip with the new fields, and old JSON;
  - `CredentialKey::kind` / `id`;
  - `keys()` on MemoryStore;
  - `run_pending_reset` on a temp profile with a fake store;
  - undo (tab, favorite, the cap of 20);
  - the toast queue (cap 3, timers);
  - the tray tooltip cut at 127;
  - column width clamp;
  - M13, M14, 118;
  - `lint_tests`.
- **Gate:** `tools/check.ps1` exits 0 (with the deny and SPDX checks), and `package.ps1` succeeds.
- **Measurements** with `tools/measure.ps1` on the user's PC, release build: S1–S10 (S1 and S10 on the test zone
  and bucket only), each with its target.
- **User hands-on:**
  1. drag, snap, Snap Layouts, double-click and the system menu on the toolbar;
  2. the icon in the taskbar and Alt+Tab;
  3. close-to-tray during a long upload;
  4. toast Undo;
  5. Narrator through the sidebar, panes, drawer and sheets;
  6. text size 150 %;
  7. animations off;
  8. Reset on a copy of the profile;
  9. installing from setup, then uninstalling.
- **Report:** tick the I3, I4 and I5 rows of the checklist, and write `docs/acceptance/I5.md`.

---

## Not in I5
- **I6:** the CLI and `httporter.com`, scheduled publish, the watch folder, creating zones and buckets, zone and bucket
  settings.
- **Deviations, with reasons:**
  - **GPU renderer setting (§6, §9.1):** added back as an opt-in Settings › Advanced choice (software stays the default for S7 memory); applies after a restart, falls back to software if the GPU renderer can't start.
  - **Text scaling:** applied live via `WM_SETTINGCHANGE`. Per-element reflow beyond the Theme sizes is best effort.
  - **Translations:** the code is ready for them, but no languages ship in 1.0.
- **User actions at release:**
  - choose the public repository, then set `HTTPORTER_RELEASE_REPO` in CI;
  - get a signing certificate (SignPath Foundation or Azure Trusted Signing) and set its CI secrets;
  - make the repository public.
