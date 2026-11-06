// SPDX-License-Identifier: MIT OR Apache-2.0

//! The CLI's commands, over a [`HeadlessCtx`]: the same session, credentials, engine, journal,
//! history and activity log as the window.
//!
//! - Sites and profiles are found by id or by exact name, any case ([`find`]); more than one of
//!   that name is refused with each one's id (exit 2).
//! - `publish` is the headless publish (`run_headless`), as a schedule and the watch folder run
//!   it; `upload` and `download` plan as the window does and run one engine batch; `ls`, `sites`
//!   and `profiles` only read.
//! - Every exit code is `httporter_core::exit`'s: a refused sign-in is 3 at once, a mistake in
//!   a name, a path or a setting 2, failed transfers 1 once the rest is done.

use std::collections::{HashMap, HashSet};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, SecondsFormat};
use futures_util::future::{Either, select};
use httporter_core::{
    Checksum, CompareStatus, Entry, HeadlessOptions, HeadlessOutcome, HistorySource, Job,
    ProfileRecord, ProviderError, RemotePath, Resolution, SiteRecord, StorageProvider, exit,
};
use httporter_platform::win;
use httporter_transfer::{
    Batch, BatchOutcome, plan_download, plan_upload, policy_to_resolution, resolve_downloads,
    resolve_uploads,
};

use super::args::{Command, DownloadArgs, LsArgs, PublishArgs, Remote, UploadArgs};
use super::console::{Cancel, Progress, err, out};
use super::copy;
use crate::copy::{Attempt, describe_for};
use crate::publish::flow;
use crate::publish::headless::{
    HeadlessCtx, HeadlessError, HeadlessProgress, log_busy, log_not_run, run_headless, supervise,
};
use crate::session::{SiteProvider, blocking, listing_error_text};
use crate::site_hub::{Interactive, list_interactive};

/// After Ctrl+C, how long the command may take to stop cleanly (its batches cancelled, the
/// history and the journal written) before it is given up.
const STOP_WITHIN: Duration = Duration::from_secs(10);

/// Runs `command` until it ends or Ctrl+C stops it; the exit code.
pub async fn run(ctx: &HeadlessCtx, command: Command, cancel: &Cancel) -> i32 {
    let work = pin!(dispatch(ctx, command));
    let fired = pin!(cancel.fired());
    match select(work, fired).await {
        Either::Left((code, _)) => code,
        Either::Right(((), work)) => {
            err(copy::CANCELLING);
            ctx.cancel();
            match tokio::time::timeout(STOP_WITHIN, work).await {
                Ok(code) if code != exit::OK => code,
                _ => exit::FAILURES,
            }
        }
    }
}

async fn dispatch(ctx: &HeadlessCtx, command: Command) -> i32 {
    match command {
        Command::Sites => sites(ctx),
        Command::Profiles => profiles(ctx).await,
        Command::Ls(args) => ls(ctx, args).await,
        Command::Upload(args) => upload(ctx, args).await,
        Command::Download(args) => download(ctx, args).await,
        Command::Publish(args) => publish(ctx, args).await,
        // Answered before anything opened.
        Command::Version | Command::Help(_) => exit::OK,
    }
}

/// Says `text` on stderr as an error; `code`.
fn fail(code: i32, text: &str) -> i32 {
    err(&copy::error(text));
    code
}

/// The exit code for an error of the site's.
fn code_of(e: &ProviderError) -> i32 {
    match e {
        ProviderError::Auth { .. } => exit::AUTH,
        ProviderError::NotFound => exit::CONFIG,
        _ => exit::FAILURES,
    }
}

/// `rows` as columns two spaces apart, each as wide as its widest cell (by characters).
fn table(rows: &[Vec<String>]) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(i) {
                Some(w) => *w = (*w).max(width),
                None => widths.push(width),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i + 1 == row.len() {
                    line.push_str(cell);
                } else {
                    let pad = widths[i] - cell.chars().count() + 2;
                    line.push_str(cell);
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            line
        })
        .collect()
}

