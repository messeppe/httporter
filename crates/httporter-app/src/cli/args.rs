// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading the command line: what follows `--cli` (or `httporter` in a terminal) into a
//! [`Command`]. Hand-written: a few commands with a few options each.
//!
//! - Options are long (`--yes`), with a value after them or after `=` (`--profile x`,
//!   `--profile=x`); `-h` and `-V` are the only short ones. `--` ends the options, for a path that
//!   starts with `-`.
//! - `--help` (or `-h`) anywhere asks for the command's help instead of running it.
//! - A place on a site is `<site>:/path` (`<site>:` alone, or for `ls` just `<site>`, is its
//!   root). The site is everything before the first `:/` (or `:\`), so a site's name may hold a
//!   colon as long as no slash follows it.
//! - Local paths are kept as Windows gave them (not necessarily valid Unicode); everything else
//!   must be text.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use httporter_core::{PurgeMode, RemotePath, Resolution};

use super::copy;

/// A command, as named on the command line; also what its `--help` is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    Publish,
    Ls,
    Upload,
    Download,
    Sites,
    Profiles,
}

impl Topic {
    const ALL: [Topic; 6] = [
        Topic::Publish,
        Topic::Ls,
        Topic::Upload,
        Topic::Download,
        Topic::Sites,
        Topic::Profiles,
    ];

    /// The word that names it.
    pub fn name(self) -> &'static str {
        match self {
            Topic::Publish => "publish",
            Topic::Ls => "ls",
            Topic::Upload => "upload",
            Topic::Download => "download",
            Topic::Sites => "sites",
            Topic::Profiles => "profiles",
        }
    }

    fn named(word: &str) -> Option<Topic> {
        Self::ALL.into_iter().find(|topic| topic.name() == word)
    }
}

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Version,
    /// The help of a command, or of them all.
    Help(Option<Topic>),
    Publish(PublishArgs),
    Ls(LsArgs),
    Upload(UploadArgs),
    Download(DownloadArgs),
    Sites,
    Profiles,
}

/// A place on a site: `<site>:/path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The site's name or id, as typed.
    pub site: String,
    pub path: RemotePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishArgs {
    /// The profile's name or id, as typed.
    pub profile: String,
    pub dry_run: bool,
    pub yes: bool,
    pub no_delete: bool,
    /// `None`: the profile's (or, for `--scheduled`, the schedule's).
    pub purge: Option<PurgeMode>,
    /// Run by the profile's scheduled task (internal: the task's command line has it).
    pub scheduled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsArgs {
    pub target: Remote,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadArgs {
    pub sources: Vec<PathBuf>,
    pub target: Remote,
    /// `--overwrite` or `--skip-existing`; `None`: the site's remembered choice, else Replace if
    /// different.
    pub conflicts: Option<Resolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadArgs {
    pub sources: Vec<Remote>,
    pub dest: PathBuf,
    /// As [`UploadArgs::conflicts`].
    pub conflicts: Option<Resolution>,
}

/// Why the arguments can't be run (exit 2), and the command whose help says how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError {
    pub message: String,
    pub topic: Option<Topic>,
}

impl ArgError {
    fn new(message: String, topic: Option<Topic>) -> Self {
        Self { message, topic }
    }
}

/// The options of a command, as written; and its other arguments.
#[derive(Default)]
struct Scanned {
    switches: Vec<&'static str>,
    values: Vec<(&'static str, String)>,
    positionals: Vec<OsString>,
}

impl Scanned {
    fn has(&self, switch: &str) -> bool {
        self.switches.contains(&switch)
    }

    fn value(&self, option: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(name, _)| *name == option)
            .map(|(_, value)| value.as_str())
    }
}

/// `arg` as text, or the error that it isn't.
fn text(arg: &OsStr, topic: Option<Topic>) -> Result<&str, ArgError> {
    arg.to_str()
        .ok_or_else(|| ArgError::new(copy::not_text(&arg.to_string_lossy()), topic))
}

/// Whether `args` ask for help (before any `--`).
fn wants_help(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--help" || arg == "-h")
}

/// Splits `args` into the `switches` and `valued` options of `topic` and the rest.
fn scan(
    args: &[OsString],
    switches: &[&'static str],
    valued: &[&'static str],
    topic: Topic,
) -> Result<Scanned, ArgError> {
    let fail = |message: String| ArgError::new(message, Some(topic));
    let mut scanned = Scanned::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            scanned.positionals.extend(rest.by_ref().cloned());
            break;
        }
        let looks_like_option = arg
            .to_str()
            .is_some_and(|a| a.starts_with('-') && a.len() > 1);
        if !looks_like_option {
            scanned.positionals.push(arg.clone());
            continue;
        }
        let arg = text(arg, Some(topic))?;
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg, None),
        };
        if let Some(&switch) = switches.iter().find(|&&s| s == name) {
            if inline.is_some() {
                return Err(fail(copy::takes_no_value(switch)));
            }
            if scanned.has(switch) {
                return Err(fail(copy::given_twice(switch)));
            }
            scanned.switches.push(switch);
        } else if let Some(&option) = valued.iter().find(|&&v| v == name) {
            let value = match inline {
                Some(value) => value,
                None => match rest.next() {
                    Some(value) => text(value, Some(topic))?.to_owned(),
                    None => return Err(fail(copy::needs_value(option))),
                },
            };
            if scanned.value(option).is_some() {
                return Err(fail(copy::given_twice(option)));
            }
            scanned.values.push((option, value));
        } else {
            return Err(fail(copy::unknown_option(name)));
        }
    }
    Ok(scanned)
}

