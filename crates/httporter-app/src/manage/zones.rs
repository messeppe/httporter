// SPDX-License-Identifier: MIT OR Apache-2.0

//! Zone and bucket tools (§8.8.4–8.8.5, I6, T5):
//!
//! - **Create New Zone…** (the Add Site sheet's account-key sign-in): a Bunny storage zone and, if
//!   asked, a pull zone in front of it (`BunnyAccount::create_storage_zone`, `create_pull_zone`).
//!   Both names are checked first, here and with Bunny (`checkavailability`), so a name problem
//!   goes under its field and nothing is created. The zone becomes a site the way Find Zones adds
//!   one (`Session::add_zones`: its password into Credential Manager only), and with "Keep it…"
//!   the account key is kept (once: an account already holding the same key is reused) and the
//!   pull zone is stored as the site's CDN link. The new site opens.
//! - **Create Bucket…** (the bucket picker): `S3Storage::create_bucket` with the sheet's endpoint
//!   and key; the name goes into the bucket field.
//! - **Settings › Sites › Zone**: the 404 page, "Return status 200 for the 404 page" and the
//!   replication regions (which can only be added), through a kept account key: the site's CDN
//!   link's account first, then every kept Bunny account, whichever finds the zone by name.
//! - **Settings › Sites › Bucket**: the CORS rule (the first one; the others are kept as they are)
//!   and the static website, where the preset offers them (`httporter_s3::PresetFeatures`); a
//!   part it doesn't offer is replaced by a line saying so.
//!
//! The account key and the S3 secret stay in this file's state only while their sheet is up, and
//! are never put in a window property or logged. A read-only site's zone and bucket are shown but
//! not changed. Tests use wiremock (the provider crates): building HTTPorter never creates a real
//! zone, pull zone or bucket.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::Utc;
use httporter_bunny::{
    BunnyAccount, NewZone, REGIONS, ZoneRef, ZoneSettings, pull_zone_name_problem,
    zone_name_problem,
};
use httporter_core::{
    AccountId, CdnLink, CdnZone, ProviderError, ProviderKind, SiteId, SiteRecord,
};
use httporter_platform::SiteSecret;
use httporter_s3::{
    CORS_METHODS, CorsRule, Credentials, NOT_OFFERED, PresetFeatures, S3Config, S3Storage, Website,
    bucket_name_problem,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{Manager, copy};
use crate::add_site::{SiteForm, field, s3_host, validate};
use crate::comfort::toast::{self, ToastKind};
use crate::session::{Session, blocking, hub_error_text};
use crate::site_hub::{read_secret, s3_endpoint};
use crate::{
    AppWindow, BucketSettingsData, NewZoneDraft, SettingsWindow, SiteDraft, ZoneSettingsData,
};

/// Create New Zone…: the account key typed in Add Site, and whether it is kept afterwards. No
/// `Debug`: it holds the key.
struct ZoneAsk {
    key: String,
    keep: bool,
}

/// A zone on show in Settings › Sites, as read from Bunny.
#[derive(Clone)]
struct ShownZone {
    site: SiteId,
    /// The kept account whose key reaches the zone.
    account: AccountId,
    zone_id: u64,
    /// The main region's code (lower case).
    main: String,
    settings: ZoneSettings,
}

/// A bucket on show in Settings › Sites, as read from the service.
#[derive(Clone)]
struct ShownBucket {
    site: SiteId,
    service: &'static str,
    cors_shown: bool,
    website_shown: bool,
    cors: Vec<CorsRule>,
    website: Option<Website>,
}

/// What this file remembers between callbacks (UI thread and tasks).
struct State {
    zone_ask: Option<ZoneAsk>,
    /// Create Bucket…: the Add Site sheet's S3 form, with its secret.
    bucket_ask: Option<SiteForm>,
    /// Goes up whenever a sheet opens or closes, so that an answer for one that has gone is
    /// dropped.
    run: u64,
    zone: Option<ShownZone>,
    bucket: Option<ShownBucket>,
}

static STATE: Mutex<State> = Mutex::new(State {
    zone_ask: None,
    bucket_ask: None,
    run: 0,
    zone: None,
    bucket: None,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The city of a Bunny region code ("Singapore"), or the code in capitals for one not known.
fn region_name(code: &str) -> String {
    REGIONS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(code))
        .map_or_else(|| code.to_ascii_uppercase(), |(_, city)| (*city).to_owned())
}

fn bools(values: Vec<bool>) -> ModelRc<bool> {
    ModelRc::new(VecModel::from(values))
}

fn text(value: &str) -> SharedString {
    value.into()
}

// ---- Create New Zone… and Create Bucket… ----

/// Wires Create New Zone… and Create Bucket… of `ui` to `m`.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_add_site_create_zone({
        let m = m.clone();
        move |key, keep| open_zone_sheet(&m, key.trim().to_owned(), keep)
    });
    ui.on_create_zone({
        let m = m.clone();
        move |draft| create_zone(&m, &draft)
    });
    ui.on_create_zone_cancel(|| {
        let mut state = state();
        state.zone_ask = None;
        state.run += 1;
    });
    ui.on_add_site_create_bucket({
        let m = m.clone();
        move |draft, secret| open_bucket_sheet(&m, &draft, &secret)
    });
    ui.on_create_bucket({
        let m = m.clone();
        move |name| create_bucket(&m, name.trim())
    });
    ui.on_create_bucket_cancel(|| {
        let mut state = state();
        state.bucket_ask = None;
        state.run += 1;
    });
}

