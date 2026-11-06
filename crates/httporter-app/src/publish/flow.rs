// SPDX-License-Identifier: MIT OR Apache-2.0

//! The steps of a publish that the Publish sheet ([`super::sheet_view`]) and the headless publish
//! ([`super::headless`]: the CLI, the scheduled publish and the watch folder) share, so the two
//! compare, plan, guard, purge and record a publish alike:
//!
//! - [`compare`]: the profile's folder against the site, with the site's pull zones;
//! - [`purge_for_site`] and [`zone_instead`]: the purge a publish starts from, and the whole zone
//!   instead of changed files past [`PURGE_URL_LIMIT`] URLs;
//! - [`guard_trips`]: the delete guard; [`secrets_uploaded`]: the secrets check;
//! - [`lock_profile`]: one publish per profile at a time, across processes;
//! - [`execute`]: the run itself (uploads, deletes, purge) and its history record;
//! - [`record`]: the history row and the activity log entry.
//!
//! What only the sheet does (drawing the plan, the CDN key sheet, Retry) stays in `sheet_view`;
//! what only a run without a window does (never asking, the guard holding deletes back, the
//! progress lines) stays in `headless`.

use std::sync::Arc;
use std::time::Duration;

use httporter_core::{
    ActivityKind, CdnPurger, CdnZone, CompareStatus, HistoryRecord, HistorySource, PURGE_URL_LIMIT,
    ProfileId, ProfileRecord, ProviderError, ProviderKind, PublishResult, PurgeMode, PurgeResult,
    SiteRecord, SkipRules, StorageProvider, purge_urls,
};
use httporter_platform::win::ProfileLock;
use httporter_transfer::{
    BatchTarget, CompareError, CompareMode, CompareResult, Engine, HashCache, PublishPhase,
    PublishPlan, ScanProgress, compare_trees, run_publish,
};

use super::copy;
use crate::copy::{self as app_copy, Attempt};
use crate::session::{Session, blocking};

/// The purge a publish to `site` starts from: `wanted` on Bunny; none on S3, which has no CDN
/// to purge.
pub(crate) fn purge_for_site(site: &SiteRecord, wanted: PurgeMode) -> PurgeMode {
    match site.provider {
        ProviderKind::Bunny => wanted,
        ProviderKind::S3 => PurgeMode::Off,
    }
}

/// A compare of a profile's folders, and the site's pull zones when it has a CDN link (Bunny).
pub(crate) struct Compared {
    pub result: CompareResult,
    pub zones: Option<Vec<CdnZone>>,
}

/// Why a compare failed: the words the sheet shows, and the site's error when the site answered
/// with one (a headless run tells a refused sign-in by it).
pub(crate) struct CompareFailed {
    pub text: String,
    pub remote: Option<ProviderError>,
}

/// Compares `profile`'s folder with its folder on `site` through `provider`, as the Transfers
/// settings say (checksums or not) and with the profile's skip rules; then reads the site's pull
/// zones. `progress` hears how far the scan is.
pub(crate) async fn compare(
    session: &Session,
    hashes: &HashCache,
    provider: &dyn StorageProvider,
    site: &SiteRecord,
    profile: &ProfileRecord,
    progress: &(dyn Fn(ScanProgress) + Send + Sync),
) -> Result<Compared, CompareFailed> {
    let mode = CompareMode::for_site(site.provider, session.settings().verify_checksums);
    let skip = SkipRules::new(&profile.skip_rules);
    let found = compare_trees(
        provider,
        &profile.local_dir,
        &profile.remote_dir,
        &skip,
        mode,
        hashes,
        progress,
    )
    .await;
    match found {
        Ok(result) => {
            let zones = match site.provider {
                ProviderKind::Bunny => session.cdn_link(site.id.clone()).await.map(|l| l.zones),
                ProviderKind::S3 => None,
            };
            Ok(Compared { result, zones })
        }
        Err(CompareError::Local { path, message }) => Err(CompareFailed {
            text: copy::compare_local_failed(&path, &message),
            remote: None,
        }),
        Err(CompareError::Remote(e)) => Err(CompareFailed {
            text: app_copy::describe_for(
                site.provider,
                &e,
                &site.container,
                &site.host,
                Attempt::List(&profile.remote_dir),
            ),
            remote: Some(e),
        }),
    }
}

