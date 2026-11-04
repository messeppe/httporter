//! Publish: comparing the two panes' folders, the Publish sheet and its runs, the CDN purge, and
//! the profile pages with their history.
//!
//! The parts, one file each (each UI task owns its own):
//! - this file (T7): the [`Publisher`], shared by the window's callbacks and the tasks it starts;
//! - [`compare_view`] (T6): Compare in the main window;
//! - [`sheet_view`] and [`cdn`] (T7): the Publish sheet, its runs, the CDN key sheet and the
//!   delete alert's purge;
//! - [`profile_view`] (T8): the profile page, Edit Profile and the history;
//! - [`copy`]: the words of all of the above, one section per task.

// T1 skeleton: drop this once T6, T7 and T8 fill the bodies.
#![allow(dead_code, unused_variables)]

mod cdn;
mod compare_view;
mod copy;
mod profile_view;
mod sheet_view;

use std::sync::{Arc, Mutex};

use httporter_core::{CdnPurger, ProfileId, SiteRecord};
use httporter_transfer::HashCache;

use crate::AppWindow;
use crate::binding::Link;
use crate::session::Session;
use crate::transfers::Transfers;

/// Whether a site's CDN can be purged, and with what.
pub enum PurgerAnswer {
    Ready(Arc<dyn CdnPurger>),
    /// A Bunny site whose account key isn't kept yet: ask for it (`ask_cdn_key`).
    NeedKey,
    /// No purge for this site (S3, no pull zone, the key sheet cancelled), and why.
    Unavailable(String),
}

/// What the Publisher remembers between callbacks (T7).
#[derive(Default)]
struct State {}

/// Compare, publish and the profile pages. Shared as `Arc<Publisher>` by the window's callbacks
/// and the tasks they start.
pub struct Publisher {
    session: Arc<Session>,
    transfers: Arc<Transfers>,
    link: Link,
    /// Local digests, kept across compares.
    hashes: HashCache,
    state: Mutex<State>,
    /// Each view's own state, in its own file.
    compare: Mutex<compare_view::CompareState>,
    sheet: Mutex<sheet_view::SheetState>,
    profile: Mutex<profile_view::ProfileState>,
}

impl Publisher {
    pub fn new(session: Arc<Session>, transfers: Arc<Transfers>, link: Link) -> Arc<Self> {
        Arc::new(Self {
            session,
            transfers,
            link,
            hashes: HashCache::default(),
            state: Mutex::default(),
            compare: Mutex::default(),
            sheet: Mutex::default(),
            profile: Mutex::default(),
        })
    }

    /// Compares the profile's folders, then shows the Publish sheet.
    pub fn open_sheet(self: &Arc<Self>, profile: ProfileId) {
        todo!("T7")
    }

    /// The toolbar's Publish and Ctrl+P.
    pub fn publish_current(self: &Arc<Self>) {
        todo!("T7")
    }

    pub fn retry_failed(self: &Arc<Self>, history: i64) {
        todo!("T7")
    }

    pub fn retry_purge(self: &Arc<Self>, history: i64) {
        todo!("T7")
    }

    pub async fn purger_for(&self, site: &SiteRecord) -> PurgerAnswer {
        todo!("T7")
    }

    /// Opens the CDN key sheet for `site` (Edit Profile uses it too).
    pub fn ask_cdn_key(self: &Arc<Self>, site: SiteRecord) {
        todo!("T7")
    }
}

/// Wires the window's Compare, Publish and profile callbacks to `p`.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    compare_view::install(ui, p);
    sheet_view::install(ui, p);
    profile_view::install(ui, p);
}
