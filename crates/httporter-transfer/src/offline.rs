//! When a site can't be reached or the disk is full.
//!
//! A lane goes offline after [`OFFLINE_AFTER`] network errors within [`OFFLINE_WINDOW`], or when
//! one job uses up its retries on network errors. While it is offline nothing new starts on it,
//! and a job that fails with a network error goes back to the queue as it was, without using up
//! an attempt. The site is probed every [`PROBE_EVERY`] with `StorageProvider::reachable`; the
//! first answer puts the lane back to work. The work in flight is left alone until the first
//! probe gets no answer either; then it goes back to the queue at once (reviews M1, N1), so a
//! job whose own requests keep failing on a site that answers never restarts the transfers
//! beside it.
//!
//! A download that finds its disk full stops every lane, with the drive and the download bytes
//! still to come on it, until the user resumes.

use std::collections::VecDeque;
use std::io;
use std::path::{Component, Path, Prefix};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::engine::Inner;

/// Network errors on one lane, within [`OFFLINE_WINDOW`], that take it offline.
pub(crate) const OFFLINE_AFTER: usize = 3;

/// See [`OFFLINE_AFTER`].
pub(crate) const OFFLINE_WINDOW: Duration = Duration::from_secs(10);

/// How often an offline site is asked whether it answers again.
pub(crate) const PROBE_EVERY: Duration = Duration::from_secs(5);

/// A job whose own network errors take its lane offline (or meet it offline) in this many
/// offline cycles fails with the network error instead of sending the lane offline once more
/// (ruling Q50): a site that answers the probe but never this job's requests would otherwise
/// hold the job forever. The user's Retry starts it again with none.
pub(crate) const MAX_STRIKES: u8 = 3;

/// The latest network errors on one lane.
#[derive(Debug, Default)]
pub(crate) struct NetworkErrors {
    recent: VecDeque<Instant>,
}

impl NetworkErrors {
    /// Notes an error at `now`. True when that makes [`OFFLINE_AFTER`] errors within
    /// [`OFFLINE_WINDOW`].
    pub(crate) fn record(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) > OFFLINE_WINDOW)
        {
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        self.recent.len() >= OFFLINE_AFTER
    }

    pub(crate) fn clear(&mut self) {
        self.recent.clear();
    }
}

/// Asks an offline lane's site every [`PROBE_EVERY`] whether it answers, until it does, the lane
/// is no longer offline (an edited site, say), or the engine stops. `cycle` is the lane's
/// offline cycle this probing is for.
///
/// When the first probe gets no answer either, the site is out of reach, and the work in flight
/// on the lane goes back to the queue ([`Inner::outage`], review N1).
pub(crate) async fn probe(inner: Arc<Inner>, lane: usize, cycle: u64) {
    let mut first = true;
    loop {
        tokio::time::sleep(PROBE_EVERY).await;
        let Some(provider) = inner.offline_provider(lane) else {
            return;
        };
        if provider.reachable().await.is_ok() {
            inner.back_online(lane);
            return;
        }
        if first {
            first = false;
            inner.outage(lane, cycle);
        }
    }
}

/// The disk is full: `ErrorKind::StorageFull`, or Windows' `ERROR_DISK_FULL` (112) or
/// `ERROR_HANDLE_DISK_FULL` (39).
pub(crate) fn is_disk_full(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::StorageFull || matches!(error.raw_os_error(), Some(112 | 39))
}

/// The drive `path` is on, as the user knows it: `D:`, or `\\server\share`. A path without a
/// drive gives its root (`\` or `/`), or nothing when it is relative.
pub(crate) fn drive_of(path: &Path) -> String {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                format!("{}:", char::from(letter).to_ascii_uppercase())
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
                r"\\{}\{}",
                server.to_string_lossy(),
                share.to_string_lossy()
            ),
            _ => prefix.as_os_str().to_string_lossy().into_owned(),
        },
        Some(Component::RootDir) => std::path::MAIN_SEPARATOR.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_network_errors_within_ten_seconds_go_offline() {
        let mut errors = NetworkErrors::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(!errors.record(at(0)));
        assert!(!errors.record(at(9)));
        // The first is 11 s old by now: two within the window.
        assert!(!errors.record(at(11)));
        assert!(errors.record(at(12)));
        errors.clear();
        assert!(!errors.record(at(12)));
        // Exactly 10 s apart still counts as within.
        assert!(!errors.record(at(22)));
        assert!(errors.record(at(22)));
    }

    #[test]
    fn a_full_disk_is_known_by_kind_or_by_windows_code() {
        assert!(is_disk_full(&io::Error::from(io::ErrorKind::StorageFull)));
        assert!(is_disk_full(&io::Error::from_raw_os_error(112)));
        assert!(is_disk_full(&io::Error::from_raw_os_error(39)));
        assert!(!is_disk_full(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(!is_disk_full(&io::Error::other("disk on fire")));
    }

    #[cfg(windows)]
    #[test]
    fn drives_as_the_user_knows_them() {
        assert_eq!(drive_of(Path::new(r"d:\downloads\a.bin")), "D:");
        assert_eq!(drive_of(Path::new(r"\\?\C:\very\long\path")), "C:");
        assert_eq!(
            drive_of(Path::new(r"\\nas\media\films\a.mkv")),
            r"\\nas\media"
        );
        assert_eq!(
            drive_of(Path::new(r"\\?\UNC\nas\media\a.mkv")),
            r"\\nas\media"
        );
        assert_eq!(drive_of(Path::new(r"\downloads\a.bin")), r"\");
        assert_eq!(drive_of(Path::new(r"relative\a.bin")), "");
    }
}
