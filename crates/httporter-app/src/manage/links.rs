//! Copy Public Link and Copy Path (T5), the inspector's copy buttons, and the links the inspector
//! shows: a file's public URL (from the site's public base URL, else its pull zones) and its
//! storage URL.
//!
//! Copy Public Link copies one URL per selected item, on the first hostname (custom domains come
//! before the zone's own `*.b-cdn.net`); the inspector names the others. On This PC, Copy Public
//! Link (Ctrl+Shift+C) and Copy Path copy the files' paths, as Explorer's Copy as Path does.
//!
//! A copied link is told in a toast (I5); a copied path, in the status line.

use std::sync::Arc;

use httporter_core::{ProviderKind, RemotePath, SiteRecord, encode_segment};
use httporter_platform::win::set_clipboard_text;
use slint::ComponentHandle;

use super::inspector_view::{open_site, selected_names};
use super::{Manager, Side, copy};
use crate::AppWindow;
use crate::comfort::toast::{self, ToastKind};
use crate::publish::PurgerAnswer;
use crate::site_hub::s3_endpoint;

/// Where a site's files are served from: the start of their public URLs (`https://host/`), the
/// first one the one copied, and what they are ("Public · Pull Zone 7654321").
pub(super) struct Public {
    prefixes: Vec<String>,
    pub note: String,
}

impl Public {
    /// The public URLs of `path` (a folder's with a trailing `/`), one per prefix.
    pub fn urls(&self, path: &RemotePath, folder: bool) -> Vec<String> {
        let tail = path.to_url_path(folder);
        self.prefixes
            .iter()
            .map(|prefix| format!("{prefix}{tail}"))
            .collect()
    }
}

/// A pull zone hostname as it may be stored (with a scheme, a trailing `/`), bare.
fn bare_host(host: &str) -> &str {
    let host = host.trim();
    host.strip_prefix("https://")
        .or_else(|| host.strip_prefix("http://"))
        .unwrap_or(host)
        .trim_end_matches('/')
}

/// Where `site`'s files are served from: its public base URL (Settings › Sites) when it has one;
/// else, for Bunny, the hostnames of its pull zones (the stored CDN link, or the zones a kept
/// Bunny account finds); else none, and the note says how to add one.
pub(super) async fn public_of(m: &Manager, site: &SiteRecord) -> Public {
    let extras = m.session.site_extras(&site.id);
    if let Some(base) = extras
        .public_base_url
        .as_deref()
        .map(str::trim)
        .filter(|base| !base.is_empty())
    {
        return Public {
            prefixes: vec![format!("{}/", base.trim_end_matches('/'))],
            note: copy::PUBLIC_FROM_SETTINGS.to_owned(),
        };
    }
    if site.provider == ProviderKind::Bunny {
        let zones = match m.session.cdn_link(site.id.clone()).await {
            Some(link) => link.zones,
            None => match m.publisher.purger_for(site).await {
                PurgerAnswer::Ready(purger) => purger.zones().to_vec(),
                _ => Vec::new(),
            },
        };
        let mut hosts: Vec<(u64, String)> = zones
            .iter()
            .flat_map(|zone| {
                zone.hostnames
                    .iter()
                    .map(move |host| (zone.id, bare_host(host).to_owned()))
            })
            .filter(|(_, host)| !host.is_empty())
            .collect();
        // Custom domains first (a stable sort keeps Bunny's order among each kind).
        hosts.sort_by_key(|(_, host)| host.to_ascii_lowercase().ends_with(".b-cdn.net"));
        if let Some((zone, _)) = hosts.first() {
            return Public {
                note: copy::public_zone_note(*zone, hosts.len() - 1),
                prefixes: hosts
                    .iter()
                    .map(|(_, host)| format!("https://{host}/"))
                    .collect(),
            };
        }
    }
    Public {
        prefixes: Vec::new(),
        note: copy::NO_PUBLIC_URL.to_owned(),
    }
}

/// The storage API's URL of `path` of `site` (a folder's with a trailing `/`); empty for an S3
/// site whose endpoint can't be read.
pub(super) fn storage_url(site: &SiteRecord, path: &RemotePath, folder: bool) -> String {
    match site.provider {
        ProviderKind::Bunny => format!(
            "https://{}/{}/{}",
            bare_host(&site.host),
            encode_segment(&site.container),
            path.to_url_path(folder)
        ),
        ProviderKind::S3 => match s3_endpoint(site) {
            Ok((endpoint, _)) => {
                let mut url =
                    httporter_s3::object_url(&endpoint, &site.container, path).to_string();
                if folder && !path.is_root() {
                    url.push('/');
                }
                url
            }
            Err(_) => String::new(),
        },
    }
}

