# Translations

HTTPorter 1.0 ships in English only. The code is ready for other languages: this folder is
where they go.

## Where the words are

- **The window (`ui/**/*.slint`)**: every word a person reads or a screen reader speaks goes
  through `@tr("…")`. `src/lint_tests.rs` fails the tests when one does not.
- **Words that Rust builds** (status lines, errors, counts, sizes, dates): only in the `copy.rs`
  files (`src/copy.rs`, and one in each of `src/manage`, `src/publish`, `src/window`,
  `src/comfort` and `src/system`). The same lint test fails when a string is written elsewhere.
  These are not translated by the steps below yet: see "Rust's words" at the end.

## Adding a language to the window (Slint bundled translations)

Slint compiles the translations into the exe, so nothing is installed beside it, and at start it
picks the language that matches Windows' display language (`de-DE`, else `de`), or English when
there is none.

1. **Extract the strings** into a template, from the crate folder (`crates/httporter-app`):

   ```powershell
   cargo install slint-tr-extractor
   slint-tr-extractor (Get-ChildItem ui -Recurse -Filter *.slint).FullName `
       -o translations/httporter-app.pot
   ```

   Do it again whenever `@tr()` strings change; commit the `.pot`.

2. **Start the language** from the template, named by its language code (`de`, `pt-BR`, …):

   ```powershell
   New-Item -ItemType Directory -Force translations/de/LC_MESSAGES
   msginit --input=translations/httporter-app.pot --locale=de `
       --output=translations/de/LC_MESSAGES/httporter-app.po
   ```

   The file name must be the crate's name, `httporter-app.po`. Translate it with any `.po`
   editor (Poedit, Lokalize) or by hand. Keep every `{}` placeholder, in the order the sentence
   needs: Slint fills them in order, or by position with `{0}`, `{1}`.

3. **Plurals**: a string written `@tr("{n} file" | "{n} files" % count)` in Slint gets the
   language's own plural forms (`msgstr[0]`, `msgstr[1]`, …) from the `Plural-Forms` header.

4. **Keep a language up to date** after a new template:

   ```powershell
   msgmerge --update translations/de/LC_MESSAGES/httporter-app.po translations/httporter-app.pot
   ```

   New strings come in untranslated (shown in English), changed ones marked `fuzzy`.

5. **Bundle them**: in `build.rs`, add the folder to the compiler configuration (once, when the
   first language arrives):

   ```rust
   config = config.with_bundled_translations("translations");
   ```

   Then build as usual. Each `<lang>/LC_MESSAGES/httporter-app.po` becomes a language of the exe.

6. **Check it**: switch Windows' display language (or call
   `slint::select_bundled_translation("de")` once after the main window is made, in a debug
   build), and look for text that overflows: some languages run 30 % longer than English.
   Buttons, chips and rows grow with their text; fixed widths (the inspector's label column, the
   drawer's columns) cut long words with an ellipsis.

## Context for translators

- **Title case** is used for buttons, menu items and window titles ("Show Transfers", "Add
  Site…"); sentence case for everything else (Apple HIG, adapted to Windows). Follow the target
  language's own convention.
- An ellipsis `…` after a command means it asks for more before it acts.
- `“…”` quotes are the typographic ones; use the target language's quotation marks.
- Names of things on a site (files, folders, zones, buckets) are filled in at `{}` and are never
  translated.

## Rust's words

The `copy.rs` files hold every Rust-built word, each behind a function or a constant, so that a
later version can route them through a gettext catalog (for example with the `tr` crate's
`tr!` macro and the same `.po` files) without touching the rest of the code. Until then they are
English. Dates are written "Today 10:02", "Yesterday 17:02" and "12 Mar 2026" by
`copy::today_at`, `copy::yesterday_at` and `copy::day_month_year`; sizes use
`copy::SIZE_UNITS`.