/// The window opened Create New Zone…: keep the key for it, and size the replication ticks to
/// the region list (or they would do nothing).
fn open_zone_sheet(m: &Manager, key: String, keep: bool) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    {
        let mut state = state();
        state.zone_ask = Some(ZoneAsk { key, keep });
        state.run += 1;
    }
    ui.set_create_zone_replication(bools(vec![false; REGIONS.len()]));
    ui.set_create_zone_busy(false);
    show_zone_errors(&ui, None, None, None);
}

fn show_zone_errors(ui: &AppWindow, name: Option<&str>, pull: Option<&str>, other: Option<&str>) {
    ui.set_create_zone_name_error(text(name.unwrap_or_default()));
    ui.set_create_zone_pull_error(text(pull.unwrap_or_default()));
    ui.set_create_zone_error(text(other.unwrap_or_default()));
}

/// Create Zone: checks the names, then creates the zone (and the pull zone) on a task.
fn create_zone(m: &Arc<Manager>, draft: &NewZoneDraft) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    if ui.get_create_zone_busy() {
        return;
    }
    let name = draft.name.trim().to_owned();
    let main = usize::try_from(draft.region_index)
        .ok()
        .filter(|i| *i < REGIONS.len());
    let replication: Vec<String> = draft
        .replication
        .iter()
        .enumerate()
        .filter(|(i, on)| *on && Some(*i) != main)
        .filter_map(|(i, _)| REGIONS.get(i).map(|(code, _)| (*code).to_owned()))
        .collect();
    let pull_zone = draft.pull_zone.then(|| {
        let typed = draft.pull_zone_name.trim();
        if typed.is_empty() {
            name.clone()
        } else {
            typed.to_owned()
        }
    });
    let name_error = zone_name_problem(&name).map(copy::zone_name_problem);
    let pull_error = pull_zone
        .as_deref()
        .and_then(pull_zone_name_problem)
        .map(copy::zone_name_problem);
    let region_error = main.is_none().then_some(crate::copy::CHOOSE_REGION);
    show_zone_errors(
        &ui,
        name_error.as_deref(),
        pull_error.as_deref(),
        region_error,
    );
    let Some(main) = main else { return };
    if name_error.is_some() || pull_error.is_some() {
        return;
    }
    let (ask, run) = {
        let state = state();
        let ask = state
            .zone_ask
            .as_ref()
            .map(|ask| (ask.key.clone(), ask.keep));
        (ask, state.run)
    };
    let Some((key, keep)) = ask else {
        show_zone_errors(&ui, None, None, Some(copy::ZONE_SHEET_GONE));
        return;
    };
    let new = NewZone {
        name,
        region: REGIONS[main].0.to_owned(),
        replication,
        pull_zone,
    };
    ui.set_create_zone_busy(true);
    let task = m.clone();
    m.link.rt().spawn(async move {
        let made = make_zone(&task.session, key, keep, new).await;
        let m = task.clone();
        let _ = task
            .link
            .ui()
            .upgrade_in_event_loop(move |ui| zone_made(&ui, &m, run, made));
    });
}

/// How Create Zone ended.
enum ZoneMade {
    NameTaken,
    PullZoneTaken,
    /// Nothing was created.
    Failed(String),
    /// The zone is on Bunny, but not a site.
    NotAdded(String),
    /// The zone is a site; `warning` says what of the rest didn't happen.
    Added {
        site: SiteId,
        name: String,
        warning: Option<String>,
    },
}

/// Creates the zone and its pull zone with `key`, and adds the zone as a site.
async fn make_zone(session: &Arc<Session>, key: String, keep: bool, new: NewZone) -> ZoneMade {
    let settings = session.settings();
    let made = {
        let key = key.clone();
        // Making the client sets up TLS, which takes a while.
        blocking(move || BunnyAccount::with_settings(key, &settings)).await
    };
    let account = match made {
        Ok(account) => account,
        Err(e) => return ZoneMade::Failed(copy::zone_tool_failed(&e)),
    };
    match account.zone_name_available(&new.name).await {
        Ok(true) => {}
        Ok(false) => return ZoneMade::NameTaken,
        Err(e) => return ZoneMade::Failed(copy::zone_tool_failed(&e)),
    }
    if let Some(pull_zone) = &new.pull_zone {
        match account.pull_zone_name_available(pull_zone).await {
            Ok(true) => {}
            Ok(false) => return ZoneMade::PullZoneTaken,
            Err(e) => return ZoneMade::Failed(copy::zone_tool_failed(&e)),
        }
    }
    let mut zone = match account.create_storage_zone(&new).await {
        Ok(zone) => zone,
        Err(e) => return ZoneMade::Failed(copy::zone_tool_failed(&e)),
    };
    let mut warning = None;
    if let Some(pull_zone) = &new.pull_zone {
        match account.create_pull_zone(pull_zone, zone.id).await {
            Ok(pull_zone) => zone.pull_zones.push(pull_zone),
            Err(e) => {
                warning = Some(copy::zone_created_without_pull_zone(
                    &zone.name,
                    &copy::zone_tool_failed(&e),
                ));
            }
        }
    }

    // A site the way Find Zones adds one; "Keep it…" keeps the key, once.
    let kept = if keep {
        kept_account(session, &key).await
    } else {
        None
    };
    let keep_key = (keep && kept.is_none()).then(|| key.clone());
    let name = zone.name.clone();
    let pull_zones = zone.pull_zones.clone();
    let site = match session.add_zones(vec![zone], keep_key).await {
        Ok(sites) => sites.into_iter().next().map(|site| site.id),
        // The site may have been saved and only the key not kept: the error says which.
        Err(why) => match saved_zone(session, &name) {
            Some(site) => {
                warning.get_or_insert(why);
                Some(site)
            }
            None => return ZoneMade::NotAdded(copy::zone_not_added(&name, &why)),
        },
    };
    let Some(site) = site else {
        return ZoneMade::NotAdded(copy::zone_not_added(&name, copy::ZONE_NOT_LISTED));
    };

    // The pull zone is the site's CDN, purged with the kept key.
    if keep && !pull_zones.is_empty() {
        let account = match kept {
            Some(account) => Some(account),
            None => kept_account(session, &key).await,
        };
        if let Some(account) = account {
            let link = CdnLink {
                account,
                zones: pull_zones
                    .iter()
                    .map(|pull_zone| CdnZone {
                        id: pull_zone.id,
                        name: pull_zone.name.clone(),
                        hostnames: pull_zone.hostnames.clone(),
                    })
                    .collect(),
                checked_at: Utc::now(),
            };
            if let Err(why) = session.save_cdn_link(site.clone(), link, None).await {
                warning.get_or_insert(copy::zone_cdn_not_linked(&name, why));
            }
        }
    }
    ZoneMade::Added {
        site,
        name,
        warning,
    }
}