/// What the storage link is: it opens only with the site's secret.
pub(super) fn storage_note(site: &SiteRecord) -> &'static str {
    match site.provider {
        ProviderKind::Bunny => copy::STORAGE_NOTE_BUNNY,
        ProviderKind::S3 => copy::STORAGE_NOTE_S3,
    }
}

/// How a copy says it is done: a link in a toast (I5), a path in the status line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Told {
    Status,
    Toast,
}

/// Puts `text` on the clipboard (on a blocking thread), then says `done` as `told` says. A
/// failure goes to the status line.
fn to_clipboard(m: &Manager, text: String, done: String, told: Told) {
    let link = m.link.clone();
    m.link
        .rt()
        .spawn_blocking(move || match set_clipboard_text(&text) {
            Ok(()) if told == Told::Toast => toast::post(link.ui(), ToastKind::Done, done),
            Ok(()) => link.status(done),
            Err(why) => link.status(copy::not_copied(&why)),
        });
}

/// The paths of the This PC selection, one per line.
fn copy_local_paths(m: &Manager, ui: &AppWindow) {
    let paths = m.session.local_paths(&selected_names(ui, Side::Local));
    if paths.is_empty() {
        m.link.status(copy::SELECT_FIRST.to_owned());
        return;
    }
    let text: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    to_clipboard(
        m,
        text.join("\r\n"),
        copy::copied_paths(paths.len()),
        Told::Status,
    );
}

/// Copy Public Link (the menu, Ctrl+Shift+C) for `pane` (0 This PC, 1 the cloud pane).
fn copy_link(m: &Arc<Manager>, ui: &AppWindow, pane: i32) {
    if pane == 0 {
        copy_local_paths(m, ui);
        return;
    }
    let entries = m.session.remote_entries(&selected_names(ui, Side::Remote));
    let Some(site) = open_site(m, ui).filter(|_| !entries.is_empty()) else {
        m.link.status(copy::SELECT_FIRST.to_owned());
        return;
    };
    let this = m.clone();
    m.link.rt().spawn(async move {
        let public = public_of(&this, &site).await;
        let urls: Vec<String> = entries
            .iter()
            .filter_map(|entry| public.urls(&entry.path, entry.is_folder).into_iter().next())
            .collect();
        if urls.is_empty() {
            this.link.status(copy::no_public_url(&site.name));
            return;
        }
        let done = copy::copied_links(urls.len());
        to_clipboard(&this, urls.join("\r\n"), done, Told::Toast);
    });
}

/// Copy Path for `pane`: the cloud selection's paths from the site's root (`/a/b.jpg`, a folder's
/// with a trailing `/`), or the This PC selection's.
fn copy_path(m: &Manager, ui: &AppWindow, pane: i32) {
    if pane == 0 {
        copy_local_paths(m, ui);
        return;
    }
    let entries = m.session.remote_entries(&selected_names(ui, Side::Remote));
    if entries.is_empty() {
        m.link.status(copy::SELECT_FIRST.to_owned());
        return;
    }
    let paths: Vec<String> = entries
        .iter()
        .map(|entry| {
            let mut path = format!("/{}", entry.path.as_str());
            if entry.is_folder && !entry.path.is_root() {
                path.push('/');
            }
            path
        })
        .collect();
    to_clipboard(
        m,
        paths.join("\r\n"),
        copy::copied_paths(paths.len()),
        Told::Status,
    );
}

/// The inspector's copy buttons: 0 the public link, 1 the storage link, as the inspector shows
/// them.
fn copy_shown(m: &Manager, ui: &AppWindow, which: i32) {
    let data = ui.get_inspector();
    let url = if which == 0 {
        data.public_url
    } else {
        data.storage_url
    };
    if !url.is_empty() {
        to_clipboard(m, url.to_string(), copy::copied_links(1), Told::Toast);
    }
}

/// Wires Copy Public Link, Copy Path and the inspector's copy buttons of `ui` to `m`.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_copy_link({
        let (ui, m) = (ui.as_weak(), m.clone());
        move |pane| {
            if let Some(ui) = ui.upgrade() {
                copy_link(&m, &ui, pane);
            }
        }
    });
    ui.on_copy_path({
        let (ui, m) = (ui.as_weak(), m.clone());
        move |pane| {
            if let Some(ui) = ui.upgrade() {
                copy_path(&m, &ui, pane);
            }
        }
    });
    ui.on_inspector_copy({
        let (ui, m) = (ui.as_weak(), m.clone());
        move |which| {
            if let Some(ui) = ui.upgrade() {
                copy_shown(&m, &ui, which);
            }
        }
    });
}
