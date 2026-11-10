// SPDX-License-Identifier: MIT OR Apache-2.0

//! Syntax highlighting for Quick Look: a small hand-written tokenizer per language, run line by
//! line with its state carried from one line to the next (a comment, a template string or a
//! `<script>` that goes on).
//!
//! - html / xml / svg: tags, attributes, strings and comments; `<script>` and `<style>` as js and
//!   css;
//! - css: selectors, properties, strings, comments, numbers, at-rules;
//! - js / json: keywords, strings, templates, comments, numbers (json keys as attributes);
//! - md: headings, code spans and fences, links, quotes and list marks.
//!
//! It colours text for reading, nothing more: unusual constructs come out plain or close enough.

/// The styles of a span, as `CodeSpan.style` says them.
pub const PLAIN: i32 = 0;
pub const TAG: i32 = 1;
pub const ATTR: i32 = 2;
pub const STRING: i32 = 3;
pub const COMMENT: i32 = 4;
pub const KEYWORD: i32 = 5;
pub const NUMBER: i32 = 6;
pub const PUNCT: i32 = 7;

/// A line longer than this many characters is cut (minified files), with an ellipsis.
pub const MAX_LINE_CHARS: usize = 1_000;

/// At most this many spans a line; the rest of the line is one plain span.
const MAX_SPANS: usize = 240;

/// A run of text in one style.
pub type Span = (String, i32);

/// The languages it knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// html, htm, xml, svg.
    Markup,
    Css,
    Js,
    Json,
    Markdown,
    Plain,
}

/// The language of a file called `name`, by its extension.
pub fn lang_for(name: &str) -> Lang {
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" | "xhtml" | "xml" | "svg" | "rss" | "atom" | "xsl" | "vue" => Lang::Markup,
        "css" | "scss" | "less" => Lang::Css,
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => Lang::Js,
        "json" | "map" | "webmanifest" => Lang::Json,
        "md" | "markdown" => Lang::Markdown,
        _ => Lang::Plain,
    }
}

/// Colours the lines of one text in order.
#[derive(Debug, Clone)]
pub struct Highlighter {
    lang: Lang,
    markup: Markup,
    css: Css,
    js: Js,
    fence: bool,
}

impl Highlighter {
    pub fn new(lang: Lang) -> Self {
        Self {
            lang,
            markup: Markup::default(),
            css: Css::default(),
            js: Js::default(),
            fence: false,
        }
    }

    /// The spans of the next line (without its line break). Tabs become four spaces; a very long
    /// line is cut at [`MAX_LINE_CHARS`].
    pub fn line(&mut self, line: &str) -> Vec<Span> {
        let mut text = line.replace('\t', "    ");
        let cut = text
            .char_indices()
            .nth(MAX_LINE_CHARS)
            .map(|(at, _)| at)
            .filter(|_| text.len() > MAX_LINE_CHARS);
        if let Some(at) = cut {
            text.truncate(at);
        }
        let mut out = Out::default();
        match self.lang {
            Lang::Markup => markup_line(&mut self.markup, &text, &mut out),
            Lang::Css => css_line(&mut self.css, &text, &mut out),
            Lang::Js => js_line(&mut self.js, &text, &mut out, false),
            Lang::Json => js_line(&mut self.js, &text, &mut out, true),
            Lang::Markdown => md_line(&mut self.fence, &text, &mut out),
            Lang::Plain => out.push(&text, PLAIN),
        }
        if cut.is_some() {
            out.push("…", PLAIN);
        }
        out.spans
    }
}

/// The spans of a line, adjacent ones of a style merged.
#[derive(Default)]
struct Out {
    spans: Vec<Span>,
}