/// A place on a site, `<site>:/path`; with `bare_site`, a name alone is the site's root.
fn remote(arg: &OsStr, bare_site: bool, topic: Topic) -> Result<Remote, ArgError> {
    let fail = |message: String| ArgError::new(message, Some(topic));
    let typed = text(arg, Some(topic))?;
    let split = typed
        .find(":/")
        .or_else(|| typed.find(":\\"))
        .map(|at| (&typed[..at], &typed[at + 1..]));
    let (site, path) = match split {
        Some(split) => split,
        None => match typed.strip_suffix(':') {
            Some(site) => (site, ""),
            None if bare_site => (typed, ""),
            None => return Err(fail(copy::not_remote(typed))),
        },
    };
    if site.trim().is_empty() {
        return Err(fail(copy::not_remote(typed)));
    }
    let path = RemotePath::parse(&path.replace('\\', "/"))
        .map_err(|why| fail(copy::bad_remote_path(typed, why)))?;
    Ok(Remote {
        site: site.to_owned(),
        path,
    })
}

/// `--overwrite` or `--skip-existing`, if one is given.
fn conflicts(scanned: &Scanned, topic: Topic) -> Result<Option<Resolution>, ArgError> {
    match (scanned.has("--overwrite"), scanned.has("--skip-existing")) {
        (true, true) => Err(ArgError::new(
            copy::OVERWRITE_AND_SKIP.to_owned(),
            Some(topic),
        )),
        (true, false) => Ok(Some(Resolution::Replace)),
        (false, true) => Ok(Some(Resolution::Skip)),
        (false, false) => Ok(None),
    }
}

fn no_more(scanned: &Scanned, topic: Topic) -> Result<(), ArgError> {
    match scanned.positionals.first() {
        Some(extra) => Err(ArgError::new(
            copy::unexpected(&extra.to_string_lossy()),
            Some(topic),
        )),
        None => Ok(()),
    }
}

fn publish(args: &[OsString]) -> Result<Command, ArgError> {
    let topic = Topic::Publish;
    let scanned = scan(
        args,
        &["--dry-run", "--yes", "--no-delete", "--scheduled"],
        &["--profile", "--purge"],
        topic,
    )?;
    no_more(&scanned, topic)?;
    let profile = scanned
        .value("--profile")
        .filter(|profile| !profile.trim().is_empty())
        .ok_or_else(|| ArgError::new(copy::PROFILE_MISSING.to_owned(), Some(topic)))?
        .to_owned();
    let purge = match scanned.value("--purge") {
        None => None,
        Some(value) => Some(match value.to_ascii_lowercase().as_str() {
            "changed" => PurgeMode::Changed,
            "all" => PurgeMode::Zone,
            "none" => PurgeMode::Off,
            _ => return Err(ArgError::new(copy::bad_purge(value), Some(topic))),
        }),
    };
    Ok(Command::Publish(PublishArgs {
        profile,
        dry_run: scanned.has("--dry-run"),
        yes: scanned.has("--yes"),
        no_delete: scanned.has("--no-delete"),
        purge,
        scheduled: scanned.has("--scheduled"),
    }))
}

fn ls(args: &[OsString]) -> Result<Command, ArgError> {
    let topic = Topic::Ls;
    let mut scanned = scan(args, &["--json"], &[], topic)?;
    if scanned.positionals.is_empty() {
        return Err(ArgError::new(copy::LS_WHAT.to_owned(), Some(topic)));
    }
    let first = scanned.positionals.remove(0);
    no_more(&scanned, topic)?;
    Ok(Command::Ls(LsArgs {
        target: remote(&first, true, topic)?,
        json: scanned.has("--json"),
    }))
}