// ---- Finding sites and profiles ----

/// What a name or an id found.
enum Found<'a, T> {
    One(&'a T),
    Nothing,
    Several(Vec<&'a T>),
}

/// The item whose id is `wanted`, else those whose name is `wanted` in any case.
fn find<'a, T>(
    items: &'a [T],
    wanted: &str,
    id: impl Fn(&T) -> &str,
    name: impl Fn(&T) -> &str,
) -> Found<'a, T> {
    if let Some(item) = items
        .iter()
        .find(|item| id(item).eq_ignore_ascii_case(wanted))
    {
        return Found::One(item);
    }
    let wanted = wanted.to_lowercase();
    let named: Vec<&T> = items
        .iter()
        .filter(|item| name(item).to_lowercase() == wanted)
        .collect();
    match named.len() {
        0 => Found::Nothing,
        1 => Found::One(named[0]),
        _ => Found::Several(named),
    }
}

/// The site `wanted` names; or the exit code, having said why not.
fn pick_site<'a>(sites: &'a [SiteRecord], wanted: &str) -> Result<&'a SiteRecord, i32> {
    match find(sites, wanted, |s| &s.id.0, |s| &s.name) {
        Found::One(site) => Ok(site),
        Found::Nothing => Err(fail(exit::CONFIG, &copy::no_site(wanted))),
        Found::Several(several) => {
            let candidates: Vec<(String, String)> = several
                .iter()
                .map(|s| (s.name.clone(), s.id.0.clone()))
                .collect();
            Err(fail(
                exit::CONFIG,
                &copy::ambiguous(copy::SITE, wanted, &candidates),
            ))
        }
    }
}

/// The profile `wanted` names; or the exit code, having said why not.
fn pick_profile<'a>(profiles: &'a [ProfileRecord], wanted: &str) -> Result<&'a ProfileRecord, i32> {
    match find(profiles, wanted, |p| &p.id.0, |p| &p.name) {
        Found::One(profile) => Ok(profile),
        Found::Nothing => Err(fail(exit::CONFIG, &copy::no_profile(wanted))),
        Found::Several(several) => {
            let candidates: Vec<(String, String)> = several
                .iter()
                .map(|p| (p.name.clone(), p.id.0.clone()))
                .collect();
            Err(fail(
                exit::CONFIG,
                &copy::ambiguous(copy::PROFILE, wanted, &candidates),
            ))
        }
    }
}

/// The site `wanted` names and its storage client; or the exit code, having said why not.
async fn connect(
    ctx: &HeadlessCtx,
    wanted: &str,
) -> Result<(SiteRecord, Arc<dyn StorageProvider>), i32> {
    let sites = ctx.session.sites();
    let site = pick_site(&sites, wanted)?.clone();
    match ctx.provider(&site).await {
        Ok(provider) => Ok((site, provider)),
        Err(e) => Err(fail(e.exit_code(), e.text())),
    }
}

// ---- sites, profiles ----

fn sites(ctx: &HeadlessCtx) -> i32 {
    let sites = ctx.session.sites();
    if sites.is_empty() {
        err(copy::NO_SITES);
        return exit::OK;
    }
    let rows: Vec<Vec<String>> = sites
        .iter()
        .map(|site| {
            vec![
                site.name.clone(),
                SiteProvider::of(site).label().to_owned(),
                site.id.0.clone(),
            ]
        })
        .collect();
    for line in table(&rows) {
        out(&line);
    }
    exit::OK
}

async fn profiles(ctx: &HeadlessCtx) -> i32 {
    let profiles = ctx.session.profiles().await;
    if profiles.is_empty() {
        err(copy::NO_PROFILES);
        return exit::OK;
    }
    let rows: Vec<Vec<String>> = profiles
        .iter()
        .map(|profile| {
            let site = ctx
                .session
                .site(&profile.site)
                .map(|site| site.name)
                .unwrap_or_default();
            vec![profile.name.clone(), site, profile.id.0.clone()]
        })
        .collect();
    for line in table(&rows) {
        out(&line);
    }
    exit::OK
}

