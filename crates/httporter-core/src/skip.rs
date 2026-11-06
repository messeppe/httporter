// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skip rules: the files a publish or a compare leaves out.
//!
//! Paths are relative, with '/' between segments, and matched ignoring case. A rule ending in '/'
//! (`x/`) is a folder of that name anywhere, and everything below it; a rule without '/' matches
//! the last segment; a rule with an inner '/' matches the whole relative path. '*' is a run of
//! characters without '/', '?' one character. Whatever a rule matches as a folder takes
//! everything below it along.

/// The rules a new profile starts with (spec 8.6.11).
pub const DEFAULT_SKIP_RULES: &[&str] = &[
    ".env",
    ".env.*",
    "*.key",
    "*.pem",
    ".git/",
    ".svn/",
    "node_modules/",
    "Thumbs.db",
    "desktop.ini",
    ".DS_Store",
    "*.tmp",
    "~$*",
];

/// A set of skip rules, compiled. The default skips nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkipRules {
    rules: Vec<Rule>,
}

/// One rule, lowercase, split into what it matches against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    pattern: Vec<char>,
    /// Matched against the whole relative path (an inner '/'), not one segment.
    whole_path: bool,
    /// Matches folders only (a trailing '/').
    folder_only: bool,
}

impl SkipRules {
    /// Compiles `patterns`; blank lines are ignored, and a leading '/' ties a rule to the root.
    pub fn new(patterns: &[String]) -> Self {
        let rules = patterns
            .iter()
            .filter_map(|pattern| {
                let pattern = pattern.trim();
                let folder_only = pattern.ends_with('/');
                let anchored = pattern.starts_with('/');
                let body = pattern.trim_matches('/');
                if body.is_empty() {
                    return None;
                }
                Some(Rule {
                    pattern: body.to_lowercase().chars().collect(),
                    whole_path: anchored || body.contains('/'),
                    folder_only,
                })
            })
            .collect();
        Self { rules }
    }

    /// [`DEFAULT_SKIP_RULES`], as a profile keeps them.
    pub fn defaults() -> Vec<String> {
        DEFAULT_SKIP_RULES.iter().map(|r| (*r).to_owned()).collect()
    }

    /// Whether `rel` (a relative '/' path; `is_folder` for a folder) is left out: a rule matches
    /// it, or a folder it is in.
    pub fn is_skipped(&self, rel: &str, is_folder: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let rel = rel.to_lowercase();
        let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let Some(last) = segments.len().checked_sub(1) else {
            return false;
        };
        // Each segment with the path up to it, and whether it is a folder.
        let mut prefix: Vec<char> = Vec::with_capacity(rel.len());
        let mut steps = Vec::with_capacity(segments.len());
        for (i, segment) in segments.iter().enumerate() {
            if i > 0 {
                prefix.push('/');
            }
            let segment: Vec<char> = segment.chars().collect();
            prefix.extend(&segment);
            steps.push((segment, prefix.clone(), i < last || is_folder));
        }
        self.rules.iter().any(|rule| {
            steps.iter().any(|(segment, path, folder)| {
                (*folder || !rule.folder_only)
                    && glob(&rule.pattern, if rule.whole_path { path } else { segment })
            })
        })
    }
}

/// Whether `text` matches `pattern`, ignoring case: '*' is a run of characters without '/', '?'
/// one character other than '/'.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    glob(&pattern, &text)
}

/// Whether `text` matches `pattern`: '*' is a run of characters without '/', '?' one character
/// other than '/'.
fn glob(pattern: &[char], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    // The last '*' seen, and where in `text` its run ends so far.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some('?') if text[t] != '/' => {
                p += 1;
                t += 1;
            }
            Some(&c) if c != '*' && c != '?' && c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                // Let the last '*' take one more character, if it may.
                Some((sp, st)) if text[st] != '/' => {
                    star = Some((sp, st + 1));
                    p = sp + 1;
                    t = st + 1;
                }
                _ => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Whether `rel` looks like a file that holds a secret: `.env*`, `*.pem`, `*.key`, `id_rsa*`,
/// `*.p12`, `wp-config.php` (spec 8.6.12).
pub fn looks_secret(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_lowercase();
    name.starts_with(".env")
        || name.starts_with("id_rsa")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name == "wp-config.php"
}