/// The kept Bunny account whose key is `key`, if any.
async fn kept_account(session: &Session, key: &str) -> Option<AccountId> {
    for account in session.bunny_accounts().await {
        let kept = session.account_key(account.id.clone()).await;
        if kept.as_deref().map(str::trim) == Some(key) {
            return Some(account.id);
        }
    }
    None
}

/// The saved Bunny site of zone `name`.
fn saved_zone(session: &Session, name: &str) -> Option<SiteId> {
    session
        .sites()
        .into_iter()
        .find(|site| site.provider == ProviderKind::Bunny && site.container == name)
        .map(|site| site.id)
}

/// Shows how Create Zone ended (UI thread). A zone that became a site closes both sheets, joins
/// the sidebar and opens.
fn zone_made(ui: &AppWindow, m: &Arc<Manager>, run: u64, made: ZoneMade) {
    let latest = state().run == run;
    match made {
        ZoneMade::Added {
            site,
            name,
            warning,
        } => {
            {
                let mut state = state();
                state.zone_ask = None;
                state.run += 1;
            }
            ui.set_create_zone_busy(false);
            ui.set_create_zone_open(false);
            if ui.get_add_site_open() {
                // The Add Site sheet is done too: it forgets what it knew, as on its Cancel.
                ui.set_add_site_open(false);
                ui.invoke_add_site_cancel();
            }
            m.link.show_remote(m.session.remote_view());
            select_site(ui, &m.session, &site);
            match warning {
                Some(warning) => toast::show(ui, ToastKind::Warning, warning, None),
                None => toast::show(ui, ToastKind::Done, copy::zone_created(&name), None),
            }
        }
        ZoneMade::NotAdded(why) => {
            m.link.show_remote(m.session.remote_view());
            if latest {
                ui.set_create_zone_busy(false);
                show_zone_errors(ui, None, None, Some(&why));
            } else {
                toast::show(ui, ToastKind::Warning, why, None);
            }
        }
        _ if !latest => {}
        ZoneMade::NameTaken => {
            ui.set_create_zone_busy(false);
            show_zone_errors(ui, Some(copy::ZONE_NAME_TAKEN), None, None);
        }
        ZoneMade::PullZoneTaken => {
            ui.set_create_zone_busy(false);
            show_zone_errors(ui, None, Some(copy::PULL_ZONE_NAME_TAKEN), None);
        }
        ZoneMade::Failed(why) => {
            ui.set_create_zone_busy(false);
            show_zone_errors(ui, None, None, Some(&why));
        }
    }
}

/// Selects the saved site `id` in the sidebar and opens it.
fn select_site(ui: &AppWindow, session: &Session, id: &SiteId) {
    let index = session
        .sites()
        .iter()
        .position(|site| site.id == *id)
        .and_then(|i| i32::try_from(i).ok());
    if let Some(index) = index {
        ui.set_selected_site(index);
        ui.invoke_site_selected(index);
    }
}

/// The first problem of the Add Site fields Create Bucket… needs (all but the bucket).
fn bucket_form_problem(form: &SiteForm) -> Option<String> {
    let problems = validate(form, false).err().unwrap_or_default();
    problems
        .into_iter()
        .find(|(name, _)| *name != field::BUCKET)
        .map(|(_, message)| copy::bucket_needs(&message))
}

/// The window opened Create Bucket…: keep the form (and its secret) for it, and say at once if
/// the Add Site fields can't make a bucket yet.
fn open_bucket_sheet(m: &Manager, draft: &SiteDraft, secret: &str) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let Some(form) = SiteForm::from_s3_draft(draft, secret) else {
        return;
    };
    let service = match &form {
        SiteForm::S3 { preset, .. } => preset.label(),
        _ => "",
    };
    let problem = bucket_form_problem(&form);
    {
        let mut state = state();
        state.bucket_ask = Some(form);
        state.run += 1;
    }
    ui.set_create_bucket_service(text(service));
    ui.set_create_bucket_busy(false);
    ui.set_create_bucket_error(text(problem.as_deref().unwrap_or_default()));
}

