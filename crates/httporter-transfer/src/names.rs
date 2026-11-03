//! File names: another name for a copy kept beside an existing file, and names Windows can store.

use std::collections::HashSet;

/// `"photo.jpg"` → `("photo", ".jpg")`, `"archive.tar.gz"` → `("archive.tar", ".gz")`. A name
/// whose only dot leads it (`".htaccess"`) or that has none has no extension.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

/// `name` with ` (n)` before its extension: `"photo.jpg"`, 2 → `"photo (2).jpg"`.
fn numbered(name: &str, n: u64) -> String {
    let (stem, extension) = split_extension(name);
    format!("{stem} ({n}){extension}")
}

/// The first of `name (from)`, `name (from + 1)`, … that `taken` doesn't hold.
pub(crate) fn first_free(name: &str, from: u64, taken: impl Fn(&str) -> bool) -> String {
    let mut n = from;
    loop {
        let candidate = numbered(name, n);
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// The name for a file kept beside an existing one of the same name: `"photo.jpg"` →
/// `"photo (1).jpg"`, `".htaccess"` → `".htaccess (1)"`, counting up past every name in `taken`.
pub fn keep_both_name(name: &str, taken: &HashSet<String>) -> String {
    first_free(name, 1, |candidate| taken.contains(candidate))
}

/// Characters Windows doesn't allow in a name, besides control characters.
const FORBIDDEN: &str = r#"<>:"/\|?*"#;

/// A name Windows can store for a file called `name` elsewhere: forbidden and control
/// characters become `_`, trailing dots and spaces go, and a device name (`CON`, `aux`,
/// `LPT1.log`, …) gets a leading `_`. A name with nothing left is `_`.
pub fn safe_local_name(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|c| {
            if c < ' ' || FORBIDDEN.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let kept = safe.trim_end_matches(['.', ' ']).len();
    safe.truncate(kept);
    if safe.is_empty() {
        return "_".to_owned();
    }
    if is_device_name(&safe) {
        safe.insert(0, '_');
    }
    safe
}

/// Windows reserves these names for devices, whatever the case and whatever follows the first
/// dot: `CON`, `PRN`, `AUX`, `NUL`, and `COM` or `LPT` with one digit 1–9 (or ¹ ² ³).
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let Some(digit) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    let mut chars = digit.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('1'..='9' | '¹' | '²' | '³'), None)
    )
}
