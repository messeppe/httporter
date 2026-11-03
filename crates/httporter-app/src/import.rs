//! Reading what a user already has: the `BUNNY_*` lines of a `.env` file and the profiles of the
//! AWS CLI's `credentials` and `config` files. The Import button on the Welcome screen turns
//! what they hold into an Add Site form.
//!
//! Both parsers take the text of a file, never a path. Only [`read_env_file`] (the file the user
//! picked) and [`read_aws_profiles`] (the AWS CLI's own files) read from disk, and only when the
//! user asks to import; nothing they read is logged.
//!
//! The password and the secret access key pass through here and nowhere else: the `Debug` output
//! of both result types leaves them out, and no error quotes a line of the file.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::SiteDraft;
use crate::add_site::{bunny_region_index, tile_for};
use crate::copy;
use httporter_core::S3Preset;

/// The Bunny storage zone a `.env` file describes.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvImport {
    /// `BUNNY_ZONE`.
    pub zone: String,
    /// `BUNNY_KEY`: the zone's password.
    pub password: String,
    /// `BUNNY_REGION`, as written (a code such as `sg` or a city); empty when the file has none.
    pub region: String,
}

// Not derived: a derived `Debug` would print the password in a failed assertion or a log line.
impl fmt::Debug for EnvImport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvImport")
            .field("zone", &self.zone)
            .field("password", &"<hidden>")
            .field("region", &self.region)
            .finish()
    }
}

/// Reads `BUNNY_ZONE`, `BUNNY_KEY` and `BUNNY_REGION` from the text of a `.env` file.
///
/// Tolerated: a byte order mark, Windows line endings, `export ` in front, spaces around the
/// `=`, single or double quotes around the value, and a ` # comment` after an unquoted or quoted
/// value. A name that appears twice takes its last value. `BUNNY_LOCAL_DIR` and `BUNNY_PREFIX`
/// are ignored until publishing profiles exist (I3).
///
/// `BUNNY_KEY` is looked for first, so a file with neither line says the key is missing. The
/// error is a sentence for the user and never contains a value from the file.
pub fn parse_env(text: &str) -> Result<EnvImport, String> {
    let assignments: Vec<(&str, String)> = env_assignments(text).collect();
    let find = |name: &str| {
        assignments
            .iter()
            .rev()
            .find(|(found, _)| *found == name)
            .map(|(_, value)| value.as_str())
    };
    let required = |name: &str| match find(name) {
        None => Err(copy::env_missing(name)),
        Some("") => Err(copy::env_empty(name)),
        Some(value) => Ok(value.to_owned()),
    };
    let password = required("BUNNY_KEY")?;
    let zone = required("BUNNY_ZONE")?;
    Ok(EnvImport {
        zone,
        password,
        region: find("BUNNY_REGION").unwrap_or_default().to_owned(),
    })
}

/// The `NAME=value` lines of a `.env` text, in order, with their values unquoted.
fn env_assignments(text: &str) -> impl Iterator<Item = (&str, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let line = match line.strip_prefix("export") {
            Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
            _ => line,
        };
        let (name, value) = line.split_once('=')?;
        let name = name.trim();
        let is_name =
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        is_name.then(|| (name, unquote(value.trim())))
    })
}

/// A value without its quotes or its trailing comment.
fn unquote(value: &str) -> String {
    if let Some(quote) = value.chars().next().filter(|c| matches!(*c, '"' | '\'')) {
        let inside = &value[1..];
        // A value that ends with its quote is taken whole, so that a quote inside a password
        // survives; otherwise it ends at the next quote and what follows is a comment.
        return match inside.strip_suffix(quote) {
            Some(whole) => whole.to_owned(),
            None => inside.split(quote).next().unwrap_or_default().to_owned(),
        };
    }
    let end = value
        .char_indices()
        .find(|&(i, c)| c == '#' && value[..i].ends_with([' ', '\t']))
        .map_or(value.len(), |(i, _)| i);
    value[..end].trim_end().to_owned()
}

/// One profile of the AWS CLI's files that has an access key.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsProfile {
    /// The profile's name: `default`, or the name between the brackets.
    pub name: String,
    /// `aws_access_key_id`.
    pub key_id: String,
    /// `aws_secret_access_key`.
    pub secret: String,
    /// `region` from the `config` file's section for this profile.
    pub region: Option<String>,
}