/// Create Bucket: checks the name, then creates the bucket on a task.
fn create_bucket(m: &Arc<Manager>, name: &str) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    if ui.get_create_bucket_busy() {
        return;
    }
    let (form, run) = {
        let state = state();
        (state.bucket_ask.clone(), state.run)
    };
    let Some(form) = form else {
        ui.set_create_bucket_error(text(copy::BUCKET_SHEET_GONE));
        return;
    };
    if let Some(problem) = bucket_form_problem(&form) {
        ui.set_create_bucket_error(problem.into());
        return;
    }
    let SiteForm::S3 {
        preset,
        region,
        account_id,
        endpoint,
        path_style,
        key_id,
        secret,
        ..
    } = form
    else {
        return;
    };
    if let Some(problem) = bucket_name_problem(preset, name) {
        ui.set_create_bucket_error(copy::bucket_name_problem(problem).into());
        return;
    }
    let found = match httporter_s3::endpoint(preset, &region, &account_id, &endpoint, path_style) {
        Ok(found) => found,
        Err(message) => {
            ui.set_create_bucket_error(copy::bucket_needs(&message).into());
            return;
        }
    };
    let host = s3_host(&found.base);
    let service = preset.label();
    let name = name.to_owned();
    ui.set_create_bucket_error(SharedString::new());
    ui.set_create_bucket_busy(true);
    let task = m.clone();
    m.link.rt().spawn(async move {
        let creds = Credentials {
            access_key_id: key_id.trim().to_owned(),
            secret_access_key: secret,
            session_token: None,
        };
        let cfg = S3Config::for_site("", found, creds, &task.session.settings());
        // Making the client sets up TLS, which takes a while.
        let storage = blocking(move || S3Storage::new(cfg)).await;
        let made = match storage {
            Ok(storage) => storage.create_bucket(&name).await,
            Err(e) => Err(e),
        };
        let made = made.map_err(|e| copy::bucket_tool_failed(&e, service, &host));
        let _ = task.link.ui().upgrade_in_event_loop(move |ui| {
            if state().run != run {
                return;
            }
            ui.set_create_bucket_busy(false);
            match made {
                Ok(()) => {
                    {
                        let mut state = state();
                        state.bucket_ask = None;
                        state.run += 1;
                    }
                    ui.set_create_bucket_open(false);
                    ui.set_add_site_picked_bucket(name.as_str().into());
                    ui.set_add_site_picked_bucket_serial(
                        ui.get_add_site_picked_bucket_serial().wrapping_add(1),
                    );
                    toast::show(&ui, ToastKind::Done, copy::bucket_created(&name), None);
                }
                Err(why) => ui.set_create_bucket_error(why.into()),
            }
        });
    });
}

// ---- Settings › Sites › Zone and Bucket ----

/// Wires the Zone and Bucket sections of a new Settings window (`settings_tabs::wire` calls it,
/// on the UI thread).
pub fn wire_settings(window: &SettingsWindow) {
    let Some(m) = super::manager() else {
        return;
    };
    let manager = Arc::downgrade(&m);
    let (weak, mgr) = (window.as_weak(), manager.clone());
    window.on_site_zone_save(move |data| {
        if let (Some(window), Some(m)) = (weak.upgrade(), mgr.upgrade()) {
            save_zone(&window, &m, &data);
        }
    });
    let weak = window.as_weak();
    window.on_site_zone_add_regions(move || {
        if let Some(window) = weak.upgrade() {
            add_regions(&window);
        }
    });
    let (weak, mgr) = (window.as_weak(), manager);
    window.on_site_bucket_save(move |data| {
        if let (Some(window), Some(m)) = (weak.upgrade(), mgr.upgrade()) {
            save_bucket(&window, &m, &data);
        }
    });
}

/// Settings › Sites chose `site` (or none): its Zone or Bucket section, hidden until there is
/// something to show (`settings_tabs::show_site` calls it).
pub fn show_site(window: &SettingsWindow, m: &Arc<Manager>, site: Option<&SiteRecord>) {
    {
        let mut state = state();
        state.zone = None;
        state.bucket = None;
    }
    let id = site.map(|site| text(&site.id.0)).unwrap_or_default();
    window.set_site_zone(ZoneSettingsData {
        id: id.clone(),
        ..ZoneSettingsData::default()
    });
    window.set_site_bucket(BucketSettingsData {
        id,
        ..BucketSettingsData::default()
    });
    match site {
        Some(site) if site.provider == ProviderKind::Bunny => load_zone(window, m, site.clone()),
        Some(site) => load_bucket(window, m, site.clone()),
        None => {}
    }
}

/// The kept account whose key finds `site`'s zone, a client signed in with it and the zone; `None`
/// when no kept key does (the section stays hidden), an error when a key that might have could
/// not be asked.
async fn find_site_zone(
    session: &Session,
    site: &SiteRecord,
) -> Option<Result<(AccountId, BunnyAccount, ZoneRef), String>> {
    let mut accounts: Vec<AccountId> = Vec::new();
    if let Some(link) = session.cdn_link(site.id.clone()).await {
        accounts.push(link.account);
    }
    for account in session.bunny_accounts().await {
        if !accounts.contains(&account.id) {
            accounts.push(account.id);
        }
    }
    let mut failure = None;
    for id in accounts {
        let Some(key) = session.account_key(id.clone()).await else {
            continue;
        };
        let settings = session.settings();
        let Ok(account) = blocking(move || BunnyAccount::with_settings(key, &settings)).await
        else {
            continue;
        };
        match account.find_zone(&site.container).await {
            Ok(zone) => return Some(Ok((id, account, zone))),
            Err(ProviderError::NotFound) => {}
            Err(e) => {
                failure.get_or_insert_with(|| copy::zone_tool_failed(&e));
            }
        }
    }
    failure.map(Err)
}