// ---- ls ----

async fn ls(ctx: &HeadlessCtx, args: LsArgs) -> i32 {
    let (site, provider) = match connect(ctx, &args.target.site).await {
        Ok(found) => found,
        Err(code) => return code,
    };
    let dir = &args.target.path;
    let mut entries = match list_interactive(&*provider, dir).await {
        Ok(listing) => listing.entries,
        Err(e) => return fail(code_of(&e), &listing_error_text(&site, &e, dir)),
    };
    entries.sort_by(|a, b| {
        b.is_folder
            .cmp(&a.is_folder)
            .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
            .then_with(|| a.name().cmp(b.name()))
    });
    if args.json {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "name": entry.name(),
                    "size": entry.size,
                    "modified": entry
                        .modified
                        .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true)),
                    "folder": entry.is_folder,
                })
            })
            .collect();
        out(&serde_json::to_string_pretty(&items).unwrap_or_default());
        return exit::OK;
    }
    if entries.is_empty() {
        err(&copy::ls_empty(&site.name, dir));
        return exit::OK;
    }
    let rows: Vec<(String, String, String)> = entries
        .iter()
        .map(|entry| {
            let modified = entry.modified.map_or_else(
                || copy::LS_NO_DATE.to_owned(),
                |at| {
                    at.with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                },
            );
            (
                copy::ls_size(entry.size, entry.is_folder),
                modified,
                copy::ls_name(entry.name(), entry.is_folder),
            )
        })
        .collect();
    let size_width = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    let date_width = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
    for (size, modified, name) in rows {
        out(&format!(
            "{size:>size_width$}  {modified:<date_width$}  {name}"
        ));
    }
    exit::OK
}

// ---- upload, download ----

/// How conflicts go: `flag`'s way, else the site's remembered choice (unless that is Ask), else
/// replace only what differs.
async fn resolution(
    ctx: &HeadlessCtx,
    site: &SiteRecord,
    upload: bool,
    flag: Option<Resolution>,
) -> Resolution {
    if let Some(flag) = flag {
        return flag;
    }
    ctx.session
        .remembered_conflicts(&site.id, upload)
        .await
        .and_then(policy_to_resolution)
        .unwrap_or(Resolution::ReplaceIfDifferent)
}

/// Journals `jobs` as one batch to `site` and waits for it, with its progress; the outcome, and
/// what stopped it early if anything did. `Err` is the exit code once said why it didn't start.
async fn transfer(
    ctx: &HeadlessCtx,
    site: &SiteRecord,
    provider: Arc<dyn StorageProvider>,
    jobs: Vec<Job>,
    checksums: HashMap<RemotePath, Checksum>,
) -> Result<(BatchOutcome, Option<HeadlessError>), i32> {
    let batch = Batch {
        site: site.id.clone(),
        site_name: site.name.clone(),
        host: site.host.clone(),
        provider,
        jobs,
        checksums,
    };
    let engine = ctx.engine.clone();
    // The journal is written before anything starts: a disk write, so off the runtime.
    let id = match blocking(move || engine.submit_tracked(batch)).await {
        Ok(id) => id,
        Err(e) => return Err(fail(exit::FAILURES, &copy::transfer_not_started(&e.0))),
    };
    let progress = Progress::new();
    let tick = || {
        let s = ctx.engine.snapshot();
        progress.show(copy::transfer_progress(
            s.files_done,
            s.files_total,
            s.bytes_done,
            s.bytes_total,
        ));
    };
    Ok(supervise(ctx, site, tick, ctx.engine.wait_batch(&id)).await)
}