impl Out {
    fn push(&mut self, text: &str, style: i32) {
        if text.is_empty() {
            return;
        }
        // Spaces take the style of what they follow, so a run is not broken by them.
        let style = if text.bytes().all(|b| b == b' ') {
            self.spans.last().map_or(PLAIN, |s| s.1)
        } else {
            style
        };
        let full = self.spans.len() >= MAX_SPANS;
        match self.spans.last_mut() {
            Some(last) if last.1 == style || full => last.0.push_str(text),
            _ => self.spans.push((text.to_owned(), style)),
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_ident(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

/// Where the run of bytes from `from` for which `keep` holds ends.
fn run(s: &[u8], from: usize, keep: impl Fn(u8) -> bool) -> usize {
    s[from..]
        .iter()
        .position(|&b| !keep(b))
        .map_or(s.len(), |n| from + n)
}

/// Where a string opened by the quote at `from` ends (just past its closing quote), and
/// whether it closed on this line. A backslash escapes the next character.
fn string_end(s: &[u8], from: usize, quote: u8) -> (usize, bool) {
    let mut i = from + 1;
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            b if b == quote => return (i + 1, true),
            _ => i += 1,
        }
    }
    (s.len(), false)
}

/// Finds `needle` (ASCII, any case) in `s` from `from`.
fn find_ci(s: &str, from: usize, needle: &str) -> Option<usize> {
    let hay = s.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || hay.len() < n.len() {
        return None;
    }
    (from..=hay.len() - n.len()).find(|&i| hay[i..i + n.len()].eq_ignore_ascii_case(n))
}

// ---- js / json ----

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Js {
    /// Inside a `/* … */` comment.
    comment: bool,
    /// Inside a `` `…` `` template string.
    template: bool,
}

const JS_KEYWORDS: [&str; 46] = [
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "NaN",
];

fn js_line(st: &mut Js, line: &str, out: &mut Out, json: bool) {
    let s = line.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if st.comment {
            match line[i..].find("*/") {
                Some(n) => {
                    out.push(&line[i..i + n + 2], COMMENT);
                    i += n + 2;
                    st.comment = false;
                }
                None => {
                    out.push(&line[i..], COMMENT);
                    return;
                }
            }
            continue;
        }
        if st.template {
            // Only at a line's start: a template that ends on its own line clears the flag.
            let (n, closed) = template_rest(&s[i..]);
            out.push(&line[i..i + n], STRING);
            i += n;
            st.template = !closed;
            continue;
        }
        let b = s[i];
        let next = s.get(i + 1).copied();
        if b == b'/' && next == Some(b'/') {
            out.push(&line[i..], COMMENT);
            return;
        }
        if b == b'/' && next == Some(b'*') {
            out.push("/*", COMMENT);
            i += 2;
            st.comment = true;
            continue;
        }
        if b == b'"' || b == b'\'' {
            let (end, _) = string_end(s, i, b);
            // A JSON key: a string followed by a colon.
            let after = run(s, end, |c| c == b' ');
            let style = if json && s.get(after) == Some(&b':') {
                ATTR
            } else {
                STRING
            };
            out.push(&line[i..end], style);
            i = end;
            continue;
        }
        if b == b'`' {
            let (end, closed) = string_end(s, i, b'`');
            out.push(&line[i..end], STRING);
            i = end;
            st.template = !closed;
            continue;
        }
        if b.is_ascii_digit() || (b == b'.' && next.is_some_and(|c| c.is_ascii_digit())) {
            let mut end = run(s, i, |c| {
                c.is_ascii_alphanumeric() || c == b'.' || c == b'_'
            });
            // An exponent's sign: 1e-6.
            while end < s.len()
                && (s[end] == b'-' || s[end] == b'+')
                && matches!(s[end - 1], b'e' | b'E')
            {
                end = run(s, end + 1, |c| c.is_ascii_alphanumeric() || c == b'.');
            }
            out.push(&line[i..end], NUMBER);
            i = end;
            continue;
        }
        if is_ident_start(b) {
            let end = run(s, i, is_ident);
            let word = &line[i..end];
            let keyword = if json {
                matches!(word, "true" | "false" | "null")
            } else {
                JS_KEYWORDS.contains(&word)
            };
            out.push(word, if keyword { KEYWORD } else { PLAIN });
            i = end;
            continue;
        }
        if b == b' ' {
            let end = run(s, i, |c| c == b' ');
            out.push(&line[i..end], PLAIN);
            i = end;
            continue;
        }
        let end = i + 1;
        out.push(&line[i..end], PUNCT);
        i = end;
    }
}

/// A line that starts inside a template string: where the template ends (just past its
/// backtick), and whether it does on this line.
fn template_rest(s: &[u8]) -> (usize, bool) {
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            b'`' => return (i + 1, true),
            _ => i += 1,
        }
    }
    (s.len(), false)
}