/// Reads the zone of the Bunny site `site` with a kept key, and shows its settings.
fn load_zone(window: &SettingsWindow, m: &Arc<Manager>, site: SiteRecord) {
    let (session, weak) = (m.session.clone(), window.as_weak());
    m.link.rt().spawn(async move {
        let Some(found) = find_site_zone(&session, &site).await else {
            return;
        };
        let id = site.id.clone();
        let read = match found {
            Ok((account, client, zone)) => {
                // A kept key reaches the zone: the section shows while its settings are read.
                let (reading, id_now) = (weak.clone(), id.clone());
                let _ = reading.upgrade_in_event_loop(move |window| {
                    if window.get_site_zone().id.as_str() == id_now.0 {
                        window.set_site_zone(ZoneSettingsData {
                            id: text(&id_now.0),
                            shown: true,
                            note: text(copy::ZONE_READING),
                            ..ZoneSettingsData::default()
                        });
                    }
                });
                client
                    .zone_settings(zone.id)
                    .await
                    .map(|settings| ShownZone {
                        site: id.clone(),
                        account,
                        zone_id: zone.id,
                        main: zone.region,
                        settings,
                    })
                    .map_err(|e| copy::zone_tool_failed(&e))
            }
            Err(why) => Err(why),
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            if window.get_site_zone().id.as_str() != id.0 {
                return;
            }
            match read {
                Ok(shown) => {
                    window.set_site_zone(zone_data(&shown, ""));
                    state().zone = Some(shown);
                }
                Err(why) => window.set_site_zone(ZoneSettingsData {
                    id: text(&id.0),
                    shown: true,
                    note: copy::zone_unreadable(&why).into(),
                    ..ZoneSettingsData::default()
                }),
            }
        });
    });
}

/// The Zone section for `shown`, with `note` on its Save row.
fn zone_data(shown: &ShownZone, note: &str) -> ZoneSettingsData {
    let cities: Vec<String> = shown
        .settings
        .replication
        .iter()
        .map(|code| region_name(code))
        .collect();
    let region_on: Vec<bool> = REGIONS
        .iter()
        .map(|(code, _)| {
            *code == shown.main || shown.settings.replication.iter().any(|r| r == code)
        })
        .collect();
    let names: Vec<SharedString> = REGIONS.iter().map(|(_, city)| text(city)).collect();
    ZoneSettingsData {
        id: text(&shown.site.0),
        shown: true,
        custom_404: text(
            shown
                .settings
                .custom_404_path
                .as_deref()
                .unwrap_or_default(),
        ),
        rewrite_404: shown.settings.rewrite_404_to_200,
        regions: copy::regions_list(&cities).into(),
        busy: false,
        error: SharedString::new(),
        loaded: true,
        note: text(note),
        path_error: SharedString::new(),
        region_names: ModelRc::new(VecModel::from(names)),
        region_on: bools(region_on),
        region_add: bools(vec![false; REGIONS.len()]),
        adding: false,
    }
}

/// Add Regions…: the list of regions shows, none ticked yet.
fn add_regions(window: &SettingsWindow) {
    let mut zone = window.get_site_zone();
    if !zone.loaded || zone.busy {
        return;
    }
    zone.adding = true;
    zone.region_add = bools(vec![false; REGIONS.len()]);
    window.set_site_zone(zone);
}

/// A 404 page path as typed, tidied: it starts with `/` and names a file. `None` when it can't be
/// one (a backslash, a query, a fragment, control characters, or a folder).
fn page_path(typed: &str) -> Option<String> {
    let typed = typed.trim();
    let refused = |c: char| c.is_control() || matches!(c, '\\' | '?' | '#');
    if typed.is_empty() || typed.contains(refused) || typed.ends_with('/') {
        return None;
    }
    Some(if typed.starts_with('/') {
        typed.to_owned()
    } else {
        format!("/{typed}")
    })
}

/// Save on the Zone section: checks the 404 path, then changes the zone on Bunny (adding the
/// regions ticked) and reads it back.
fn save_zone(window: &SettingsWindow, m: &Arc<Manager>, data: &ZoneSettingsData) {
    let Some(shown) = state().zone.clone() else {
        return;
    };
    if shown.site.0 != data.id.as_str() {
        return;
    }
    let mut zone = window.get_site_zone();
    if crate::manage::settings_tabs::site_read_only(&m.session, &shown.site) {
        zone.error = text(copy::READ_ONLY_NOT_CHANGED);
        window.set_site_zone(zone);
        return;
    }
    let typed = data.custom_404.trim();
    let custom_404_path = if typed.is_empty() {
        None
    } else {
        match page_path(typed) {
            Some(path) => Some(path),
            None => {
                zone.path_error = text(copy::PATH_404_INVALID);
                window.set_site_zone(zone);
                return;
            }
        }
    };
    let mut replication = shown.settings.replication.clone();
    for (i, on) in data.region_add.iter().enumerate() {
        if let (true, Some((code, _))) = (on, REGIONS.get(i))
            && *code != shown.main
            && !replication.iter().any(|r| r == code)
        {
            replication.push((*code).to_owned());
        }
    }
    let wanted = ZoneSettings {
        custom_404_path,
        rewrite_404_to_200: data.rewrite_404,
        replication,
    };
    if wanted == shown.settings {
        zone.note = text(copy::NOTHING_CHANGED);
        zone.error = SharedString::new();
        zone.adding = false;
        window.set_site_zone(zone);
        return;
    }
    zone.busy = true;
    zone.error = SharedString::new();
    zone.path_error = SharedString::new();
    zone.note = SharedString::new();
    window.set_site_zone(zone);

    let (session, weak) = (m.session.clone(), window.as_weak());
    m.link.rt().spawn(async move {
        let saved = save_zone_settings(&session, &shown, &wanted).await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            let mut zone = window.get_site_zone();
            if zone.id.as_str() != shown.site.0 {
                return;
            }
            match saved {
                Ok(settings) => {
                    let shown = ShownZone { settings, ..shown };
                    window.set_site_zone(zone_data(&shown, copy::ZONE_SAVED));
                    state().zone = Some(shown);
                }
                Err(why) => {
                    zone.busy = false;
                    zone.error = why.into();
                    window.set_site_zone(zone);
                }
            }
        });
    });
}