/// What a transfer did, said; its exit code. `files` are the jobs that move a file (not the
/// folders made on the way); `summary` says how many of them went, and how many bytes.
fn report(
    outcome: &BatchOutcome,
    stuck: Option<HeadlessError>,
    files: &HashSet<RemotePath>,
    summary: impl FnOnce(u64, u64) -> String,
) -> i32 {
    for (path, why) in &outcome.failed {
        err(&copy::failed_line(path, why));
    }
    let done = outcome
        .done
        .iter()
        .filter(|path| files.contains(*path))
        .count() as u64;
    out(&summary(done, outcome.bytes_done));
    if let Some(why) = stuck {
        return fail(why.exit_code(), why.text());
    }
    let failed = outcome.failed.len() as u64;
    let stopped = outcome.cancelled.len() as u64;
    if failed > 0 {
        err(&copy::some_failed(failed));
        return exit::FAILURES;
    }
    if stopped > 0 {
        err(&copy::some_stopped(stopped));
        return exit::FAILURES;
    }
    exit::OK
}

async fn upload(ctx: &HeadlessCtx, args: UploadArgs) -> i32 {
    for source in &args.sources {
        if std::fs::symlink_metadata(source).is_err() {
            return fail(exit::CONFIG, &copy::local_missing(source));
        }
    }
    let (site, provider) = match connect(ctx, &args.target.site).await {
        Ok(found) => found,
        Err(code) => return code,
    };
    if crate::manage::settings_tabs::site_read_only(&ctx.session, &site.id) {
        return fail(
            exit::CONFIG,
            &crate::manage::settings_tabs::read_only_refused(&site.name),
        );
    }
    ctx.register_cache_control(&site).await;
    let dir = args.target.path.clone();
    let planner = Interactive(provider.clone());
    let plan = match plan_upload(&planner, &args.sources, &dir).await {
        Ok(plan) => plan,
        Err(e) => {
            let text = describe_for(
                site.provider,
                &e,
                &site.container,
                &site.host,
                Attempt::List(&dir),
            );
            return fail(code_of(&e), &text);
        }
    };
    for (path, why) in &plan.skipped {
        err(&copy::skipped(path, why));
    }
    let resolution = resolution(ctx, &site, true, args.conflicts).await;
    let jobs = resolve_uploads(plan, resolution, &HashMap::new());
    let files: HashSet<RemotePath> = jobs
        .iter()
        .filter_map(|job| match job {
            Job::Upload { remote, .. } => Some(remote.clone()),
            _ => None,
        })
        .collect();
    if files.is_empty() {
        out(copy::NOTHING_TO_UPLOAD);
        return exit::OK;
    }
    match transfer(ctx, &site, provider, jobs, HashMap::new()).await {
        Ok((outcome, stuck)) => report(&outcome, stuck, &files, |done, bytes| {
            copy::uploaded(done, bytes, &site.name, &dir)
        }),
        Err(code) => code,
    }
}

/// The file or folder at `path` of `site`, as its folder's listing has it (the root is a folder).
async fn entry_at(
    provider: &dyn StorageProvider,
    site: &SiteRecord,
    path: &RemotePath,
) -> Result<Entry, i32> {
    let Some(parent) = path.parent() else {
        return Ok(Entry {
            path: RemotePath::root(),
            is_folder: true,
            size: 0,
            modified: None,
            checksum: None,
            content_type: None,
            replicated_regions: Vec::new(),
        });
    };
    let listing = match list_interactive(provider, &parent).await {
        Ok(listing) => listing,
        Err(e) => return Err(fail(code_of(&e), &listing_error_text(site, &e, &parent))),
    };
    listing
        .entries
        .into_iter()
        .find(|entry| entry.name() == path.name().unwrap_or_default())
        .ok_or_else(|| fail(exit::CONFIG, &copy::no_such_path(&site.name, path)))
}