// ---- css ----

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Css {
    comment: bool,
    /// How many `{` are open: 0 is where selectors are.
    depth: u32,
    /// After a property's `:`, until its `;` or `}`.
    value: bool,
}

fn css_line(st: &mut Css, line: &str, out: &mut Out) {
    let s = line.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if st.comment {
            match line[i..].find("*/") {
                Some(n) => {
                    out.push(&line[i..i + n + 2], COMMENT);
                    i += n + 2;
                    st.comment = false;
                }
                None => {
                    out.push(&line[i..], COMMENT);
                    return;
                }
            }
            continue;
        }
        let b = s[i];
        let next = s.get(i + 1).copied();
        let in_value = st.depth > 0 && st.value;
        let end = match b {
            b'/' if next == Some(b'*') => {
                out.push("/*", COMMENT);
                st.comment = true;
                i += 2;
                continue;
            }
            b'"' | b'\'' => {
                let (end, _) = string_end(s, i, b);
                out.push(&line[i..end], STRING);
                end
            }
            b'{' => {
                st.depth += 1;
                st.value = false;
                out.push("{", PUNCT);
                i + 1
            }
            b'}' => {
                st.depth = st.depth.saturating_sub(1);
                st.value = false;
                out.push("}", PUNCT);
                i + 1
            }
            b';' => {
                st.value = false;
                out.push(";", PUNCT);
                i + 1
            }
            b':' if st.depth > 0 && !st.value => {
                st.value = true;
                out.push(":", PUNCT);
                i + 1
            }
            b'@' => {
                let end = run(s, i + 1, |c| is_ident(c) || c == b'-');
                out.push(&line[i..end], KEYWORD);
                end
            }
            b'!' if in_value => {
                let end = run(s, i + 1, is_ident);
                out.push(&line[i..end], KEYWORD);
                end
            }
            b'#' if in_value => {
                let end = run(s, i + 1, |c| c.is_ascii_hexdigit());
                out.push(&line[i..end], NUMBER);
                end
            }
            _ if in_value
                && (b.is_ascii_digit()
                    || ((b == b'.' || b == b'-') && next.is_some_and(|c| c.is_ascii_digit()))) =>
            {
                let end = run(s, i + 1, |c| {
                    c.is_ascii_alphanumeric() || c == b'.' || c == b'%'
                });
                out.push(&line[i..end], NUMBER);
                end
            }
            b' ' => {
                let end = run(s, i, |c| c == b' ');
                out.push(&line[i..end], PLAIN);
                end
            }
            _ if is_ident(b) || b == b'-' || (st.depth == 0 && matches!(b, b'.' | b'#' | b':')) => {
                let end = run(s, i + 1, |c| is_ident(c) || c == b'-');
                let style = if st.depth == 0 {
                    TAG
                } else if st.value {
                    PLAIN
                } else {
                    ATTR
                };
                out.push(&line[i..end], style);
                end
            }
            _ => {
                out.push(&line[i..i + 1], PUNCT);
                i + 1
            }
        };
        i = end;
    }
}