/// Changes the zone and reads its settings back, with the kept key read again now.
async fn save_zone_settings(
    session: &Session,
    shown: &ShownZone,
    wanted: &ZoneSettings,
) -> Result<ZoneSettings, String> {
    let key = session
        .account_key(shown.account.clone())
        .await
        .ok_or_else(|| copy::ZONE_KEY_GONE.to_owned())?;
    let settings = session.settings();
    let account = blocking(move || BunnyAccount::with_settings(key, &settings))
        .await
        .map_err(|e| copy::zone_tool_failed(&e))?;
    account
        .update_zone_settings(shown.zone_id, wanted)
        .await
        .map_err(|e| copy::zone_tool_failed(&e))?;
    account
        .zone_settings(shown.zone_id)
        .await
        .map_err(|e| copy::zone_tool_failed(&e))
}

/// The client of the S3 site `site`, with its saved key, and its host (for messages).
async fn open_bucket(session: &Session, site: &SiteRecord) -> Result<S3Storage, String> {
    // Its error is one of the app's own sentences.
    let (endpoint, preset) = s3_endpoint(site).map_err(|e| match e {
        ProviderError::Permanent(text) => text,
        other => other.to_string(),
    })?;
    let creds = session.creds();
    let settings = session.settings();
    let record = site.clone();
    blocking(move || {
        let secret = read_secret(&*creds, &record).map_err(|e| hub_error_text(&e, &record))?;
        let SiteSecret::S3 {
            access_key_id,
            secret_access_key,
        } = secret
        else {
            return Err(crate::copy::password_unreadable(
                &record.name,
                crate::copy::SECRET_MISMATCH,
            ));
        };
        let creds = Credentials {
            access_key_id,
            secret_access_key,
            session_token: None,
        };
        let cfg = S3Config::for_site(&record.container, endpoint, creds, &settings);
        S3Storage::new(cfg).map_err(|e| copy::bucket_tool_failed(&e, preset.label(), &record.host))
    })
    .await
}

/// What reading a bucket's settings found: each part's value, `None` when the service turned out
/// not to offer it, or why it couldn't be read.
struct BucketRead {
    cors: Result<Option<Vec<CorsRule>>, String>,
    website: Result<Option<Option<Website>>, String>,
}

/// Reads the parts of the bucket `shown` says the service offers.
async fn read_bucket(storage: &S3Storage, shown: &ShownBucket, host: &str) -> BucketRead {
    let offered = |e: &ProviderError| matches!(e, ProviderError::Refused(m) if m == NOT_OFFERED);
    let cors = if shown.cors_shown {
        match storage.cors().await {
            Ok(rules) => Ok(Some(rules)),
            Err(e) if offered(&e) => Ok(None),
            Err(e) => Err(copy::bucket_tool_failed(&e, shown.service, host)),
        }
    } else {
        Ok(None)
    };
    let website = if shown.website_shown {
        match storage.website().await {
            Ok(website) => Ok(Some(website)),
            Err(e) if offered(&e) => Ok(None),
            Err(e) => Err(copy::bucket_tool_failed(&e, shown.service, host)),
        }
    } else {
        Ok(None)
    };
    BucketRead { cors, website }
}

/// `shown` with what was read: a part the service doesn't offer after all is hidden. The error,
/// if a part couldn't be read.
fn apply_read(shown: &mut ShownBucket, read: BucketRead) -> Option<String> {
    let mut failed = None;
    match read.cors {
        Ok(Some(rules)) => shown.cors = rules,
        Ok(None) => shown.cors_shown = false,
        Err(why) => failed = Some(why),
    }
    match read.website {
        Ok(Some(website)) => shown.website = website,
        Ok(None) => shown.website_shown = false,
        Err(why) => {
            failed.get_or_insert(why);
        }
    }
    failed
}

/// The Bucket section for `shown`: the first CORS rule's origins and methods, the website's
/// documents; `note` on its Save row, and `loaded` when the settings were read.
fn bucket_data(shown: &ShownBucket, note: &str, loaded: bool) -> BucketSettingsData {
    let first = shown.cors.first();
    let joined = |values: Option<&Vec<String>>| values.map(|v| v.join(", ")).unwrap_or_default();
    let not_offered = |shown_part: bool| {
        if shown_part {
            String::new()
        } else {
            copy::not_offered(shown.service)
        }
    };
    BucketSettingsData {
        id: text(&shown.site.0),
        shown: true,
        cors_shown: shown.cors_shown,
        cors_origins: joined(first.map(|rule| &rule.origins)).into(),
        cors_methods: joined(first.map(|rule| &rule.methods)).into(),
        website_shown: shown.website_shown,
        website_index: shown
            .website
            .as_ref()
            .map(|w| text(&w.index))
            .unwrap_or_default(),
        website_error: shown
            .website
            .as_ref()
            .and_then(|w| w.error.as_deref())
            .map(text)
            .unwrap_or_default(),
        note: text(note),
        busy: false,
        error: SharedString::new(),
        loaded,
        cors_note: not_offered(shown.cors_shown).into(),
        website_note: not_offered(shown.website_shown).into(),
        origins_error: SharedString::new(),
        methods_error: SharedString::new(),
        index_error: SharedString::new(),
        error_doc_error: SharedString::new(),
    }
}

