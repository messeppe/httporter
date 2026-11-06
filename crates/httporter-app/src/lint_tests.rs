// SPDX-License-Identifier: MIT OR Apache-2.0

//! Source-text lints. Two rules about where words may be written, checked by reading the
//! repository's own sources:
//!
//! - every user-facing string in the `.slint` files goes through `@tr()`;
//! - every user-facing string that Rust builds lives in `copy.rs`.
//!
//! These are text scans, not parsers, and say so. [`mask`] blanks comments and the insides of
//! string literals first, so a `//` in a string is not a comment, a `;` in a string does not end a
//! property, and a commented-out line is not code. Each rule is a function over one source text,
//! pinned by examples, and then run over the real files.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Slint,
}

/// `source` with every comment replaced by spaces and the inside of every string (and Rust char)
/// literal replaced by underscores. Quotes stay, newlines stay, and every character keeps its
/// byte length, so an offset in the result is an offset in `source`.
fn mask(source: &str, lang: Lang) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let blank = |out: &mut String, c: char, with: char| {
        if c == '\n' {
            out.push('\n');
        } else {
            out.extend(std::iter::repeat_n(with, c.len_utf8()));
        }
    };
    let at = |i: usize| chars.get(i).copied();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match (c, at(i + 1)) {
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    blank(&mut out, chars[i], ' ');
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                // Rust block comments nest; Slint's are treated alike, which only matters for a
                // comment that contains `/*`.
                let mut depth = 0;
                while i < chars.len() {
                    if (chars[i], at(i + 1)) == ('/', Some('*')) {
                        depth += 1;
                        blank(&mut out, '/', ' ');
                        blank(&mut out, '*', ' ');
                        i += 2;
                    } else if (chars[i], at(i + 1)) == ('*', Some('/')) {
                        depth -= 1;
                        blank(&mut out, '*', ' ');
                        blank(&mut out, '/', ' ');
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        blank(&mut out, chars[i], ' ');
                        i += 1;
                    }
                }
            }
            ('"', _) => {
                out.push('"');
                i += 1;
                while i < chars.len() {
                    match chars[i] {
                        '\\' => {
                            blank(&mut out, '\\', '_');
                            i += 1;
                            if let Some(escaped) = at(i) {
                                blank(&mut out, escaped, '_');
                                i += 1;
                            }
                        }
                        '"' => {
                            out.push('"');
                            i += 1;
                            break;
                        }
                        other => {
                            blank(&mut out, other, '_');
                            i += 1;
                        }
                    }
                }
            }
            ('r', Some('"' | '#'))
                if lang == Lang::Rust
                    && !i
                        .checked_sub(1)
                        .and_then(at)
                        .is_some_and(|p| p.is_alphanumeric() || p == '_') =>
            {
                // A raw string, `r"…"` or `r#"…"#`; `r#ident` is not one.
                let hashes = chars[i + 1..].iter().take_while(|c| **c == '#').count();
                if at(i + 1 + hashes) == Some('"') {
                    out.push('r');
                    out.extend(std::iter::repeat_n('#', hashes));
                    out.push('"');
                    i += hashes + 2;
                    while i < chars.len() {
                        let closes = chars[i] == '"'
                            && chars[i + 1..].iter().take_while(|c| **c == '#').count() >= hashes;
                        if closes {
                            out.push('"');
                            out.extend(std::iter::repeat_n('#', hashes));
                            i += hashes + 1;
                            break;
                        }
                        blank(&mut out, chars[i], '_');
                        i += 1;
                    }
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            ('\'', Some(next)) if lang == Lang::Rust => {
                // `'a'` and `'\n'` are characters (and may hold a quote); `'a` is a lifetime.
                let closes_at = if next == '\\' {
                    (i + 2..chars.len().min(i + 12)).find(|&j| chars[j] == '\'')
                } else {
                    (at(i + 2) == Some('\'')).then_some(i + 2)
                };
                match closes_at {
                    Some(end) => {
                        out.push('\'');
                        for &inner in &chars[i + 1..end] {
                            blank(&mut out, inner, '_');
                        }
                        out.push('\'');
                        i = end + 1;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The index just past the bracket that closes the one at `open` (any of `(`, `[`, `{`), in a
/// masked text; the end of the text if it never closes.
fn past_matching(masked: &str, open: usize) -> usize {
    let bytes = masked.as_bytes();
    let mut depth = 0;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_all<'a>(haystack: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    haystack.match_indices(needle).map(|(at, _)| at)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---- Rule 1: Slint text goes through @tr() ----

/// The properties that carry text a person reads or a screen reader speaks.
const TEXT_PROPS: [&str; 8] = [
    "text",
    "label",
    "title",
    "description",
    "accessible-label",
    "accessible-description",
    "accessible-placeholder-text",
    "placeholder-text",
];

/// Whether `value` (masked) has a string literal with something in it outside every `@tr(…)`.
/// An empty string is no text.
fn has_untranslated_literal(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut tr_depth = 0;
    let mut i = 0;
    while i < bytes.len() {
        if tr_depth == 0 && value[i..].starts_with("@tr(") {
            tr_depth = 1;
            i += 4;
            continue;
        }
        match bytes[i] {
            b'(' if tr_depth > 0 => tr_depth += 1,
            b')' if tr_depth > 0 => tr_depth -= 1,
            b'"' => {
                let close = value[i + 1..].find('"').map_or(bytes.len(), |j| i + 1 + j);
                if tr_depth == 0 && close > i + 1 {
                    return true;
                }
                i = close;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Every text property in `source` (Slint) that is given a string without `@tr()`, as
/// "line N: the property and its value". The property may start a line or follow a `{` on it
/// (`Text { text: "x"; }`), and its value runs to the `;` that ends it.
fn untranslated(source: &str) -> Vec<String> {
    let masked = mask(source, Lang::Slint);
    let bytes = masked.as_bytes();
    let mut offenders = Vec::new();
    for prop in TEXT_PROPS {
        for at in find_all(&masked, prop) {
            let after = at + prop.len();
            // Not the tail of a longer name (`accessible-label`, `event.text`) …
            if at > 0 && (is_ident_byte(bytes[at - 1]) || matches!(bytes[at - 1], b'-' | b'.')) {
                continue;
            }
            // … at the start of a statement (or after the `>` of `property <string>`) …
            let before = masked[..at].trim_end().bytes().last();
            if !matches!(before, None | Some(b'{' | b'}' | b';' | b'>')) {
                continue;
            }
            // … followed by a colon (`name := Text` names an element).
            let rest = masked[after..].trim_start();
            let Some(value) = rest.strip_prefix(':') else {
                continue;
            };
            if value.starts_with('=') {
                continue;
            }
            let start = masked.len() - value.len();
            let mut depth = 0;
            let mut end = masked.len();
            for (i, &b) in bytes.iter().enumerate().skip(start) {
                match b {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b'}' if depth == 0 => {
                        end = i;
                        break;
                    }
                    b'}' => depth -= 1,
                    b';' if depth == 0 => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
            if has_untranslated_literal(&masked[start..end]) {
                offenders.push(format!(
                    "line {}: {prop}: {}",
                    line_of(source, at),
                    collapse(&source[start..end])
                ));
            }
        }
    }
    offenders.sort();
    offenders
}

#[test]
fn tr_scanner_catches_one_line_and_description_forms() {
    // The one-line form, and the property that was not on the list.
    assert_eq!(
        untranslated("Text { text: \"x\"; }"),
        ["line 1: text: \"x\""]
    );
    assert_eq!(
        untranslated("Item {\n    description: \"y\";\n}"),
        ["line 2: description: \"y\""]
    );
    assert_eq!(
        untranslated("Button { label: \"a\"; accessible-label: \"b\"; }").len(),
        2
    );
    // A property of a property declaration, and one that goes on to the next line.
    assert_eq!(untranslated("in property <string> title: \"Hi\";").len(), 1);
    assert_eq!(
        untranslated("Text {\n    text: root.on\n        ? \"yes\"\n        : @tr(\"no\");\n}")
            .len(),
        1
    );

    // Translated, empty or not text at all.
    for clean in [
        "Text { text: @tr(\"x\"); }",
        "Item { description: @tr(\"y\"); }",
        "Text { text: @tr(\"{}, {}\", root.name, \"kind\"); }",
        "Text { text: root.on ? @tr(\"a\") : @tr(\"b\"); }",
        "Item { accessible-value: root.secret ? \"\" : self.text; }",
        "Text { text: \"\"; }",
        "if root.error != \"\": ErrorLine { text: root.error; }",
        "if event.text == \"r\" { root.go(); }",
        "text := Text { text: root.t; }",
        "in-out property <string> text <=> input.text;",
        "Image { source: @image-url(\"a.png\"); }",
    ] {
        assert_eq!(untranslated(clean), Vec::<String>::new(), "{clean}");
    }
}

#[test]
fn tr_scanner_ignores_comments_and_slashes_inside_strings() {
    // A `//` inside a string does not start a comment: the property after it is still read …
    assert_eq!(
        untranslated("Item { text: @tr(\"see http://x\"); label: \"c\"; }"),
        ["line 1: label: \"c\""]
    );
    assert_eq!(
        untranslated("Item { text: \"a // b\"; }"),
        ["line 1: text: \"a // b\""]
    );
    assert!(untranslated("Item { text: @tr(\"http://x\"); }").is_empty());
    // … and a `;` or `}` inside a string does not end the value.
    assert_eq!(
        untranslated("Item { text: \"a; b }\"; }"),
        ["line 1: text: \"a; b }\""]
    );
    // A comment is not code, nor is what it quotes.
    assert!(untranslated("Item { } // text: \"commented out\"").is_empty());
    assert!(untranslated("/* label: \"x\"; */ Item { }").is_empty());
    assert!(untranslated("// description: \"x\"\nItem { }").is_empty());
}

fn files_under(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == extension) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Every user-facing string in the .slint sources must go through @tr() so it can be
/// translated later.
#[test]
fn user_facing_strings_are_wrapped_in_tr() {
    let files = files_under(&Path::new(env!("CARGO_MANIFEST_DIR")).join("ui"), "slint");
    assert!(files.len() >= 10, "found only {files:?}");

    let mut offenders = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        for offender in untranslated(&source) {
            offenders.push(format!("{}: {offender}", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "strings without @tr():\n{}",
        offenders.join("\n")
    );
}

// ---- Rule 2: Rust-built text lives in copy.rs ----

/// The functions that make the app's words. They are defined in `copy.rs` and nowhere else.
const COPY_FUNCTIONS: [&str; 14] = [
    "items",
    "items_status",
    "describe",
    "describe_for",
    "describe_listing",
    "zone_not_found",
    "bucket_not_found",
    "describe_transfer_error",
    "drop_status",
    "selection_summary",
    "status_right",
    "local_not_found",
    "local_open_failed",
    "startup_error_text",
];

/// `masked` with every item marked `#[cfg(test)]` (a module, a function, a `mod x;` line) blanked.
fn without_test_items(masked: &str) -> String {
    let bytes = masked.as_bytes();
    let mut out = masked.to_owned().into_bytes();
    for start in find_all(masked, "#[cfg(test)]") {
        let mut at = start + "#[cfg(test)]".len();
        // Skip further attributes, then read the item up to its `;` or through its `{ … }`.
        loop {
            let skipped = masked[at..].trim_start();
            at = masked.len() - skipped.len();
            if skipped.starts_with("#[") {
                at = past_matching(masked, at + 1);
            } else {
                break;
            }
        }
        let mut end = masked.len();
        let mut parens = 0;
        for (i, &b) in bytes.iter().enumerate().skip(at) {
            match b {
                b'(' | b'[' => parens += 1,
                b')' | b']' => parens -= 1,
                b';' if parens == 0 => {
                    end = i + 1;
                    break;
                }
                b'{' if parens == 0 => {
                    end = past_matching(masked, i);
                    break;
                }
                _ => {}
            }
        }
        for byte in &mut out[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(out).unwrap()
}

/// The ways `source` (Rust, outside `copy.rs`, test code left out) writes words that belong in
/// `copy.rs`, as "line N: what":
///
/// - a string literal with something in it passed to a `set_*` call (`set_status_message`,
///   `set_error_text`, … : whatever a window shows), or a `format!` inside one;
/// - a field error of the Add Site form written out as a literal;
/// - a definition of one of [`COPY_FUNCTIONS`].
fn copy_offenders(source: &str) -> Vec<String> {
    let masked = without_test_items(&mask(source, Lang::Rust));
    let bytes = masked.as_bytes();
    let mut offenders = Vec::new();

    for at in find_all(&masked, ".set_") {
        let name_end = masked[at + 1..]
            .bytes()
            .position(|b| !is_ident_byte(b))
            .map_or(masked.len(), |n| at + 1 + n);
        if bytes.get(name_end) != Some(&b'(') {
            continue;
        }
        let name = &masked[at + 1..name_end];
        let args = &masked[name_end..past_matching(&masked, name_end)];
        if args.contains("format!") {
            offenders.push(format!(
                "line {}: format! feeding {name}",
                line_of(source, at)
            ));
            continue;
        }
        let mut quotes = args.match_indices('"').map(|(i, _)| i);
        while let (Some(open), Some(close)) = (quotes.next(), quotes.next()) {
            if close > open + 1 {
                offenders.push(format!(
                    "line {}: a string literal passed to {name}",
                    line_of(source, at)
                ));
                break;
            }
        }
    }

    for variant in ["Zone", "Region", "Password"] {
        for at in find_all(&masked, &format!("FieldError::{variant}(")) {
            let inside = &masked[at + "FieldError::".len() + variant.len() + 1..];
            if inside.trim_start().starts_with('"') {
                offenders.push(format!(
                    "line {}: the {variant} field error written out",
                    line_of(source, at)
                ));
            }
        }
    }

    for name in COPY_FUNCTIONS {
        for at in find_all(&masked, &format!("fn {name}(")) {
            if at == 0 || !is_ident_byte(bytes[at - 1]) {
                offenders.push(format!("line {}: fn {name} defined", line_of(source, at)));
            }
        }
    }

    offenders.sort();
    offenders
}

#[test]
fn copy_scanner_flags_words_written_outside_copy_rs() {
    let bad = r#"
fn show(ui: &AppWindow) {
    ui.set_status_message("Hello".into());
    ui.set_remote_status(format!("{} items", 3).into());
    ui.set_connection_text(SharedString::from("Not connected"));
    ui.set_add_site_result(
        "a // b".into(),
    );
}
fn items(n: usize) -> String { String::new() }
fn check() -> FieldError { FieldError::Zone("Enter it.".into()) }
"#;
    assert_eq!(
        copy_offenders(bad),
        [
            "line 10: fn items defined",
            "line 11: the Zone field error written out",
            "line 3: a string literal passed to set_status_message",
            "line 4: format! feeding set_remote_status",
            "line 5: a string literal passed to set_connection_text",
            "line 6: a string literal passed to set_add_site_result",
        ]
    );
}

#[test]
fn copy_scanner_leaves_alone_what_is_not_copy() {
    let fine = r##"
fn show(ui: &AppWindow, batch: &DropBatch) {
    ui.set_status_message(copy::drop_status(batch, None).into());
    ui.set_error_text(SharedString::new());
    ui.set_add_site_result("".into());
    ui.set_selected_site(3);
    ui.set_remote_chip(copy::chip(&host).into());
    // ui.set_status_message("commented out".into());
    let raw = r#"ui.set_status_message("inside a raw string")"#;
    let quote = '"'; ui.set_remote_selected(-1);
    let _ = FieldError::Zone(copy::ZONE_MISSING.into());
    let _ = SomeOtherError::Zone("not the form's".into());
}
fn not_items(n: usize) {}
#[cfg(test)]
mod tests {
    fn items() {}
    fn t(ui: &AppWindow) { ui.set_status_message("only in a test".into()); }
}
#[cfg(test)]
#[path = "x_tests.rs"]
mod more_tests;
"##;
    assert_eq!(copy_offenders(fine), Vec::<String>::new());
}

/// Every user-facing string that Rust builds lives in a `copy.rs`: the app's own, and one per
/// module folder (`manage`, `publish`, I5's `window`, `comfort` and `system`, and I6's `cli`).
/// A string literal passed to a `set_*` call, a `format!` feeding one, the field errors of the Add
/// Site form and the functions that make the app's words are looked for in every other source
/// file (tests and fakes excepted). This is a heuristic by design: it catches the usual ways a
/// string reaches the window, not every way.
#[test]
fn copy_strings_live_only_in_copy_rs() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = files_under(&src, "rs");
    assert!(files.contains(&src.join("copy.rs")), "copy.rs is missing");
    for folder in ["manage", "publish", "window", "comfort", "system", "cli"] {
        assert!(
            files.contains(&src.join(folder).join("copy.rs")),
            "{folder}/copy.rs is missing"
        );
    }

    let mut scanned = 0;
    let mut offenders = Vec::new();
    for file in &files {
        let name = file.file_name().unwrap().to_string_lossy();
        if name == "copy.rs" || name == "fakes.rs" || name.ends_with("_tests.rs") {
            continue;
        }
        scanned += 1;
        let source = std::fs::read_to_string(file).unwrap();
        for offender in copy_offenders(&source) {
            offenders.push(format!("{}: {offender}", file.display()));
        }
    }
    assert!(scanned >= 8, "scanned only {scanned} files");
    assert!(
        offenders.is_empty(),
        "words written outside copy.rs:\n{}",
        offenders.join("\n")
    );
}

// ---- Rule 1, for the context menus ----

/// The title of every `MenuItem` of the context menu declared as `{name} := ContextMenuArea { … }`
/// in `source` (Slint), in order, as written: `@tr("Download")` gives `Download`, and a title that
/// is not a translated string is given as it is, with "NOT TRANSLATED: " in front.
fn menu_titles(source: &str, name: &str) -> Vec<String> {
    let masked = mask(source, Lang::Slint);
    let start = masked
        .find(&format!("{name} := ContextMenuArea"))
        .unwrap_or_else(|| panic!("no context menu {name}"));
    let open = start + masked[start..].find('{').unwrap();
    let end = past_matching(&masked, open);
    let mut titles = Vec::new();
    for item in find_all(&masked[open..end], "MenuItem {") {
        let from = open + item;
        let after = &masked[from..end];
        let Some(title) = after.find("title:") else {
            continue;
        };
        let value_start = from + title + "title:".len();
        let value_end = value_start + masked[value_start..].find(';').unwrap();
        let value = source[value_start..value_end].trim();
        match value
            .strip_prefix("@tr(\"")
            .and_then(|rest| rest.strip_suffix("\")"))
        {
            Some(text) if !text.contains('"') => titles.push(text.to_owned()),
            _ => titles.push(format!("NOT TRANSLATED: {value}")),
        }
    }
    titles
}

#[test]
fn menu_title_scanner_reads_titles_and_flags_plain_strings() {
    let source = r#"
        Pane {
            // MenuItem { title: "commented out"; }
            first := ContextMenuArea {
                Menu {
                    MenuItem { title: @tr("Open"); activated => { root.open(); } }
                    MenuSeparator { }
                    MenuItem {
                        title: @tr("Save as…");
                        enabled: root.can-save;
                    }
                    MenuItem { title: "Plain"; }
                    MenuItem { title: root.name; }
                }
            }
            second := ContextMenuArea {
                Menu { MenuItem { title: @tr("Other"); } }
            }
        }
    "#;
    assert_eq!(
        menu_titles(source, "first"),
        [
            "Open",
            "Save as…",
            "NOT TRANSLATED: \"Plain\"",
            "NOT TRANSLATED: root.name"
        ]
    );
    assert_eq!(menu_titles(source, "second"), ["Other"]);
}

/// Task 17 and I4: the context menus carry the entries the brief lists, cloud rows: Quick Look,
/// Open, Download, Download to…, New Folder…, Rename…, Move To…, Delete…, Copy Public Link, Copy
/// Path, Copy Presigned Link… (where the site has them), Add to Favorites, Purge from CDN, Refresh;
/// This PC rows: Quick Look, Upload, New Folder…, Show in Explorer, Add to Favorites, Refresh, and
/// every title goes through `@tr()` (the headless test in `pane_tests.rs` opens the menus and reads
/// what they show).
#[test]
fn context_menu_items_are_the_listed_ones_and_all_go_through_tr() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/components/pane.slint");
    let source = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        menu_titles(&source, "cloud-menu"),
        [
            "Quick Look",
            "Open",
            "Download",
            "Download to…",
            "New Folder…",
            "Rename…",
            "Move To…",
            "Delete…",
            "Copy Public Link",
            "Copy Path",
            "Copy Presigned Link…",
            "Add to Favorites",
            "Purge from CDN",
            "Refresh"
        ]
    );
    assert_eq!(
        menu_titles(&source, "local-menu"),
        [
            "Quick Look",
            "Upload",
            "New Folder…",
            "Show in Explorer",
            "Add to Favorites",
            "Refresh"
        ]
    );
    // The sidebar's site menu is another context menu, and its titles go through `@tr()` too.
    let sidebar = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/components/sidebar.slint");
    let sidebar = std::fs::read_to_string(sidebar).unwrap();
    let site_menu = menu_titles(&sidebar, "site-menu");
    assert!(!site_menu.is_empty());
    assert!(
        site_menu.iter().all(|t| !t.starts_with("NOT TRANSLATED")),
        "{site_menu:?}"
    );
}
