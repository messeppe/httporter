//! The profile page, Edit Profile and the publish history (T8).

use std::path::PathBuf;
use std::sync::Arc;

use httporter_core::{RemotePath, SiteRecord};

use super::Publisher;
use crate::AppWindow;

/// What the profile page remembers between callbacks.
#[derive(Default)]
pub(super) struct ProfileState {}

/// Wires the profile page and profile sheet callbacks. Runs at startup: must not start anything.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    // T8 fills this.
}

/// Opens the profile sheet for a new profile of `site`, publishing `local` to `remote`.
pub fn open_new(ui: &AppWindow, site: &SiteRecord, local: PathBuf, remote: RemotePath) {
    todo!("T8")
}