/// Shows the Bucket section of the S3 site `site`: at once what its service offers, then what
/// the bucket has.
fn load_bucket(window: &SettingsWindow, m: &Arc<Manager>, site: SiteRecord) {
    let Some(s3) = &site.s3 else {
        return;
    };
    let preset = s3.preset;
    let mut shown = ShownBucket {
        site: site.id.clone(),
        service: preset.label(),
        cors_shown: preset.supports_cors(),
        website_shown: preset.supports_website(),
        cors: Vec::new(),
        website: None,
    };
    let any = shown.cors_shown || shown.website_shown;
    window.set_site_bucket(bucket_data(
        &shown,
        if any { copy::BUCKET_READING } else { "" },
        false,
    ));
    if !any {
        return;
    }
    let (session, weak) = (m.session.clone(), window.as_weak());
    m.link.rt().spawn(async move {
        let read = match open_bucket(&session, &site).await {
            Ok(storage) => {
                let read = read_bucket(&storage, &shown, &site.host).await;
                apply_read(&mut shown, read)
            }
            Err(why) => Some(why),
        };
        let _ = weak.upgrade_in_event_loop(move |window| {
            if window.get_site_bucket().id.as_str() != shown.site.0 {
                return;
            }
            match read {
                None => {
                    window.set_site_bucket(bucket_data(&shown, "", true));
                    state().bucket = Some(shown);
                }
                Some(why) => {
                    window.set_site_bucket(bucket_data(
                        &shown,
                        &copy::bucket_unreadable(&why),
                        false,
                    ));
                }
            }
        });
    });
}