fn upload(args: &[OsString]) -> Result<Command, ArgError> {
    let topic = Topic::Upload;
    let mut scanned = scan(args, &["--overwrite", "--skip-existing"], &[], topic)?;
    let conflicts = conflicts(&scanned, topic)?;
    if scanned.positionals.len() < 2 {
        return Err(ArgError::new(copy::UPLOAD_WHAT.to_owned(), Some(topic)));
    }
    let last = scanned.positionals.pop().unwrap_or_default();
    Ok(Command::Upload(UploadArgs {
        sources: scanned.positionals.into_iter().map(PathBuf::from).collect(),
        target: remote(&last, false, topic)?,
        conflicts,
    }))
}

fn download(args: &[OsString]) -> Result<Command, ArgError> {
    let topic = Topic::Download;
    let mut scanned = scan(args, &["--overwrite", "--skip-existing"], &[], topic)?;
    let conflicts = conflicts(&scanned, topic)?;
    if scanned.positionals.len() < 2 {
        return Err(ArgError::new(copy::DOWNLOAD_WHAT.to_owned(), Some(topic)));
    }
    let dest = PathBuf::from(scanned.positionals.pop().unwrap_or_default());
    let sources = scanned
        .positionals
        .iter()
        .map(|arg| remote(arg, false, topic))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Command::Download(DownloadArgs {
        sources,
        dest,
        conflicts,
    }))
}

