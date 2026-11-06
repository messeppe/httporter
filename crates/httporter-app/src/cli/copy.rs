// SPDX-License-Identifier: MIT OR Apache-2.0

//! Every word the CLI prints (`httporter.exe --cli …`, through `httporter.com`): results on
//! stdout, progress and errors on stderr.

use std::path::Path;

use httporter_core::{PurgeResult, RemotePath};

use super::args::{ArgError, Topic};
use crate::format::{format_count, format_size};
use crate::publish::headless::HeadlessProgress;

// ---- Help and version ----

/// `httporter --version`.
pub fn version() -> String {
    format!("httporter {}", env!("CARGO_PKG_VERSION"))
}

/// `httporter --help`, or a command's `--help`.
pub fn help(topic: Option<Topic>) -> String {
    match topic {
        None => format!(
            "HTTPorter {}: publish and transfer from the command line.\n\n{HELP}",
            env!("CARGO_PKG_VERSION")
        ),
        Some(Topic::Publish) => HELP_PUBLISH.to_owned(),
        Some(Topic::Ls) => HELP_LS.to_owned(),
        Some(Topic::Upload) => HELP_UPLOAD.to_owned(),
        Some(Topic::Download) => HELP_DOWNLOAD.to_owned(),
        Some(Topic::Sites) => HELP_SITES.to_owned(),
        Some(Topic::Profiles) => HELP_PROFILES.to_owned(),
    }
}

const HELP: &str = "\
Usage:
  httporter publish --profile <name or id> [--dry-run] [--yes] [--no-delete]
                    [--purge changed|all|none]
  httporter ls <site>[:/folder] [--json]
  httporter upload <file or folder>... <site>:/folder [--overwrite | --skip-existing]
  httporter download <site>:/path... <folder> [--overwrite | --skip-existing]
  httporter sites
  httporter profiles
  httporter <command> --help
  httporter --version

Sites and profiles are the ones in HTTPorter, named as there (any case will do) or by the id
that `httporter sites` and `httporter profiles` show. Put a name with spaces in quotes:
  httporter publish --profile \"My site\"
  httporter ls \"My site:/blog/2026\"
Keys and passwords are read from Windows Credential Manager, as HTTPorter reads them. Nothing
here asks for one.

Exit codes:
  0  done
  1  some transfers failed, or the run was stopped
  2  a setup problem: a name, an option, a folder or a site setting is wrong
  3  signing in to the site failed: its key or password is wrong or missing
  4  deletions were held back by the delete guard: run again with --yes to make them";

const HELP_PUBLISH: &str = "\
httporter publish --profile <name or id> [options]

Publishes a profile as HTTPorter's Publish button does: compares the folder with the site,
uploads what is new or changed, deletes what is only on the site (when the profile says to),
then purges the CDN. Nothing is asked: a publish that would delete more files than the
profile's delete guard allows uploads, keeps those files and exits with 4.

Options:
  --profile <name or id>    the publish profile (see httporter profiles)
  --dry-run                 list what would change, and change nothing
  --yes                     delete even past the delete guard
  --no-delete               delete nothing this time
  --purge changed|all|none  purge the changed files, the whole pull zone, or nothing
                            (default: as the profile says)

Exit codes: 0 done, 1 some files failed, 2 a setup problem, 3 sign-in failed,
4 deletions held back by the delete guard.";

const HELP_LS: &str = "\
httporter ls <site>[:/folder] [--json]

Lists a folder of a site (its root without a folder): size, date and name, one line each,
folders first and marked with a trailing /.

Options:
  --json   the entries as a JSON array of {name, size, modified, folder}";

const HELP_UPLOAD: &str = "\
httporter upload <file or folder>... <site>:/folder [options]

Uploads files and folders (with everything in them) into a folder of a site. A file already
there is replaced only if it differs, unless the site remembers another choice.

Options:
  --overwrite       replace files that are there already
  --skip-existing   leave files that are there already";

const HELP_DOWNLOAD: &str = "\
httporter download <site>:/path... <folder> [options]

