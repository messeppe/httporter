//! Purge from CDN, now (T5): the toolbar's, the menu's and the inspector's purge, and the local
//! digests the inspector's "Matches local" reads through the Publisher's cache.
//!
//! As the delete alert's purge (`cdn.rs`), it finds the site's purger first (the CDN key sheet
//! asks for the key when none is kept), then waits until the engine's late checks for the site are
//! over (ruling Q55: a check may send a file again, and a purge must come after it).

use std::io;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use httporter_core::{
    ActivityEvent, ActivityKind, CdnPurger, CdnZone, PURGE_URL_LIMIT, PurgeMode, PurgeResult,
    RemotePath, SiteRecord, purge_urls, run_purge,
};
use httporter_transfer::{FileHashes, PublishPhase};

use super::{Publisher, cdn, copy};
use crate::comfort::toast;
use crate::manage::activity_view;

impl Publisher {
    /// Waits until `pending_checks_for(site) == 0` (Q55), then purges `purge_urls(files)` and
    /// `https://{host}/{folder}/*` for each folder; a missing key opens the CDN key sheet; logs a
    /// Purge event. More URLs than [`PURGE_URL_LIMIT`] purge the whole zone instead, as a publish
    /// does. The status line says how far it is ("Purging CDN… n of m"), then a toast how it went.
    pub fn purge_now(
        self: &Arc<Self>,
        site: SiteRecord,
        files: Vec<RemotePath>,
        folders: Vec<RemotePath>,
    ) {
        if files.is_empty() && folders.is_empty() {
            self.link.status(copy::PURGE_NOTHING.to_owned());
            return;
        }
        let this = self.clone();
        self.link.rt().spawn(async move {
            let result = match cdn::purger_or_key(&this, &site).await {
                Err(why) => PurgeResult::Unavailable(why),
                Ok(purger) => {
                    cdn::settled(this.transfers.engine(), &site.id).await;
                    purge(&this, &*purger, &site, &files, &folders).await
                }
            };
            let message = copy::purge_done(&result);
            let ok = matches!(result, PurgeResult::Urls(_) | PurgeResult::Zone);
            activity_view::log(ActivityEvent {
                at: Utc::now(),
                kind: ActivityKind::Purge,
                site: site.name.clone(),
                path: files
                    .first()
                    .or(folders.first())
                    .map(|path| format!("/{}", path.as_str()))
                    .unwrap_or_default(),
                ok,
                detail: copy::purged_what(files.len(), folders.len(), &message),
                bytes: 0,
            });
            toast::post_after_progress(&this.link, toast::purge_kind(&result), message);
        });
    }

    /// The digests of the local file `path`, from the cache the compares share (the inspector's
    /// "Matches local"). Reads the file when it changed since: call it on a blocking thread.
    pub(crate) fn local_hashes(&self, path: &Path) -> io::Result<FileHashes> {
        self.hashes.get_or_hash(path)
    }
}

/// `https://{host}/{folder}/*` for every hostname of `zones` (a whole folder, at Bunny's edge).
fn folder_urls(zones: &[CdnZone], folders: &[RemotePath]) -> Vec<String> {
    let mut urls = Vec::new();
    for host in zones.iter().flat_map(|zone| &zone.hostnames) {
        let host = host.trim();
        let host = host
            .strip_prefix("https://")
            .or_else(|| host.strip_prefix("http://"))
            .unwrap_or(host)
            .trim_end_matches('/');
        if host.is_empty() {
            continue;
        }
        for folder in folders {
            urls.push(format!("https://{host}/{}*", folder.to_url_path(true)));
        }
    }
    urls
}

/// Purges the URLs of `files` and `folders` one by one, going on past failures; the whole zone
/// when they are too many.
async fn purge(
    p: &Publisher,
    purger: &dyn CdnPurger,
    site: &SiteRecord,
    files: &[RemotePath],
    folders: &[RemotePath],
) -> PurgeResult {
    let link = p.link.clone();
    let name = site.name.clone();
    let progress = move |done: usize, total: usize| {
        link.status(copy::phase(&name, PublishPhase::Purging { done, total }));
    };
    let zones = purger.zones();
    let mut urls = purge_urls(zones, files);
    urls.extend(folder_urls(zones, folders));
    urls.sort();
    urls.dedup();
    if urls.is_empty() || urls.len() > PURGE_URL_LIMIT {
        // No hostnames to name (run_purge says so), or more URLs than are worth purging one by one.
        return run_purge(purger, PurgeMode::Zone, &[], &progress).await;
    }
    let total = urls.len();
    progress(0, total);
    let mut failed = 0;
    let mut first = None;
    for (i, url) in urls.iter().enumerate() {
        if let Err(error) = purger.purge_url(url).await {
            failed += 1;
            first.get_or_insert(error);
        }
        progress(i + 1, total);
    }
    match first {
        Some(error) => PurgeResult::Failed(httporter_core::copy::purge_failed(
            failed,
            total,
            &error.to_string(),
        )),
        None => PurgeResult::Urls(total as u64),
    }
}