// ---- html / xml / svg ----

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Within {
    #[default]
    Text,
    /// Inside a tag, after its name.
    Tag,
    /// Inside a quoted attribute value that went on past a line's end.
    Value(u8),
    Comment,
    /// `<![CDATA[ … ]]>`.
    Cdata,
    Script,
    Style,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Markup {
    within: Within,
    /// The open tag is `<script` or `<style` (not a closing one): its `>` starts js or css.
    opens: Option<Within>,
    js: Js,
    css: Css,
}

fn markup_line(st: &mut Markup, line: &str, out: &mut Out) {
    let s = line.as_bytes();
    let mut i = 0;
    while i < s.len() {
        match st.within {
            Within::Comment | Within::Cdata => {
                let close = if st.within == Within::Comment {
                    "-->"
                } else {
                    "]]>"
                };
                match line[i..].find(close) {
                    Some(n) => {
                        let end = i + n + close.len();
                        out.push(&line[i..end], COMMENT);
                        st.within = Within::Text;
                        i = end;
                    }
                    None => {
                        out.push(&line[i..], COMMENT);
                        return;
                    }
                }
            }
            Within::Script | Within::Style => {
                let close = if st.within == Within::Script {
                    "</script"
                } else {
                    "</style"
                };
                let end = find_ci(line, i, close).unwrap_or(s.len());
                if st.within == Within::Script {
                    js_line(&mut st.js, &line[i..end], out, false);
                } else {
                    css_line(&mut st.css, &line[i..end], out);
                }
                if end < s.len() {
                    st.within = Within::Text;
                }
                i = end;
            }
            Within::Value(quote) => match line[i..].find(quote as char) {
                Some(n) => {
                    out.push(&line[i..i + n + 1], STRING);
                    st.within = Within::Tag;
                    i += n + 1;
                }
                None => {
                    out.push(&line[i..], STRING);
                    return;
                }
            },
            Within::Text => {
                let Some(n) = line[i..].find('<') else {
                    out.push(&line[i..], PLAIN);
                    return;
                };
                out.push(&line[i..i + n], PLAIN);
                i += n;
                let rest = &line[i..];
                if rest.starts_with("<!--") {
                    out.push("<!--", COMMENT);
                    st.within = Within::Comment;
                    i += 4;
                    continue;
                }
                if rest.starts_with("<![CDATA[") {
                    out.push("<![CDATA[", COMMENT);
                    st.within = Within::Cdata;
                    i += 9;
                    continue;
                }
                let after = s.get(i + 1).copied();
                let opens_tag = after
                    .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'));
                if !opens_tag {
                    out.push("<", PLAIN);
                    i += 1;
                    continue;
                }
                // `<!DOCTYPE html>` and the like: one tag-coloured run.
                if after == Some(b'!') {
                    match line[i..].find('>') {
                        Some(n) => {
                            out.push(&line[i..=i + n], TAG);
                            i += n + 1;
                        }
                        None => {
                            out.push(&line[i..], TAG);
                            st.opens = None;
                            st.within = Within::Tag;
                            return;
                        }
                    }
                    continue;
                }
                let name_from = i + 1 + usize::from(matches!(after, Some(b'/' | b'!' | b'?')));
                let end = run(s, name_from, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, b'-' | b':' | b'_' | b'.')
                });
                let name = &line[name_from..end];
                st.opens = if after == Some(b'/') {
                    None
                } else if name.eq_ignore_ascii_case("script") {
                    Some(Within::Script)
                } else if name.eq_ignore_ascii_case("style") {
                    Some(Within::Style)
                } else {
                    None
                };
                out.push(&line[i..end], TAG);
                st.within = Within::Tag;
                i = end;
            }
            Within::Tag => {
                let b = s[i];
                match b {
                    b'>' => {
                        out.push(">", TAG);
                        i += 1;
                        st.within = st.opens.take().unwrap_or(Within::Text);
                        if st.within == Within::Script {
                            st.js = Js::default();
                        } else if st.within == Within::Style {
                            st.css = Css::default();
                        }
                    }
                    b'/' | b'?' if s.get(i + 1) == Some(&b'>') => {
                        out.push(&line[i..i + 2], TAG);
                        i += 2;
                        st.opens = None;
                        st.within = Within::Text;
                    }
                    b'"' | b'\'' => {
                        let (end, closed) = string_end(s, i, b);
                        out.push(&line[i..end], STRING);
                        if !closed {
                            st.within = Within::Value(b);
                        }
                        i = end;
                    }
                    b'=' => {
                        out.push("=", PLAIN);
                        i += 1;
                    }
                    b' ' => {
                        let end = run(s, i, |c| c == b' ');
                        out.push(&line[i..end], PLAIN);
                        i = end;
                    }
                    _ => {
                        let end = run(s, i, |c| {
                            !matches!(c, b' ' | b'=' | b'>' | b'"' | b'\'' | b'/' | b'?')
                        })
                        .max(i + 1);
                        out.push(&line[i..end], ATTR);
                        i = end;
                    }
                }
            }
        }
    }
}