Downloads files and folders (with everything in them) of one site into a folder on this PC.
A file already there is replaced only if it differs, unless the site remembers another choice.

Options:
  --overwrite       replace files that are there already
  --skip-existing   leave files that are there already";

const HELP_SITES: &str = "\
httporter sites

Lists the sites of HTTPorter: name, service and id.";

const HELP_PROFILES: &str = "\
httporter profiles

Lists the publish profiles of HTTPorter: name, site and id.";

// ---- Mistakes in the arguments (exit 2) ----

/// A mistake in the arguments, and where to read how it goes.
pub fn usage_error(e: &ArgError) -> String {
    let command = match e.topic {
        Some(topic) => format!("httporter {} --help", topic.name()),
        None => "httporter --help".to_owned(),
    };
    format!(
        "httporter: {}\nRun \u{201c}{command}\u{201d} to see how.",
        e.message
    )
}

pub const NO_COMMAND: &str = "Name a command: publish, ls, upload, download, sites or profiles.";

pub fn unknown_command(name: &str) -> String {
    format!("There's no command \u{201c}{name}\u{201d}.")
}

pub fn unknown_option(option: &str) -> String {
    format!("There's no option {option} here.")
}

pub fn needs_value(option: &str) -> String {
    format!("{option} needs a value.")
}

pub fn takes_no_value(option: &str) -> String {
    format!("{option} takes no value.")
}

pub fn given_twice(option: &str) -> String {
    format!("{option} is given twice.")
}

pub fn unexpected(argument: &str) -> String {
    format!("Unexpected \u{201c}{argument}\u{201d}.")
}

pub fn not_text(argument: &str) -> String {
    format!("\u{201c}{argument}\u{201d} isn't valid text.")
}

pub const PROFILE_MISSING: &str = "Say which profile to publish: --profile <name or id>.";

pub fn bad_purge(value: &str) -> String {
    format!("--purge takes changed, all or none, not \u{201c}{value}\u{201d}.")
}

pub const OVERWRITE_AND_SKIP: &str = "Choose --overwrite or --skip-existing, not both.";

pub const LS_WHAT: &str = "Say what to list: httporter ls <site>[:/folder].";

pub const UPLOAD_WHAT: &str =
    "Say what to upload and where: httporter upload <file or folder>... <site>:/folder.";

pub const DOWNLOAD_WHAT: &str =
    "Say what to download and where: httporter download <site>:/path... <folder>.";

pub fn not_remote(argument: &str) -> String {
    format!("\u{201c}{argument}\u{201d} isn't a place on a site: write it as <site>:/folder.")
}

pub fn bad_remote_path(argument: &str, why: impl std::fmt::Display) -> String {
    format!("\u{201c}{argument}\u{201d} isn't a path on a site: {why}.")
}

pub const ONE_SITE: &str = "Download from one site at a time.";

// ---- Errors of a command ----

/// An error of a command, on stderr.
pub fn error(text: &str) -> String {
    format!("httporter: {text}")
}

/// Something worth knowing that didn't stop the command, on stderr.
pub fn note(text: &str) -> String {
    format!("httporter: note: {text}")
}

pub const RESET_PENDING: &str = "HTTPorter is about to be reset (Settings \u{203a} Security). Start \
                                 HTTPorter once first, then try again.";

pub fn runtime_failed(reason: impl std::fmt::Display) -> String {
    format!("couldn't start: {reason}")
}

pub const CANCELLING: &str = "httporter: stopping\u{2026} (press Ctrl+C again to stop at once)";

pub fn no_site(name: &str) -> String {
    format!("No site is named \u{201c}{name}\u{201d}. httporter sites lists them.")
}

pub fn no_profile(name: &str) -> String {
    format!("No publish profile is named \u{201c}{name}\u{201d}. httporter profiles lists them.")
}

/// More than one site or profile has that name: each with its id, to use instead.
pub fn ambiguous(what: &str, name: &str, candidates: &[(String, String)]) -> String {
    let mut text =
        format!("More than one {what} is named \u{201c}{name}\u{201d}. Use its id instead:");
    for (name, id) in candidates {
        text.push_str(&format!("\n  {id}  {name}"));
    }
    text
}