/// Reads `args` (what followed `--cli`).
pub fn parse(args: &[OsString]) -> Result<Command, ArgError> {
    let Some(first) = args.first() else {
        return Err(ArgError::new(copy::NO_COMMAND.to_owned(), None));
    };
    let first = text(first, None)?;
    let rest = &args[1..];
    match first {
        "--version" | "-V" | "version" => match rest.first() {
            None => Ok(Command::Version),
            Some(extra) => Err(ArgError::new(
                copy::unexpected(&extra.to_string_lossy()),
                None,
            )),
        },
        "--help" | "-h" | "help" => match rest.first() {
            None => Ok(Command::Help(None)),
            Some(word) => {
                let word = text(word, None)?;
                Topic::named(word)
                    .map(|topic| Command::Help(Some(topic)))
                    .ok_or_else(|| ArgError::new(copy::unknown_command(word), None))
            }
        },
        word => {
            let topic = Topic::named(word)
                .ok_or_else(|| ArgError::new(copy::unknown_command(word), None))?;
            if wants_help(rest) {
                return Ok(Command::Help(Some(topic)));
            }
            match topic {
                Topic::Publish => publish(rest),
                Topic::Ls => ls(rest),
                Topic::Upload => upload(rest),
                Topic::Download => download(rest),
                Topic::Sites | Topic::Profiles => {
                    let scanned = scan(rest, &[], &[], topic)?;
                    no_more(&scanned, topic)?;
                    Ok(if topic == Topic::Sites {
                        Command::Sites
                    } else {
                        Command::Profiles
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<Command, ArgError> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse(&args)
    }

    #[test]
    fn publish_takes_its_options_both_ways() {
        let parsed = parse_str(&[
            "publish",
            "--profile=My site",
            "--dry-run",
            "--purge",
            "ALL",
        ]);
        assert_eq!(
            parsed,
            Ok(Command::Publish(PublishArgs {
                profile: "My site".to_owned(),
                dry_run: true,
                yes: false,
                no_delete: false,
                purge: Some(PurgeMode::Zone),
                scheduled: false,
            }))
        );
        assert!(parse_str(&["publish"]).is_err());
        assert!(parse_str(&["publish", "--profile", "a", "--purge", "some"]).is_err());
        assert!(parse_str(&["publish", "--profile", "a", "--yes", "--yes"]).is_err());
        assert!(parse_str(&["publish", "--profile", "a", "extra"]).is_err());
        assert!(parse_str(&["publish", "--profile", "a", "--bogus"]).is_err());
    }

    #[test]
    fn places_on_a_site_split_at_the_first_colon_slash() {
        let parsed = parse_str(&["ls", "My: site:/a/b c/%#"]);
        assert_eq!(
            parsed,
            Ok(Command::Ls(LsArgs {
                target: Remote {
                    site: "My: site".to_owned(),
                    path: RemotePath::parse("a/b c/%#").unwrap(),
                },
                json: false,
            }))
        );
        let Ok(Command::Ls(bare)) = parse_str(&["ls", "media", "--json"]) else {
            panic!("ls media");
        };
        assert_eq!(bare.target.path, RemotePath::root());
        assert!(bare.json);
        assert!(parse_str(&["upload", "a.txt", "media"]).is_err());
        assert!(parse_str(&["upload", "a.txt", "media:/../x"]).is_err());
    }

    #[test]
    fn help_and_version_win() {
        assert_eq!(
            parse_str(&["upload", "x", "--help"]),
            Ok(Command::Help(Some(Topic::Upload)))
        );
        assert_eq!(parse_str(&["--help"]), Ok(Command::Help(None)));
        assert_eq!(parse_str(&["-V"]), Ok(Command::Version));
        assert!(parse_str(&[]).is_err());
        assert!(parse_str(&["frobnicate"]).is_err());
        assert!(parse_str(&["sites", "x"]).is_err());
    }

    #[test]
    fn upload_and_download_take_the_last_argument_as_the_other_side() {
        let Ok(Command::Upload(up)) = parse_str(&["upload", "a", "b", "s:/d", "--overwrite"])
        else {
            panic!("upload");
        };
        assert_eq!(up.sources.len(), 2);
        assert_eq!(up.conflicts, Some(Resolution::Replace));
        let Ok(Command::Download(down)) = parse_str(&["download", "s:/x", "s:/y", "C:\\out"])
        else {
            panic!("download");
        };
        assert_eq!(down.sources.len(), 2);
        assert_eq!(down.dest, PathBuf::from("C:\\out"));
        assert!(parse_str(&["download", "s:/x", "o", "--overwrite", "--skip-existing"]).is_err());
    }

    /// Review focus 4: what cmd and PowerShell hand over arrives as typed. Nothing is expanded or
    /// decoded: `%`, `#`, spaces and Unicode stay in names, places and paths.
    #[test]
    fn names_and_paths_keep_spaces_percents_hashes_and_unicode() {
        for name in [
            "My Site",
            "100% #1",
            "%USERPROFILE%",
            "Café Été 日本",
            "a=b",
        ] {
            let Ok(Command::Publish(args)) = parse_str(&["publish", "--profile", name]) else {
                panic!("publish --profile {name}");
            };
            assert_eq!(args.profile, name);
            let inline = format!("--profile={name}");
            let Ok(Command::Publish(args)) = parse_str(&["publish", &inline]) else {
                panic!("publish {inline}");
            };
            assert_eq!(args.profile, name, "after the first = only");
        }
        // A profile named by its id is just a name to the parser.
        let Ok(Command::Publish(args)) = parse_str(&[
            "publish",
            "--profile",
            "0b0f3c1e-5d7a-4c1e-9b1a-2f6d1c7e8a90",
        ]) else {
            panic!("by id");
        };
        assert_eq!(args.profile, "0b0f3c1e-5d7a-4c1e-9b1a-2f6d1c7e8a90");
        // A blank profile is no profile.
        assert!(parse_str(&["publish", "--profile", "  "]).is_err());

        let Ok(Command::Upload(up)) = parse_str(&[
            "upload",
            "C:\\My Files\\Grüße 100% #1.txt",
            "--",
            "-dash.txt",
            "Café Site:/a b/%20/#x",
        ]) else {
            panic!("upload");
        };
        assert_eq!(
            up.sources,
            [
                PathBuf::from("C:\\My Files\\Grüße 100% #1.txt"),
                PathBuf::from("-dash.txt")
            ]
        );
        assert_eq!(up.target.site, "Café Site");
        assert_eq!(up.target.path, RemotePath::parse("a b/%20/#x").unwrap());
        // A backslash after the colon starts the path too.
        let Ok(Command::Ls(ls)) = parse_str(&["ls", "日本:\\docs\\a b"]) else {
            panic!("ls");
        };
        assert_eq!(ls.target.site, "日本");
        assert_eq!(ls.target.path, RemotePath::parse("docs/a b").unwrap());
    }

    #[test]
    fn arguments_that_are_not_text_are_refused() {
        use std::os::windows::ffi::OsStringExt;
        let broken = OsString::from_wide(&[0x0061, 0xD800]);
        let args = vec![
            OsString::from("publish"),
            OsString::from("--profile"),
            broken,
        ];
        assert!(parse(&args).is_err());
        // A local path need not be text: Windows names it.
        let broken = OsString::from_wide(&[0x0061, 0xD800]);
        let args = vec![OsString::from("upload"), broken, OsString::from("s:/x")];
        assert!(matches!(parse(&args), Ok(Command::Upload(_))));
    }
}