// ---- md ----

fn md_line(fence: &mut bool, line: &str, out: &mut Out) {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        *fence = !*fence;
        out.push(line, STRING);
        return;
    }
    if *fence {
        out.push(line, STRING);
        return;
    }
    if trimmed.starts_with('#') {
        out.push(line, KEYWORD);
        return;
    }
    if trimmed.starts_with('>') {
        out.push(line, COMMENT);
        return;
    }
    let lead = line.len() - trimmed.len();
    let mut i = 0;
    let marker = ["- ", "* ", "+ "]
        .iter()
        .find(|m| trimmed.starts_with(**m))
        .map(|m| m.len())
        .or_else(|| {
            let digits = run(trimmed.as_bytes(), 0, |c| c.is_ascii_digit());
            (digits > 0 && trimmed[digits..].starts_with(". ")).then_some(digits + 2)
        });
    if let Some(len) = marker {
        out.push(&line[..lead], PLAIN);
        out.push(&line[lead..lead + len], PUNCT);
        i = lead + len;
    }
    let s = line.as_bytes();
    while i < s.len() {
        match s[i] {
            b'`' => {
                let end = line[i + 1..].find('`').map_or(s.len(), |n| i + n + 2);
                out.push(&line[i..end], STRING);
                i = end;
            }
            b'[' => {
                let close = line[i..].find("](").map(|n| i + n);
                let end = close.and_then(|c| line[c..].find(')').map(|n| c + n + 1));
                match (close, end) {
                    (Some(close), Some(end)) => {
                        out.push(&line[i..=close], ATTR);
                        out.push(&line[close + 1..end], STRING);
                        i = end;
                    }
                    _ => {
                        out.push("[", PLAIN);
                        i += 1;
                    }
                }
            }
            _ => {
                let end = run(s, i, |c| c != b'`' && c != b'[');
                out.push(&line[i..end], PLAIN);
                i = end;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spans of `lines`, coloured in order as one text in `lang`. Every line's spans give the
    /// line back (tabs as four spaces).
    fn colour(lang: Lang, lines: &[&str]) -> Vec<Vec<Span>> {
        let mut h = Highlighter::new(lang);
        lines
            .iter()
            .map(|line| {
                let spans = h.line(line);
                let text: String = spans.iter().map(|(t, _)| t.as_str()).collect();
                if line.chars().count() <= MAX_LINE_CHARS {
                    assert_eq!(text, line.replace('\t', "    "), "{line:?} lost text");
                }
                spans
            })
            .collect()
    }

    /// The style of the first span that holds `token`.
    fn style(spans: &[Span], token: &str) -> i32 {
        spans
            .iter()
            .find(|(text, _)| text.contains(token))
            .unwrap_or_else(|| panic!("no span holds {token:?}: {spans:?}"))
            .1
    }

    #[test]
    fn the_language_comes_from_the_extension() {
        for (name, lang) in [
            ("index.HTML", Lang::Markup),
            ("logo.svg", Lang::Markup),
            ("feed.xml", Lang::Markup),
            ("site.scss", Lang::Css),
            ("app.tsx", Lang::Js),
            ("main.mjs", Lang::Js),
            ("site.webmanifest", Lang::Json),
            ("data.json", Lang::Json),
            ("README.md", Lang::Markdown),
            ("notes.txt", Lang::Plain),
            ("Makefile", Lang::Plain),
        ] {
            assert_eq!(lang_for(name), lang, "{name}");
        }
    }

    #[test]
    fn js_keywords_strings_numbers_and_comments() {
        let lines = colour(
            Lang::Js,
            &[
                "const n = 42; // why",
                "let s = 'it\\'s', e = 1e-6;",
                "x.y(\"a\")",
            ],
        );
        assert_eq!(style(&lines[0], "const"), KEYWORD);
        assert_eq!(style(&lines[0], "n "), PLAIN);
        assert_eq!(style(&lines[0], "="), PUNCT);
        assert_eq!(style(&lines[0], "42"), NUMBER);
        assert_eq!(style(&lines[0], "// why"), COMMENT);
        assert_eq!(style(&lines[1], "'it\\'s'"), STRING);
        assert_eq!(style(&lines[1], "1e-6"), NUMBER);
        assert_eq!(style(&lines[2], "\"a\""), STRING);
    }

    #[test]
    fn js_comments_and_templates_go_on_to_the_next_line() {
        let lines = colour(
            Lang::Js,
            &["/* one", "two */ let a = `x", "y` + 1", "return"],
        );
        assert_eq!(lines[0], [("/* one".to_owned(), COMMENT)]);
        assert_eq!(style(&lines[1], "two */"), COMMENT);
        assert_eq!(style(&lines[1], "let"), KEYWORD);
        assert_eq!(style(&lines[1], "`x"), STRING);
        assert_eq!(style(&lines[2], "y`"), STRING);
        assert_eq!(style(&lines[2], "1"), NUMBER);
        assert_eq!(style(&lines[3], "return"), KEYWORD);
    }

    #[test]
    fn json_keys_are_attributes_and_only_its_literals_are_keywords() {
        let lines = colour(
            Lang::Json,
            &[r#"{"key" : "value", "n": 3, "ok": true, "if": null}"#],
        );
        let spans = &lines[0];
        assert_eq!(style(spans, "\"key\""), ATTR);
        assert_eq!(style(spans, "\"value\""), STRING);
        assert_eq!(style(spans, "3"), NUMBER);
        assert_eq!(style(spans, "true"), KEYWORD);
        assert_eq!(style(spans, "null"), KEYWORD);
        let lines = colour(Lang::Json, &["[const]"]);
        assert_eq!(style(&lines[0], "const"), PLAIN, "not a JSON keyword");
    }

    #[test]
    fn css_selectors_properties_and_values() {
        let lines = colour(
            Lang::Css,
            &[
                ".btn:hover, a { color: #FFF !important; margin: -2px 1.5em; }",
                "@media screen /* wide",
                "still */ {",
                "  p { content: \"x\" }",
                "}",
            ],
        );
        let rule = &lines[0];
        assert_eq!(style(rule, ".btn"), TAG);
        assert_eq!(style(rule, ":hover"), TAG);
        assert_eq!(style(rule, "color"), ATTR);
        assert_eq!(style(rule, "#FFF"), NUMBER);
        assert_eq!(style(rule, "!important"), KEYWORD);
        assert_eq!(style(rule, "-2px"), NUMBER);
        assert_eq!(style(rule, "1.5em"), NUMBER);
        assert_eq!(style(&lines[1], "@media"), KEYWORD);
        assert_eq!(style(&lines[1], "/* wide"), COMMENT);
        assert_eq!(style(&lines[2], "still */"), COMMENT);
        // Inside the at-rule's block, `p` is at depth 1.
        assert_eq!(style(&lines[3], "content"), ATTR);
        assert_eq!(style(&lines[3], "\"x\""), STRING);
    }

    #[test]
    fn markup_tags_attributes_comments_and_embedded_code() {
        let lines = colour(
            Lang::Markup,
            &[
                "<!DOCTYPE html><a href=\"/x\" data-n='1'>Hi</a> <!-- one",
                "two --><img alt=\"a",
                "b\"/> <script>let a = 1;</script><STYLE>p { color: red }</STYLE>",
                "<![CDATA[ x ]]> 1 < 2",
            ],
        );
        let first = &lines[0];
        assert_eq!(style(first, "<!DOCTYPE html>"), TAG);
        assert_eq!(style(first, "<a"), TAG);
        assert_eq!(style(first, "href"), ATTR);
        assert_eq!(style(first, "\"/x\""), STRING);
        assert_eq!(style(first, "'1'"), STRING);
        assert_eq!(style(first, "Hi"), PLAIN);
        assert_eq!(style(first, "</a"), TAG);
        assert_eq!(style(first, "<!-- one"), COMMENT);
        assert_eq!(style(&lines[1], "two -->"), COMMENT);
        assert_eq!(style(&lines[1], "\"a"), STRING);
        let third = &lines[2];
        assert_eq!(style(third, "b\""), STRING);
        assert_eq!(style(third, "/>"), TAG);
        assert_eq!(style(third, "let"), KEYWORD);
        assert_eq!(style(third, "1"), NUMBER);
        assert_eq!(style(third, "</script"), TAG);
        assert_eq!(style(third, "color"), ATTR);
        assert_eq!(style(third, "</STYLE"), TAG);
        assert_eq!(style(&lines[3], "<![CDATA[ x ]]>"), COMMENT);
        // A `<` that opens no tag is text.
        assert_eq!(style(&lines[3], "< 2"), PLAIN);
    }

    #[test]
    fn markdown_headings_quotes_lists_code_and_links() {
        let lines = colour(
            Lang::Markdown,
            &[
                "# Title",
                "> quoted",
                "- item with `code` and [a link](https://example.com).",
                "12. twelfth",
                "```rust",
                "# not a heading",
                "```",
                "after",
            ],
        );
        assert_eq!(lines[0], [("# Title".to_owned(), KEYWORD)]);
        assert_eq!(lines[1], [("> quoted".to_owned(), COMMENT)]);
        assert_eq!(style(&lines[2], "- "), PUNCT);
        assert_eq!(style(&lines[2], "`code`"), STRING);
        assert_eq!(style(&lines[2], "[a link]"), ATTR);
        assert_eq!(style(&lines[2], "(https://example.com)"), STRING);
        assert_eq!(style(&lines[3], "12. "), PUNCT);
        for fenced in &lines[4..7] {
            assert_eq!(fenced.len(), 1);
            assert_eq!(fenced[0].1, STRING);
        }
        assert_eq!(lines[7], [("after".to_owned(), PLAIN)]);
    }

    #[test]
    fn long_lines_are_cut_and_tabs_widened() {
        let mut h = Highlighter::new(Lang::Plain);
        let long = "é".repeat(MAX_LINE_CHARS + 500);
        let spans = h.line(&long);
        let text: String = spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 1);
        assert!(text.ends_with('…'));
        assert_eq!(h.line("\tx"), [("    x".to_owned(), PLAIN)]);
        // A line of many tokens has at most so many spans, and keeps its text.
        let busy = "a,".repeat(400);
        let spans = colour(Lang::Js, &[busy.as_str()]).remove(0);
        assert!(spans.len() <= MAX_SPANS);
    }

    #[test]
    fn text_beyond_ascii_never_splits_a_character() {
        for lang in [
            Lang::Markup,
            Lang::Css,
            Lang::Js,
            Lang::Json,
            Lang::Markdown,
        ] {
            colour(
                lang,
                &[
                    "é = '€\\é'; // ü",
                    "<p title=\"日本\">é€</p> ¿<é",
                    ".é { content: \"ß\"; x: é#é; } `é",
                    "[é](ü) `ñ` # ö",
                ],
            );
        }
    }
}