pub const SITE: &str = "site";
pub const PROFILE: &str = "publish profile";

// ---- sites, profiles ----

pub const NO_SITES: &str = "No sites yet. Add one in HTTPorter.";
pub const NO_PROFILES: &str = "No publish profiles yet. Make one in HTTPorter.";

// ---- ls ----

/// An `ls` line's size: the file's, or a dash for a folder.
pub fn ls_size(size: u64, folder: bool) -> String {
    if folder {
        "-".to_owned()
    } else {
        format_size(size)
    }
}

/// An `ls` line's date when the site gives none.
pub const LS_NO_DATE: &str = "-";

/// An `ls` line's name: a folder's ends with `/`.
pub fn ls_name(name: &str, folder: bool) -> String {
    if folder {
        format!("{name}/")
    } else {
        name.to_owned()
    }
}

pub fn ls_empty(site: &str, dir: &RemotePath) -> String {
    format!("{site}:/{} is empty.", dir.as_str())
}

// ---- upload, download ----

pub fn local_missing(path: &Path) -> String {
    format!("There's nothing at {} on this PC.", path.display())
}

pub fn not_a_folder(path: &Path) -> String {
    format!("{} isn't a folder on this PC.", path.display())
}

pub fn no_such_path(site: &str, path: &RemotePath) -> String {
    format!("There's nothing at {site}:/{}.", path.as_str())
}

pub fn skipped(path: &Path, why: &str) -> String {
    note(&format!("skipped {}: {why}", path.display()))
}

pub fn renamed(remote: &RemotePath, name: &str) -> String {
    note(&format!(
        "/{} is saved as \u{201c}{name}\u{201d}: Windows can't store its own name, or another \
         item has it",
        remote.as_str()
    ))
}

pub const NOTHING_TO_UPLOAD: &str = "Nothing to upload: those files are there already.";
pub const NOTHING_TO_DOWNLOAD: &str = "Nothing to download: those files are here already.";

pub fn transfer_not_started(reason: &str) -> String {
    format!("Couldn't start: HTTPorter couldn't save its list of transfers ({reason}).")
}

fn files(n: u64) -> String {
    match n {
        1 => "1 file".to_owned(),
        n => format!("{} files", format_count(n)),
    }
}

pub fn uploaded(files_done: u64, bytes: u64, site: &str, dir: &RemotePath) -> String {
    format!(
        "Uploaded {} ({}) to {site}:/{}.",
        files(files_done),
        format_size(bytes),
        dir.as_str()
    )
}

pub fn downloaded(files_done: u64, bytes: u64, dir: &Path) -> String {
    format!(
        "Downloaded {} ({}) to {}.",
        files(files_done),
        format_size(bytes),
        dir.display()
    )
}

pub fn failed_line(path: &RemotePath, why: &str) -> String {
    format!("httporter: failed: /{}: {why}", path.as_str())
}

pub fn some_failed(n: u64) -> String {
    error(&format!("{} failed.", files(n)))
}

pub fn some_stopped(n: u64) -> String {
    error(&format!("Stopped: {} didn't transfer.", files(n)))
}

// ---- Progress (stderr, a console only) ----

/// A transfer's progress line.
pub fn transfer_progress(done: u64, total: u64, bytes_done: u64, bytes_total: u64) -> String {
    format!(
        "{} of {} \u{b7} {} of {}",
        format_count(done),
        files(total),
        format_size(bytes_done),
        format_size(bytes_total)
    )
}

/// A publish's progress line.
pub fn progress_line(p: HeadlessProgress) -> String {
    match p {
        HeadlessProgress::Comparing { done, total: 0 } => {
            format!("Comparing\u{2026} {} listed", files(done))
        }
        HeadlessProgress::Comparing { done, total } => format!(
            "Comparing\u{2026} {} of {} checked",
            format_count(done),
            files(total)
        ),
        HeadlessProgress::Uploading { done, total, bytes } => format!(
            "Uploading\u{2026} {} of {} \u{b7} {}",
            format_count(done),
            files(total),
            format_size(bytes)
        ),
        HeadlessProgress::Deleting { done, total } => format!(
            "Deleting\u{2026} {} of {}",
            format_count(done),
            files(total)
        ),
        HeadlessProgress::Purging => "Purging the CDN\u{2026}".to_owned(),
    }
}