// Not derived, for the same reason as `EnvImport`.
impl fmt::Debug for AwsProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsProfile")
            .field("name", &self.name)
            .field("key_id", &self.key_id)
            .field("secret", &"<hidden>")
            .field("region", &self.region)
            .finish()
    }
}

/// The profiles of the `credentials` file that have both an access key ID and a secret, in the
/// order the file lists them, each with the `region` its section of the `config` file gives.
///
/// The `credentials` file names a profile `[name]`; the `config` file names it `[profile name]`
/// (`[default]` in both). A profile that `config` describes but `credentials` does not (single
/// sign-on, a role) has no key to import and is left out. A session token is not read: it expires.
pub fn parse_aws_profiles(credentials: &str, config: &str) -> Vec<AwsProfile> {
    let regions: HashMap<String, String> = ini_sections(config)
        .into_iter()
        .filter_map(|(section, mut keys)| {
            let name = match section.strip_prefix("profile") {
                Some(rest) if rest.starts_with([' ', '\t']) => rest.trim().to_owned(),
                _ if section == "default" => section,
                _ => return None,
            };
            let region = keys.remove("region").filter(|region| !region.is_empty())?;
            Some((name, region))
        })
        .collect();

    ini_sections(credentials)
        .into_iter()
        .filter_map(|(name, mut keys)| {
            let key_id = keys.remove("aws_access_key_id").filter(|v| !v.is_empty())?;
            let secret = keys
                .remove("aws_secret_access_key")
                .filter(|v| !v.is_empty())?;
            let region = regions.get(&name).cloned();
            Some(AwsProfile {
                name,
                key_id,
                secret,
                region,
            })
        })
        .collect()
}

/// The sections of an INI text in the order they first appear, each with its `key = value` lines
/// (keys in lower case). A section named twice is one section. Comments start a line with `#` or
/// `;`; lines outside any section are ignored.
fn ini_sections(text: &str) -> Vec<(String, HashMap<String, String>)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut sections: Vec<(String, HashMap<String, String>)> = Vec::new();
    let mut current: Option<usize> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            current = Some(match sections.iter().position(|(known, _)| known == name) {
                Some(index) => index,
                None => {
                    sections.push((name.to_owned(), HashMap::new()));
                    sections.len() - 1
                }
            });
        } else if let (Some(index), Some((key, value))) = (current, line.split_once('=')) {
            sections[index]
                .1
                .insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    sections
}

/// Reads the `.env` file the user picked and finds the Bunny zone in it. The error is a sentence
/// for the user; it never quotes the file.
pub fn read_env_file(path: &Path) -> Result<EnvImport, String> {
    let text = std::fs::read_to_string(path).map_err(copy::env_unreadable)?;
    parse_env(&text)
}

/// The profiles of the AWS CLI on this PC that have an access key: `%USERPROFILE%\.aws\credentials`,
/// with the regions of `%USERPROFILE%\.aws\config`. A file that is missing or can't be read counts
/// as empty.
pub fn read_aws_profiles() -> Vec<AwsProfile> {
    let Some(dir) = std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".aws"))
    else {
        return Vec::new();
    };
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
    parse_aws_profiles(&read("credentials"), &read("config"))
}

/// The Add Site fields a `.env` file fills: Bunny, its zone and region. Its password is kept by
/// the window, not in the draft.
pub fn env_draft(import: &EnvImport) -> SiteDraft {
    SiteDraft {
        provider: 0,
        zone: import.zone.as_str().into(),
        region_index: bunny_region_index(&import.region),
        ..SiteDraft::default()
    }
}

/// The Add Site fields an AWS profile fills: Amazon S3, its region and access key ID. Its secret
/// is kept by the window, not in the draft.
pub fn aws_draft(profile: &AwsProfile) -> SiteDraft {
    SiteDraft {
        provider: tile_for(S3Preset::AwsS3),
        region: profile.region.as_deref().unwrap_or_default().into(),
        key_id: profile.key_id.as_str().into(),
        region_index: -1,
        ..SiteDraft::default()
    }
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