async fn download(ctx: &HeadlessCtx, args: DownloadArgs) -> i32 {
    if !args.dest.is_dir() {
        return fail(exit::CONFIG, &copy::not_a_folder(&args.dest));
    }
    let Some(first) = args.sources.first() else {
        return exit::CONFIG;
    };
    let one_site = args
        .sources
        .iter()
        .all(|Remote { site, .. }| site.to_lowercase() == first.site.to_lowercase());
    if !one_site {
        return fail(exit::CONFIG, copy::ONE_SITE);
    }
    let (site, provider) = match connect(ctx, &first.site).await {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut entries = Vec::new();
    for source in &args.sources {
        match entry_at(&*provider, &site, &source.path).await {
            Ok(entry) => entries.push(entry),
            Err(code) => return code,
        }
    }
    let planner = Interactive(provider.clone());
    let plan = match plan_download(&planner, &entries, &args.dest).await {
        Ok(plan) => plan,
        Err(e) => {
            let at = args.sources[0]
                .path
                .parent()
                .unwrap_or_else(RemotePath::root);
            return fail(code_of(&e), &listing_error_text(&site, &e, &at));
        }
    };
    for (remote, name) in &plan.renamed {
        err(&copy::renamed(remote, name));
    }
    let resolution = resolution(ctx, &site, false, args.conflicts).await;
    // What arrives is checked against the listing (ruling Q27).
    let checksums = plan.checksums.clone();
    let jobs = resolve_downloads(plan, resolution, &HashMap::new());
    let files: HashSet<RemotePath> = jobs
        .iter()
        .filter_map(|job| match job {
            Job::Download { remote, .. } => Some(remote.clone()),
            _ => None,
        })
        .collect();
    if files.is_empty() {
        out(copy::NOTHING_TO_DOWNLOAD);
        return exit::OK;
    }
    let dest = args.dest.clone();
    match transfer(ctx, &site, provider, jobs, checksums).await {
        Ok((outcome, stuck)) => report(&outcome, stuck, &files, |done, bytes| {
            copy::downloaded(done, bytes, &dest)
        }),
        Err(code) => code,
    }
}

// ---- publish ----

/// A dry run's plan, one line per file, then what it comes to.
fn print_dry_run(ctx: &HeadlessCtx, profile: &ProfileRecord, outcome: &HeadlessOutcome, yes: bool) {
    let HeadlessOutcome::DryRun {
        new,
        changed,
        delete,
        bytes,
    } = *outcome
    else {
        return;
    };
    let plan = ctx.last_plan();
    if let Some(plan) = &plan {
        for item in &plan.items {
            let what = match item.status {
                CompareStatus::New => copy::PLAN_NEW,
                CompareStatus::Changed => copy::PLAN_CHANGED,
                CompareStatus::RemoteOnly if item.include => copy::PLAN_DELETE,
                CompareStatus::RemoteOnly => copy::PLAN_KEEP,
                CompareStatus::Same => continue,
            };
            out(&copy::plan_line(what, item.size, &item.rel));
        }
    }
    let guarded = !yes && plan.is_some_and(|plan| flow::guard_trips(profile, &plan));
    out(&copy::dry_run(new, changed, delete, bytes, guarded));
}

async fn publish(ctx: &HeadlessCtx, args: PublishArgs) -> i32 {
    let profiles = ctx.session.profiles().await;
    let profile = match pick_profile(&profiles, &args.profile) {
        Ok(profile) => profile.clone(),
        Err(code) => return code,
    };
    let source = if args.scheduled {
        HistorySource::Scheduled
    } else {
        HistorySource::Cli
    };
    let schedule = profile.extras.schedule.clone();
    // A task left behind by a schedule turned off publishes nothing.
    if args.scheduled && !schedule.as_ref().is_some_and(|s| s.enabled) {
        let text = copy::schedule_off(&profile.name);
        log_not_run(ctx, &profile, source, &text);
        return fail(exit::CONFIG, &text);
    }
    let purge = args.purge.or_else(|| {
        args.scheduled
            .then(|| schedule.as_ref().and_then(|s| s.purge))
            .flatten()
    });
    let opts = HeadlessOptions {
        dry_run: args.dry_run,
        yes: args.yes,
        no_delete: args.no_delete,
        purge,
        source,
    };
    let site = ctx
        .session
        .site(&profile.site)
        .map(|site| site.name)
        .unwrap_or_default();
    let progress = Progress::new();
    let show = |p: HeadlessProgress| progress.show(copy::progress_line(p));
    let result = run_headless(ctx, &profile, &opts, &show).await;
    for note in ctx.take_notes() {
        err(&copy::note(&note));
    }
    // What a scheduled run that went wrong says in a notification.
    let (code, notice) = match &result {
        Ok(HeadlessOutcome::Busy) => {
            log_busy(ctx, &profile, source);
            // Busy because the profile is being published: nothing to tell.
            (fail(exit::FAILURES, &copy::busy(&profile.name)), None)
        }
        Ok(outcome @ HeadlessOutcome::DryRun { .. }) => {
            print_dry_run(ctx, &profile, outcome, args.yes);
            (exit::OK, None)
        }
        Ok(HeadlessOutcome::GuardTripped {
            would_delete,
            uploaded,
        }) => {
            out(&copy::guard_tripped(&site, *would_delete, *uploaded));
            (exit::GUARD, Some(copy::scheduled_guard(*would_delete)))
        }
        Ok(HeadlessOutcome::Done {
            uploaded,
            deleted,
            failed,
            bytes,
            purge,
        }) => {
            if ctx.is_cancelled() {
                out(&copy::publish_stopped(&site, *uploaded, *deleted));
                (exit::FAILURES, None)
            } else if uploaded + deleted + failed == 0 {
                out(&copy::up_to_date(&site));
                (exit::OK, None)
            } else {
                out(&copy::published(&site, *uploaded, *deleted, *bytes, purge));
                if *failed > 0 {
                    err(&copy::publish_failed_files(*failed));
                    (exit::FAILURES, Some(copy::scheduled_files_failed(*failed)))
                } else {
                    (exit::OK, None)
                }
            }
        }
        Err(e) => {
            log_not_run(ctx, &profile, source, e.text());
            (fail(e.exit_code(), e.text()), Some(e.text().to_owned()))
        }
    };
    if args.scheduled
        && let Some(why) = notice
    {
        let body = copy::scheduled_failed(&profile.name, &why);
        let _ = blocking(move || win::notify(copy::NOTIFY_TITLE, &body)).await;
    }
    code
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn names_are_found_in_any_case_and_ids_first() {
        let items = vec![
            ("a1".to_owned(), "Blog".to_owned()),
            ("b2".to_owned(), "blog".to_owned()),
            ("c3".to_owned(), "Café Été".to_owned()),
        ];
        fn id(item: &(String, String)) -> &str {
            &item.0
        }
        fn name(item: &(String, String)) -> &str {
            &item.1
        }
        assert!(matches!(find(&items, "BLOG", id, name), Found::Several(v) if v.len() == 2));
        assert!(matches!(find(&items, "B2", id, name), Found::One(item) if item.0 == "b2"));
        assert!(matches!(find(&items, "café été", id, name), Found::One(item) if item.0 == "c3"));
        assert!(matches!(find(&items, "nope", id, name), Found::Nothing));
    }

    #[test]
    fn tables_line_up_by_characters() {
        let rows = vec![
            vec!["Été".to_owned(), "Bunny".to_owned(), "x".to_owned()],
            vec!["media".to_owned(), "R2".to_owned(), "y".to_owned()],
        ];
        assert_eq!(table(&rows), ["Été    Bunny  x", "media  R2     y"]);
    }

    #[test]
    fn errors_of_the_site_map_to_exit_codes() {
        assert_eq!(
            code_of(&ProviderError::Auth {
                hint: httporter_core::AuthHint::Unknown
            }),
            exit::AUTH
        );
        assert_eq!(code_of(&ProviderError::NotFound), exit::CONFIG);
        assert_eq!(code_of(&ProviderError::ClockSkew), exit::FAILURES);
    }

    #[test]
    fn a_missing_local_path_says_where() {
        assert!(copy::local_missing(Path::new("C:\\nope")).contains("C:\\nope"));
    }
}