/// The comma- (or space-) separated values of a field.
fn listed(typed: &str) -> Vec<String> {
    typed
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `*`, or `http(s)://host[:port]` with at most one `*` (S3's rule for an allowed origin).
fn is_origin(origin: &str) -> bool {
    if origin == "*" {
        return true;
    }
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    rest.is_some_and(|host| {
        !host.is_empty()
            && origin.matches('*').count() <= 1
            && !host.contains(['/', '?', '#', '@', '\\'])
            && !host.chars().any(char::is_control)
    })
}

/// The CORS rules and website the form asks for, from what the bucket has; or the problems, by
/// field (origins, methods, index, error document).
#[derive(Default)]
struct BucketProblems {
    origins: Option<String>,
    methods: Option<String>,
    index: Option<String>,
    error_doc: Option<String>,
}

impl BucketProblems {
    fn any(&self) -> bool {
        self.origins.is_some()
            || self.methods.is_some()
            || self.index.is_some()
            || self.error_doc.is_some()
    }
}

/// What Save asks for: the bucket's rules with the first one as the form says (none when the
/// origins are empty), and the website (none when the index is empty).
fn wanted_bucket(
    shown: &ShownBucket,
    data: &BucketSettingsData,
) -> Result<(Vec<CorsRule>, Option<Website>), BucketProblems> {
    let mut problems = BucketProblems::default();
    let mut rules = shown.cors.clone();
    if shown.cors_shown {
        let origins = listed(&data.cors_origins);
        let methods: Vec<String> = listed(&data.cors_methods)
            .into_iter()
            .map(|method| method.to_ascii_uppercase())
            .collect();
        problems.origins = origins
            .iter()
            .find(|origin| !is_origin(origin))
            .map(|origin| copy::origin_invalid(origin));
        problems.methods = methods
            .iter()
            .find(|method| !CORS_METHODS.contains(&method.as_str()))
            .map(|method| copy::method_invalid(method));
        if origins.is_empty() && !methods.is_empty() && problems.methods.is_none() {
            problems.methods = Some(copy::METHODS_NEED_ORIGINS.to_owned());
        }
        if origins.is_empty() {
            if !rules.is_empty() {
                rules.remove(0);
            }
        } else {
            let methods = if methods.is_empty() {
                vec!["GET".to_owned(), "HEAD".to_owned()]
            } else {
                methods
            };
            match rules.first_mut() {
                Some(first) => {
                    first.origins = origins;
                    first.methods = methods;
                }
                None => rules.push(CorsRule {
                    origins,
                    methods,
                    ..CorsRule::default()
                }),
            }
        }
    }
    let mut website = shown.website.clone();
    if shown.website_shown {
        let index = data.website_index.trim();
        let error_doc = data.website_error.trim().trim_start_matches('/');
        let bad = |text: &str| text.chars().any(char::is_control);
        if index.is_empty() {
            if error_doc.is_empty() {
                website = None;
            } else {
                problems.index = Some(copy::INDEX_NEEDED.to_owned());
            }
        } else if index.contains(['/', '\\']) || bad(index) {
            problems.index = Some(copy::INDEX_INVALID.to_owned());
        } else if bad(error_doc) || error_doc.ends_with('/') {
            problems.error_doc = Some(copy::ERROR_DOC_INVALID.to_owned());
        } else {
            website = Some(Website {
                index: index.to_owned(),
                error: (!error_doc.is_empty()).then(|| error_doc.to_owned()),
            });
        }
    }
    if problems.any() {
        Err(problems)
    } else {
        Ok((rules, website))
    }
}

/// Save on the Bucket section: checks the fields, changes what differs on the service, and reads
/// the bucket back.
fn save_bucket(window: &SettingsWindow, m: &Arc<Manager>, data: &BucketSettingsData) {
    let Some(shown) = state().bucket.clone() else {
        return;
    };
    if shown.site.0 != data.id.as_str() {
        return;
    }
    let mut bucket = window.get_site_bucket();
    if crate::manage::settings_tabs::site_read_only(&m.session, &shown.site) {
        bucket.error = text(copy::READ_ONLY_NOT_CHANGED);
        window.set_site_bucket(bucket);
        return;
    }
    let (rules, website) = match wanted_bucket(&shown, data) {
        Ok(wanted) => wanted,
        Err(problems) => {
            let shown_text = |problem: Option<String>| problem.unwrap_or_default().into();
            bucket.origins_error = shown_text(problems.origins);
            bucket.methods_error = shown_text(problems.methods);
            bucket.index_error = shown_text(problems.index);
            bucket.error_doc_error = shown_text(problems.error_doc);
            window.set_site_bucket(bucket);
            return;
        }
    };
    let cors_changed = rules != shown.cors;
    let website_changed = website != shown.website;
    if !cors_changed && !website_changed {
        bucket.note = text(copy::NOTHING_CHANGED);
        bucket.error = SharedString::new();
        window.set_site_bucket(bucket);
        return;
    }
    bucket.busy = true;
    bucket.error = SharedString::new();
    bucket.note = SharedString::new();
    window.set_site_bucket(bucket);

    let (session, weak) = (m.session.clone(), window.as_weak());
    let Some(site) = m.session.site(&shown.site) else {
        return;
    };
    m.link.rt().spawn(async move {
        let mut shown = shown;
        let outcome = async {
            let storage = open_bucket(&session, &site).await?;
            let failed = |e: ProviderError| copy::bucket_tool_failed(&e, shown.service, &site.host);
            if cors_changed {
                storage.set_cors(&rules).await.map_err(failed)?;
            }
            if website_changed {
                storage
                    .set_website(website.as_ref())
                    .await
                    .map_err(failed)?;
            }
            let read = read_bucket(&storage, &shown, &site.host).await;
            Ok::<BucketRead, String>(read)
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |window| {
            let mut bucket = window.get_site_bucket();
            if bucket.id.as_str() != shown.site.0 {
                return;
            }
            match outcome {
                Ok(read) => match apply_read(&mut shown, read) {
                    None => {
                        window.set_site_bucket(bucket_data(&shown, copy::BUCKET_SAVED, true));
                        state().bucket = Some(shown);
                    }
                    Some(why) => {
                        bucket.busy = false;
                        bucket.error = copy::bucket_unreadable(&why).into();
                        window.set_site_bucket(bucket);
                    }
                },
                Err(why) => {
                    bucket.busy = false;
                    bucket.error = why.into();
                    window.set_site_bucket(bucket);
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_paths_start_with_a_slash_and_name_a_file() {
        assert_eq!(page_path("404.html").as_deref(), Some("/404.html"));
        assert_eq!(
            page_path(" /errors/404.html ").as_deref(),
            Some("/errors/404.html")
        );
        for bad in ["", "/", "errors/", "a?b", "a#b", "a\\b", "a\u{7}b"] {
            assert_eq!(page_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn origins_follow_s3s_rule() {
        for good in [
            "*",
            "https://example.com",
            "http://localhost:3000",
            "https://*.example.com",
        ] {
            assert!(is_origin(good), "{good}");
        }
        for bad in [
            "example.com",
            "https://",
            "https://a.com/path",
            "ftp://a.com",
            "https://*.*.a",
        ] {
            assert!(!is_origin(bad), "{bad}");
        }
    }

    fn shown(rules: Vec<CorsRule>, website: Option<Website>) -> ShownBucket {
        ShownBucket {
            site: SiteId("s".into()),
            service: "Amazon S3",
            cors_shown: true,
            website_shown: true,
            cors: rules,
            website,
        }
    }

    fn form(origins: &str, methods: &str, index: &str, error: &str) -> BucketSettingsData {
        BucketSettingsData {
            cors_origins: origins.into(),
            cors_methods: methods.into(),
            website_index: index.into(),
            website_error: error.into(),
            ..BucketSettingsData::default()
        }
    }

    #[test]
    fn the_first_cors_rule_is_edited_and_the_others_kept() {
        let other = CorsRule {
            origins: vec!["*".into()],
            methods: vec!["PUT".into()],
            id: Some("uploads".into()),
            ..CorsRule::default()
        };
        let first = CorsRule {
            origins: vec!["https://old.example".into()],
            methods: vec!["GET".into()],
            max_age: Some(60),
            ..CorsRule::default()
        };
        let bucket = shown(vec![first, other.clone()], None);

        let (rules, _) = wanted_bucket(
            &bucket,
            &form("https://a.example, https://b.example", "get head", "", ""),
        )
        .ok()
        .unwrap();
        assert_eq!(rules[0].origins, ["https://a.example", "https://b.example"]);
        assert_eq!(rules[0].methods, ["GET", "HEAD"]);
        assert_eq!(rules[0].max_age, Some(60));
        assert_eq!(rules[1], other);

        // Empty origins take the first rule away; methods default to GET, HEAD for a new one.
        let (rules, _) = wanted_bucket(&bucket, &form("", "", "", "")).ok().unwrap();
        assert_eq!(rules, [other]);
        let (rules, _) = wanted_bucket(&shown(Vec::new(), None), &form("*", "", "", ""))
            .ok()
            .unwrap();
        assert_eq!(rules[0].methods, ["GET", "HEAD"]);
    }

    #[test]
    fn bucket_problems_go_to_their_fields() {
        let bucket = shown(Vec::new(), None);
        let problems = wanted_bucket(&bucket, &form("example.com", "FETCH", "a/index.html", ""))
            .err()
            .unwrap();
        assert!(problems.origins.is_some());
        assert!(problems.methods.is_some());
        assert!(problems.index.is_some());
        let problems = wanted_bucket(&bucket, &form("", "GET", "", "404.html"))
            .err()
            .unwrap();
        assert_eq!(
            problems.methods.as_deref(),
            Some(copy::METHODS_NEED_ORIGINS)
        );
        assert_eq!(problems.index.as_deref(), Some(copy::INDEX_NEEDED));
    }

    #[test]
    fn the_website_follows_the_index_field() {
        let had = Website {
            index: "index.html".into(),
            error: None,
        };
        let bucket = shown(Vec::new(), Some(had));
        let (_, website) = wanted_bucket(&bucket, &form("", "", "", "")).ok().unwrap();
        assert_eq!(website, None);
        let (_, website) = wanted_bucket(&bucket, &form("", "", "home.html", "/errors/404.html"))
            .ok()
            .unwrap();
        assert_eq!(
            website,
            Some(Website {
                index: "home.html".into(),
                error: Some("errors/404.html".into()),
            })
        );
    }
}