// ---- publish ----

fn purge_text(purge: &PurgeResult) -> String {
    match purge {
        PurgeResult::Off | PurgeResult::Urls(0) => String::new(),
        PurgeResult::Urls(n) => format!(" CDN purged ({} URLs).", format_count(*n)),
        PurgeResult::Zone => " CDN pull zone purged.".to_owned(),
        PurgeResult::Failed(why) => format!(" The CDN purge failed: {why}"),
        PurgeResult::Unavailable(why) => format!(" Not purged: {why}"),
    }
}

pub fn published(
    site: &str,
    uploaded: u64,
    deleted: u64,
    bytes: u64,
    purge: &PurgeResult,
) -> String {
    format!(
        "Published to {site}: {} uploaded ({}), {} deleted.{}",
        format_count(uploaded),
        format_size(bytes),
        format_count(deleted),
        purge_text(purge)
    )
}

pub fn publish_failed_files(failed: u64) -> String {
    error(&format!(
        "{} failed. Run the publish again, or retry them on the profile page in HTTPorter.",
        files(failed)
    ))
}

pub fn publish_stopped(site: &str, uploaded: u64, deleted: u64) -> String {
    format!(
        "Publishing to {site} was stopped: {} uploaded, {} deleted.",
        format_count(uploaded),
        format_count(deleted)
    )
}

pub fn up_to_date(site: &str) -> String {
    format!("{site} is up to date: nothing to publish.")
}

pub fn guard_tripped(site: &str, would_delete: u64, uploaded: u64) -> String {
    format!(
        "Published to {site}: {} uploaded. {} only on the site weren't deleted: more than the \
         profile's delete guard allows. See them with --dry-run, then run again with --yes to \
         delete them.",
        format_count(uploaded),
        files(would_delete)
    )
}

pub fn busy(profile: &str) -> String {
    format!(
        "\u{201c}{profile}\u{201d} is being published already (in HTTPorter, or by another \
         command or schedule). Try again when that ends."
    )
}

pub fn schedule_off(profile: &str) -> String {
    format!(
        "\u{201c}{profile}\u{201d} has no schedule turned on, so it wasn't published. Turn it on \
         in HTTPorter, or remove the task from Task Scheduler."
    )
}

/// A dry run's line for one file of the plan.
pub fn plan_line(what: &str, size: u64, rel: &str) -> String {
    format!("{what:<8} {:>10}  {rel}", format_size(size))
}

pub const PLAN_NEW: &str = "new";
pub const PLAN_CHANGED: &str = "changed";
pub const PLAN_DELETE: &str = "delete";
pub const PLAN_KEEP: &str = "keep";

pub fn dry_run(new: u64, changed: u64, delete: u64, bytes: u64, guarded: bool) -> String {
    let mut text = format!(
        "Dry run: {} new and {} changed would be uploaded ({}), {} deleted. Nothing was changed.",
        format_count(new),
        format_count(changed),
        format_size(bytes),
        format_count(delete)
    );
    if guarded {
        text.push_str(
            " The deletions are more than the profile's delete guard allows: publishing would \
             keep them unless run with --yes.",
        );
    }
    text
}

// ---- The scheduled publish's notification (on failure only) ----

pub const NOTIFY_TITLE: &str = "HTTPorter";

pub fn scheduled_failed(profile: &str, why: &str) -> String {
    format!("The scheduled publish of \u{201c}{profile}\u{201d} didn't go through: {why}")
}

pub fn scheduled_files_failed(failed: u64) -> String {
    format!("{} failed.", files(failed))
}

pub fn scheduled_guard(would_delete: u64) -> String {
    format!(
        "{} only on the site need your review before they are deleted.",
        files(would_delete)
    )
}
