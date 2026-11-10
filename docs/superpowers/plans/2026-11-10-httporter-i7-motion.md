# HTTPorter I7: Motion

Mac-like motion, asked for by the user on 2026-11-10 ("Yes, build all"). Built from established practice,
not invented:

- **Apple HIG, Motion:** motion only for feedback and orientation; none on frequent actions; never make
  people wait; let them turn it off.
- **Springs as in SwiftUI** (`spring(duration:bounce:)`): Slint 1.18's built-in `easing: spring(bounce)`
  with the `animate` duration. Its natural frequency is `2π / duration`, the formula Flutter publishes as
  matching SwiftUI. A spring keeps its velocity when the target changes, so every animation can be
  interrupted.
- **Durations (Material desktop 150–200 ms, Fluent 83/167/250 ms):** the Theme tokens below.
- **Enter decelerates, exit accelerates and is shorter** (Fluent): exits use `Theme.motion-exit` with
  `ease-in`.
- **Cheap properties:** animate opacity, x/y, colours. Don't animate sizes that re-wrap text every frame.
- **Animations off in Windows:** every token is 1 ms (snaps). A spring needs a duration above zero.

## 0. Shared interfaces (T1, committed)

- `Theme.motion-hover` 100 ms, `motion-control` 250 ms, `motion-panel` 300 ms, `motion-sheet` 350 ms,
  `motion-exit` 200 ms; all 1 ms when `Theme.motion` is false. Use these, never literal durations.
- Springs: `easing: spring(0)` for sheets and panels, `spring(0.15)` for controls (switch, segments,
  Quick Look), never above 0.3.
- `ui/components/motion.slint`: `Spinner` (indeterminate, `color`) and `Chevron` (`open`, `color`), T1
  placeholders that track C draws. Rotation is drawn as a `Path` whose points come from the angle (the
  software renderer can't rotate).
- `transform-scale` on an element: drawn by the software renderer only after track A. Until then it is
  ignored, so tracks B and C may use it already.

## Tracks (each owns its files; nobody edits another's)

### A: the software renderer draws scaled elements (vendor/i-slint-renderer-software only)
- Slint's software renderer leaves `scale()` and `rotate()` as TODO (also on Slint master), and
  `SUPPORTS_TRANSFORMATIONS = false`.
- Implement **scale** the way compositors do: draw the scaled element and its children once into an
  offscreen buffer at full size, then draw that buffer scaled (with the existing smooth image scaling)
  around `transform-origin`, with the element's opacity. Mark patches `// HTTPorter patch`.
- Rotation stays unsupported (paths cover the chevron and spinner).
- A test in `crates/httporter-app/tests/` draws a Rectangle with text at `transform-scale: 0.9` off-screen
  and checks its size and centre in the frame, and that scale 1.0 draws exactly as before.

### B: sheets and overlays
Files: `sheets.slint`, `quicklook.slint`, `palette.slint`, `toast.slint`, `add_site.slint`,
`manage_sheets.slint`, `publish_sheet.slint`, `profile_sheet.slint`, `schedule_sheet.slint`,
`zone_sheets.slint`, `activity.slint`, the matching Rust only where a callback is needed.
- Every sheet and alert: the scrim fades in (`motion-sheet`); the sheet slides down 12 px and fades in
  with `transform-scale` 0.97 → 1 (`spring(0)`). Leaving: fades and rises out with `motion-exit`.
- Clicking outside an open alert or sheet (on the scrim): a small horizontal nudge (8 px, `spring(0.3)`).
- Add Site: a wrong password or key → the field shakes (three swings, 0.35 s), like the macOS login
  window.
- Quick Look: grows from 0.92 → 1 with a fade (`spring(0.15)`) and shrinks back on close.
- Command palette: slides 8 px down and fades in; the selection highlight moves to the new row
  (`motion-control`).
- Toasts: rise 12 px and fade in; fade out; the Undo toast shows a thin countdown bar.
- `Segmented` (in sheets.slint): the selected pill slides between options (`spring(0.15)`).

### C: panes and controls
Files: `motion.slint`, `pane.slint`, `sidebar.slint`, `drawer.slint`, `statusbar.slint`, `toolbar.slint`,
`search_field.slint`, `inspector.slint`, `tab_strip.slint`, `chips.slint`, `buttons.slint`,
`settings_tabs.slint`, `settings_window.slint`, `layout.slint`, `app.slint` (layout only), and the
matching Rust.
- `Spinner` and `Chevron` drawn; the pane's "Loading…" shows the spinner; folder chevrons turn.
- Sidebar show/hide and the queue drawer slide (`motion-panel`, `spring(0)`).
- Switch knobs spring (`spring(0.15)`); Settings tab highlight slides.
- Buttons and tool buttons: hover and press colour fade (`motion-hover`). No motion on row hover.
- Rows: a new folder row slides in; deleted rows fold away (opacity and height, `motion-panel`).
- Drop targets: the highlight fades in. A cancelled drag: the drag image slides back.
- Transfers: progress bars fill smoothly; a finished row's progress turns into a ✓ and the row fades out.
- Publish result: "changed"/"new" chips fade to "same".
- Thumbnails and the inspector preview fade in when they load (`motion-control`).
- Connection pill: the dot pulses while connecting.
- Tooltips (if the app has them): fade in after the delay.

## Rules for every track
- Build env: own worktree; `CARGO_TARGET_DIR=D:\Temp\httporter-target\i7<track>`, `TMP`/`TEMP`
  `D:\Temp\httporter-build`, `CARGO_BUILD_JOBS=2`, `HTTPORTER_DATA_DIR=D:\Temp\httporter-build\data-i7<track>`.
- Never drive the user's desktop. Never `cargo update`. No `panic = "abort"`.
- Strings: `@tr()`; Rust words only in `copy.rs`.
- Done: `cargo fmt --all`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, the app's
  tests pass (existing tests may need timer advances for new animations), one commit on the track branch.

## T10
- Off-screen frames of a sheet opening (at 0, 50, 100, 200, 350 ms) with mocked time; animations off
  → final frame at once.
- `tools/check.ps1`, package, install; the user tries it.