/// How many URLs purging `plan`'s changed files would be, when `purge` is Changed and that is
/// more than [`PURGE_URL_LIMIT`]: one by one is slow past it, so the whole zone is purged instead.
/// `zones` are the site's pull zones; with no link yet, the count is for the site's host.
pub(crate) fn zone_instead(
    site: &SiteRecord,
    purge: PurgeMode,
    zones: Option<Vec<CdnZone>>,
    plan: &PublishPlan,
) -> Option<usize> {
    if purge != PurgeMode::Changed {
        return None;
    }
    let zones = zones.unwrap_or_else(|| {
        vec![CdnZone {
            id: 0,
            name: String::new(),
            hostnames: vec![site.host.clone()],
        }]
    });
    let urls = purge_urls(&zones, &plan.touched_paths()).len();
    (urls > PURGE_URL_LIMIT).then_some(urls)
}

/// The delete guard trips for `plan` as it is now: it deletes, and more than `profile`'s guard
/// lets through (the sheet asks for Review Deletions; a headless run holds the deletes back).
pub(crate) fn guard_trips(profile: &ProfileRecord, plan: &PublishPlan) -> bool {
    let deletes = plan.delete_count();
    deletes > 0 && profile.guard.trips(deletes, plan.remote_files)
}

/// The files of `secrets` (those of the compare that look like secrets) that `plan` would upload.
pub(crate) fn secrets_uploaded(secrets: &[String], plan: &PublishPlan) -> Vec<String> {
    secrets
        .iter()
        .filter(|rel| {
            plan.items.iter().any(|item| {
                item.rel == **rel && item.include && item.status != CompareStatus::RemoteOnly
            })
        })
        .cloned()
        .collect()
}

/// `profile`'s publish lock, unless another publish of it (in this window, another HTTPorter or
/// the command line) holds it for longer than `wait`. Waits on a blocking thread.
pub(crate) async fn lock_profile(profile: &ProfileId, wait: Duration) -> Option<ProfileLock> {
    let id = profile.clone();
    blocking(move || {
        if wait.is_zero() {
            ProfileLock::try_acquire(&id)
        } else {
            ProfileLock::acquire_wait(&id, wait)
        }
    })
    .await
}

/// What a publish runs.
pub(crate) struct Run {
    pub profile: ProfileRecord,
    pub site: SiteRecord,
    /// Where its batches go.
    pub target: BatchTarget,
    pub plan: PublishPlan,
    pub purge: PurgeMode,
    /// Who started it, for the history row.
    pub source: HistorySource,
}

/// The CDN purger a run purges with: `Ok(None)` when the purge is off, `Err` saying why there is
/// none when it is on.
pub(crate) type Purger = Result<Option<Arc<dyn CdnPurger>>, String>;

/// Runs `run` on `engine`: the profile's Cache-Control for its uploads (S3; Bunny sets caching in
/// the pull zone), then [`run_publish`] purging with `purger` (unpurged without one, which the
/// history says), and returns its history record (`run.source`'s, not stored yet). `Err` is what
/// to say when it couldn't start (the journal couldn't record its batch).
pub(crate) async fn execute(
    engine: &Engine,
    session: &Session,
    run: &Run,
    purger: Purger,
    on_phase: &(dyn Fn(PublishPhase) + Send + Sync),
) -> Result<HistoryRecord, String> {
    let Run {
        profile,
        site,
        target,
        plan,
        purge,
        source,
    } = run;
    if site.provider == ProviderKind::S3 {
        engine.set_cache_control(&site.id, &profile.remote_dir, profile.cache_control.clone());
    }
    let (purger, unavailable) = match purger {
        Ok(purger) => (purger, None),
        Err(why) => (None, Some(why)),
    };
    let mode = if purger.is_some() {
        *purge
    } else {
        PurgeMode::Off
    };
    let mut outcome = run_publish(engine, target.clone(), plan, mode, purger, on_phase)
        .await
        .map_err(|e| copy::publish_not_started(&e.0))?;
    if let Some(why) = unavailable {
        outcome.purge = PurgeResult::Unavailable(why);
    }
    let verified = session.settings().verify_checksums;
    let mut record = outcome.into_record(profile.id.clone(), plan, verified);
    record.source = *source;
    Ok(record)
}

/// Stores `record` of `run` in the history and logs it in the activity log (a run without the
/// window says who started it). Returns the words for how it went, and whether the history row
/// was saved.
pub(crate) async fn record(
    session: &Session,
    run: &Run,
    record: HistoryRecord,
) -> (String, Result<i64, String>) {
    let message = copy::published(&run.site.name, &record);
    let detail = copy::from_source(record.source, &message);
    let (ok, bytes) = (record.result == PublishResult::Ok, record.bytes);
    let saved = session.add_history(record).await;
    crate::manage::activity_view::event(
        ActivityKind::Publish,
        &run.site.name,
        &format!("/{}", run.profile.remote_dir.as_str()),
        ok,
        detail,
        bytes,
    );
    (message, saved)
}
