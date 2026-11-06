// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Add Site form: what the sheet collects and the checks made before a connection is tried.
//!
//! The sheet has three kinds of form: a Bunny storage zone and its password, a Bunny account key
//! (which lists the account's zones to add) and the S3 services. They are described by
//! [`SiteForm`], checked by [`validate`] (whose problems [`show_form_errors`] puts under their
//! fields) and, for S3, previewed by [`s3_endpoint_preview`]. Editing a site starts the sheet from
//! [`draft_for`]; Import starts it from what a `.env` file or an AWS profile holds.
//!
//! The first release's Bunny-only form ([`AddSiteForm`], checked into a [`ValidSite`] with one
//! [`FieldError`]) is kept for its tests; the window goes through [`SiteForm`].
//!
//! Secrets pass through here but are never logged, printed or stored: the `Debug` output of the
//! form types leaves them out.

use std::cell::RefCell;
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;

use httporter_bunny::{REGIONS, ZoneList};
use httporter_core::{ProviderKind, S3Preset, SiteRecord};
use httporter_s3::Url;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::copy;
use crate::import::EnvImport;
use crate::{AppWindow, S3Errors, SiteDraft, ZoneChoice};

/// What the user typed, exactly as the sheet reports it.
#[derive(Clone, PartialEq, Eq, Default)]
#[cfg_attr(not(test), allow(dead_code))] // The I1 form, kept for its tests (see the module docs).
pub struct AddSiteForm {
    /// Site name; empty means "same as the zone".
    pub name: String,
    pub zone: String,
    /// Index into the region list the sheet shows (negative when nothing is chosen).
    pub region_index: i32,
    pub password: String,
}

/// A form that passed [`AddSiteForm::validate`]: trimmed, with the region as its code.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct ValidSite {
    pub name: String,
    pub zone: String,
    pub region_code: String,
    pub password: String,
}

/// The first problem found, with the text to show under the field it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum FieldError {
    Zone(String),
    Region(String),
    Password(String),
}

#[cfg_attr(not(test), allow(dead_code))]
impl FieldError {
    /// The text to show under the field.
    pub fn message(&self) -> &str {
        match self {
            FieldError::Zone(m) | FieldError::Region(m) | FieldError::Password(m) => m,
        }
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

// Not derived: a derived `Debug` would print the password in a failed assertion or a log line.
impl fmt::Debug for AddSiteForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AddSiteForm")
            .field("name", &self.name)
            .field("zone", &self.zone)
            .field("region_index", &self.region_index)
            .field("password", &"<hidden>")
            .finish()
    }
}

impl fmt::Debug for ValidSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidSite")
            .field("name", &self.name)
            .field("zone", &self.zone)
            .field("region_code", &self.region_code)
            .field("password", &"<hidden>")
            .finish()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl AddSiteForm {
    /// The form from the arguments of the sheet's `add-site-test` and `add-site-save` callbacks,
    /// which come in this order: site name, zone, region index, password. Both buttons go through
    /// here, so the order is written down once (the name and the zone are both strings: swapping
    /// them would compile).
    pub fn from_sheet(name: &str, zone: &str, region_index: i32, password: &str) -> Self {
        Self {
            name: name.into(),
            zone: zone.into(),
            region_index,
            password: password.into(),
        }
    }

    /// Checks the form. `region_codes` are the codes behind the sheet's region list, in the same
    /// order. The first problem found is returned: zone, then region, then password.
    ///
    /// The zone and the name are trimmed, and a blank name becomes the zone. The password is kept
    /// exactly as typed.
    pub fn validate(&self, region_codes: &[&str]) -> Result<ValidSite, FieldError> {
        let zone = self.zone.trim();
        if let Some(problem) = zone_problem(zone) {
            return Err(FieldError::Zone(problem.into()));
        }
        let region_code = usize::try_from(self.region_index)
            .ok()
            .and_then(|index| region_codes.get(index))
            .ok_or_else(|| FieldError::Region(copy::CHOOSE_REGION.into()))?;
        if self.password.is_empty() {
            return Err(FieldError::Password(copy::PASSWORD_MISSING.into()));
        }
        let name = self.name.trim();
        Ok(ValidSite {
            name: if name.is_empty() { zone } else { name }.into(),
            zone: zone.into(),
            region_code: (*region_code).into(),
            password: self.password.clone(),
        })
    }
}

/// What is wrong with an already trimmed zone name, if anything.
fn zone_problem(zone: &str) -> Option<&'static str> {
    if zone.is_empty() {
        Some(copy::ZONE_MISSING)
    } else if zone.chars().any(char::is_whitespace) {
        Some(copy::ZONE_HAS_SPACES)
    } else {
        None
    }
}

// ---- Every kind of site ----

/// The names [`validate`] gives the fields it finds wrong; they are the sheet's field names.
pub mod field {
    pub const ZONE: &str = "zone";
    pub const REGION: &str = "region";
    pub const PASSWORD: &str = "password";
    pub const ACCOUNT_KEY: &str = "account-key";
    pub const ACCOUNT_ID: &str = "account-id";
    pub const ENDPOINT: &str = "endpoint";
    pub const BUCKET: &str = "bucket";
    pub const KEY_ID: &str = "key-id";
    pub const SECRET: &str = "secret";
}

/// What the Add Site sheet collected, by kind of sign-in. Its `Debug` output leaves out the
/// password, the account key and the secret access key.
#[derive(Clone, PartialEq, Eq)]
pub enum SiteForm {
    /// A Bunny storage zone and its password.
    BunnyPassword {
        zone: String,
        /// Index into `REGIONS` (negative when nothing is chosen).
        region_index: i32,
        password: String,
    },
    /// A Bunny account key, from which the zones to add are listed.
    BunnyAccount { key: String },
    /// A bucket on an S3-compatible service.
    S3 {
        preset: S3Preset,
        region: String,
        account_id: String,
        /// The address typed for MinIO and other services.
        endpoint: String,
        path_style: bool,
        bucket: String,
        key_id: String,
        secret: String,
    },
}

impl fmt::Debug for SiteForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BunnyPassword {
                zone, region_index, ..
            } => f
                .debug_struct("BunnyPassword")
                .field("zone", zone)
                .field("region_index", region_index)
                .field("password", &"<hidden>")
                .finish(),
            Self::BunnyAccount { .. } => f
                .debug_struct("BunnyAccount")
                .field("key", &"<hidden>")
                .finish(),
            Self::S3 {
                preset,
                region,
                account_id,
                endpoint,
                path_style,
                bucket,
                key_id,
                ..
            } => f
                .debug_struct("S3")
                .field("preset", preset)
                .field("region", region)
                .field("account_id", account_id)
                .field("endpoint", endpoint)
                .field("path_style", path_style)
                .field("bucket", bucket)
                .field("key_id", key_id)
                .field("secret", &"<hidden>")
                .finish(),
        }
    }
}

/// The S3 preset behind a tile of the provider grid, which lists Bunny first and then the S3
/// services in the order of [`S3Preset`]; `None` for Bunny (0) or an index outside the grid.
pub fn preset_for_tile(index: i32) -> Option<S3Preset> {
    const PRESETS: [S3Preset; 7] = [
        S3Preset::AwsS3,
        S3Preset::CloudflareR2,
        S3Preset::BackblazeB2,
        S3Preset::Wasabi,
        S3Preset::DigitalOceanSpaces,
        S3Preset::Minio,
        S3Preset::Other,
    ];
    usize::try_from(index.checked_sub(1)?)
        .ok()
        .and_then(|i| PRESETS.get(i).copied())
}

impl SiteForm {
    /// The S3 form behind the sheet's `add-site-test-s3` and `add-site-save-s3` callbacks (the
    /// draft of the visible fields, and the secret access key); `None` when the draft's provider
    /// is Bunny.
    pub fn from_s3_draft(draft: &SiteDraft, secret: &str) -> Option<Self> {
        Some(Self::S3 {
            preset: preset_for_tile(draft.provider)?,
            region: draft.region.to_string(),
            account_id: draft.account_id.to_string(),
            endpoint: draft.endpoint.to_string(),
            path_style: draft.path_style,
            bucket: draft.bucket.to_string(),
            key_id: draft.key_id.to_string(),
            secret: secret.to_owned(),
        })
    }
}

/// Checks a form of any kind and returns every problem found, each with the name of its field
/// (see [`field`]) and the text to show under it. `editing` is true when the form changes a saved
/// site: a secret left empty then means "keep the saved one".
///
/// S3 addresses are checked by `httporter_s3::endpoint`, whose own sentences are the messages.
pub fn validate(form: &SiteForm, editing: bool) -> Result<(), Vec<(&'static str, String)>> {
    let mut problems: Vec<(&'static str, String)> = Vec::new();
    let mut flag = |name: &'static str, message: &str| problems.push((name, message.to_owned()));
    match form {
        SiteForm::BunnyPassword {
            zone,
            region_index,
            password,
        } => {
            if let Some(problem) = zone_problem(zone.trim()) {
                flag(field::ZONE, problem);
            }
            let known = usize::try_from(*region_index).is_ok_and(|i| i < REGIONS.len());
            if !known {
                flag(field::REGION, copy::CHOOSE_REGION);
            }
            if password.is_empty() && !editing {
                flag(field::PASSWORD, copy::PASSWORD_MISSING);
            }
        }
        SiteForm::BunnyAccount { key } => {
            if key.trim().is_empty() && !editing {
                flag(field::ACCOUNT_KEY, copy::ACCOUNT_KEY_MISSING);
            }
        }
        SiteForm::S3 {
            preset,
            region,
            account_id,
            endpoint,
            path_style,
            bucket,
            key_id,
            secret,
        } => {
            if let Err(message) =
                httporter_s3::endpoint(*preset, region, account_id, endpoint, *path_style)
            {
                let wrong = endpoint_error_field(*preset, account_id, endpoint, *path_style);
                flag(wrong, &message);
            }
            if bucket.trim().is_empty() {
                flag(field::BUCKET, copy::BUCKET_MISSING);
            }
            if key_id.trim().is_empty() {
                flag(field::KEY_ID, copy::KEY_ID_MISSING);
            }
            if secret.is_empty() && !editing {
                flag(field::SECRET, copy::SECRET_MISSING);
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// The field an error of `httporter_s3::endpoint` is about.
fn endpoint_error_field(
    preset: S3Preset,
    account_id: &str,
    endpoint: &str,
    path_style: bool,
) -> &'static str {
    match preset {
        S3Preset::CloudflareR2 => field::ACCOUNT_ID,
        // These two take the address as typed and the region only if one is given, so an empty
        // region always passes: when the address alone fails, the address is the problem.
        S3Preset::Minio | S3Preset::Other => {
            if httporter_s3::endpoint(preset, "", account_id, endpoint, path_style).is_err() {
                field::ENDPOINT
            } else {
                field::REGION
            }
        }
        S3Preset::AwsS3
        | S3Preset::BackblazeB2
        | S3Preset::Wasabi
        | S3Preset::DigitalOceanSpaces => field::REGION,
    }
}

/// The address the sheet shows for an S3 service and bucket, for example
/// `https://my-bucket.s3.eu-west-1.amazonaws.com/`; empty while the fields cannot name one yet
/// (no region, a malformed address, …). The sheet shows no error for that: [`validate`] does,
/// when the user presses a button.
pub fn s3_endpoint_preview(
    preset: S3Preset,
    region: &str,
    account_id: &str,
    endpoint: &str,
    bucket: &str,
    path_style: bool,
) -> String {
    match httporter_s3::endpoint(preset, region, account_id, endpoint, path_style) {
        Ok(found) => httporter_s3::bucket_url(&found, bucket.trim()).to_string(),
        Err(_) => String::new(),
    }
}

/// The preview for what the sheet reports through `add-site-s3-changed`.
fn preview_of(draft: &SiteDraft) -> String {
    match preset_for_tile(draft.provider) {
        Some(preset) => s3_endpoint_preview(
            preset,
            &draft.region,
            &draft.account_id,
            &draft.endpoint,
            &draft.bucket,
            draft.path_style,
        ),
        None => String::new(),
    }
}

/// Puts the zones of a Bunny account in the sheet's account key list: first the ones that can be
/// added, each starting checked, then the ones that can't, greyed out with their reason. Row `i`
/// of the sheet is `list.zones[i]` for `i < list.zones.len()`, which is how the `[bool]` of
/// `add-site-zones-chosen` is read.
pub fn show_zones(ui: &AppWindow, list: &ZoneList) {
    let usable = list.zones.iter().map(|zone| {
        let hostnames: Vec<&str> = zone
            .pull_zones
            .iter()
            .flat_map(|pull_zone| pull_zone.hostnames.iter().map(String::as_str))
            .collect();
        ZoneChoice {
            name: zone.name.as_str().into(),
            region: region_city(&zone.region).into(),
            hostnames: hostnames.join(", ").into(),
            usable: true,
            reason: SharedString::new(),
        }
    });
    let unusable = list.unusable.iter().map(|zone| ZoneChoice {
        name: zone.name.as_str().into(),
        region: SharedString::new(),
        hostnames: SharedString::new(),
        usable: false,
        reason: zone.reason.as_str().into(),
    });
    let rows: Vec<ZoneChoice> = usable.chain(unusable).collect();
    let checked: Vec<bool> = (0..rows.len()).map(|i| i < list.zones.len()).collect();
    ui.set_add_site_zone_count(i32::try_from(list.zones.len()).unwrap_or(i32::MAX));
    ui.set_add_site_zones(ModelRc::new(VecModel::from(rows)));
    ui.set_add_site_zone_checked(ModelRc::new(VecModel::from(checked)));
}

/// Puts the bucket names the service listed in the sheet's bucket list.
pub fn show_buckets(ui: &AppWindow, names: &[String]) {
    let names: Vec<SharedString> = names.iter().map(|name| name.as_str().into()).collect();
    ui.set_add_site_buckets(ModelRc::new(VecModel::from(names)));
}

/// The city a Bunny region code stands for ("Singapore"); the code itself when it is not known.
fn region_city(code: &str) -> &str {
    REGIONS
        .iter()
        .find(|(known, _)| *known == code)
        .map_or(code, |(_, city)| city)
}

/// Wires the callbacks of the sheet that need no session: the address preview of an S3 service.
/// The rest (saving, Find Zones, the zone and bucket lists, Import) are answered in `binding.rs`.
pub fn install(ui: &AppWindow) {
    ui.on_add_site_s3_changed({
        let weak = ui.as_weak();
        move |draft| {
            if let Some(ui) = weak.upgrade() {
                ui.set_add_site_endpoint(preview_of(&draft).into());
            }
        }
    });
}

impl SiteForm {
    /// The Bunny password form behind the sheet's `add-site-test` and `add-site-save` callbacks.
    pub fn bunny(zone: &str, region_index: i32, password: &str) -> Self {
        Self::BunnyPassword {
            zone: zone.to_owned(),
            region_index,
            password: password.to_owned(),
        }
    }

    /// The secret field (password, account key or secret access key) was left empty.
    pub fn secret_is_empty(&self) -> bool {
        match self {
            Self::BunnyPassword { password, .. } => password.is_empty(),
            Self::BunnyAccount { key } => key.is_empty(),
            Self::S3 { secret, .. } => secret.is_empty(),
        }
    }

    /// The form with `given` in its secret field if that was left empty: the secret Import found,
    /// which the sheet does not show.
    pub fn or_secret(mut self, given: &str) -> Self {
        if self.secret_is_empty() {
            match &mut self {
                Self::BunnyPassword { password, .. } => given.clone_into(password),
                Self::BunnyAccount { key } => given.clone_into(key),
                Self::S3 { secret, .. } => given.clone_into(secret),
            }
        }
        self
    }
}

/// `host[:port]` of an S3 address: what a site's `host` holds and the cloud pane's chip shows.
pub fn s3_host(base: &Url) -> String {
    let host = base.host_str().unwrap_or_default();
    match base.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// The provider grid's tile for an S3 preset (the inverse of [`preset_for_tile`]).
pub fn tile_for(preset: S3Preset) -> i32 {
    match preset {
        S3Preset::AwsS3 => 1,
        S3Preset::CloudflareR2 => 2,
        S3Preset::BackblazeB2 => 3,
        S3Preset::Wasabi => 4,
        S3Preset::DigitalOceanSpaces => 5,
        S3Preset::Minio => 6,
        S3Preset::Other => 7,
    }
}

/// The index into `REGIONS` of a Bunny region written as its code (`sg`) or its city
/// (`Singapore`), either case; -1 (nothing chosen) for one that is not known.
pub fn bunny_region_index(region: &str) -> i32 {
    let region = region.trim();
    REGIONS
        .iter()
        .position(|(code, city)| {
            code.eq_ignore_ascii_case(region) || city.eq_ignore_ascii_case(region)
        })
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1)
}

/// The fields Edit Site starts from for a saved site: everything but the secret, which is left
/// empty (and kept if it stays so). `key_id` is the access key ID saved for an S3 site.
pub fn draft_for(site: &SiteRecord, key_id: &str) -> SiteDraft {
    let mut draft = SiteDraft {
        name: site.name.as_str().into(),
        ..SiteDraft::default()
    };
    match &site.s3 {
        None => {
            draft.zone = site.container.as_str().into();
            draft.region_index = bunny_region_index(&site.region);
        }
        Some(s3) => {
            draft.provider = tile_for(s3.preset);
            draft.bucket = site.container.as_str().into();
            draft.key_id = key_id.into();
            match s3.preset {
                S3Preset::CloudflareR2 => {
                    let account = site.host.split('.').next().unwrap_or_default();
                    draft.account_id = account.into();
                }
                S3Preset::Minio => draft.endpoint = s3.endpoint.as_str().into(),
                S3Preset::Other => {
                    draft.endpoint = s3.endpoint.as_str().into();
                    draft.region = site.region.as_str().into();
                    draft.path_style = s3.path_style;
                }
                S3Preset::AwsS3
                | S3Preset::BackblazeB2
                | S3Preset::Wasabi
                | S3Preset::DigitalOceanSpaces => draft.region = site.region.as_str().into(),
            }
        }
    }
    draft
}

/// Puts each of `problems` (from [`validate`]) under its field on the open sheet and clears the
/// other fields' messages; with any, the keyboard focus goes to the first field that has one.
/// `s3` says which form they belong to (the Bunny region is a picker, the S3 one a text field).
pub fn show_form_errors(ui: &AppWindow, s3: bool, problems: &[(&'static str, String)]) {
    let message = |name: &str| -> SharedString {
        problems
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, message)| message.as_str().into())
            .unwrap_or_default()
    };
    let (bunny, none) = (!s3, SharedString::new());
    ui.set_add_site_zone_error(message(field::ZONE));
    ui.set_add_site_region_error(if bunny {
        message(field::REGION)
    } else {
        none.clone()
    });
    ui.set_add_site_password_error(message(field::PASSWORD));
    ui.set_add_site_key_error(message(field::ACCOUNT_KEY));
    ui.set_add_site_s3_errors(S3Errors {
        region: if s3 { message(field::REGION) } else { none },
        account_id: message(field::ACCOUNT_ID),
        endpoint: message(field::ENDPOINT),
        bucket: message(field::BUCKET),
        key_id: message(field::KEY_ID),
        secret: message(field::SECRET),
    });
    if !problems.is_empty() {
        // Every error moves the keyboard focus to its field, also a repeat of the same one.
        ui.set_add_site_error_serial(ui.get_add_site_error_serial().wrapping_add(1));
    }
}

/// The S3 field a failed test's words blame (the access key ID, the secret or the region), so
/// the message also goes under it.
pub fn field_blamed(text: &str) -> Option<&'static str> {
    match text {
        copy::S3_ACCESS_KEY_UNKNOWN => Some(field::KEY_ID),
        copy::S3_SECRET_WRONG => Some(field::SECRET),
        copy::S3_REGION_WRONG => Some(field::REGION),
        _ => None,
    }
}

/// Shows `error` under its field on the open sheet and clears the other two; `None` clears all.
/// Showing an error also puts the keyboard focus in that field.
#[cfg_attr(not(test), allow(dead_code))] // The I1 form's errors (see the module docs).
pub fn show_field_error(ui: &AppWindow, error: Option<&FieldError>) {
    let message_if = |wanted: bool| -> SharedString {
        match error {
            Some(error) if wanted => error.message().into(),
            _ => SharedString::new(),
        }
    };
    ui.set_add_site_zone_error(message_if(matches!(error, Some(FieldError::Zone(_)))));
    ui.set_add_site_region_error(message_if(matches!(error, Some(FieldError::Region(_)))));
    ui.set_add_site_password_error(message_if(matches!(error, Some(FieldError::Password(_)))));
    if error.is_some() {
        // Every error moves the keyboard focus to its field, also a repeat of the same one.
        ui.set_add_site_error_serial(ui.get_add_site_error_serial().wrapping_add(1));
    }
}

// ---- The publish profile a `.env` import asks for (I3) ----

/// What an imported `.env` file says to publish (`BUNNY_LOCAL_DIR`, `BUNNY_PREFIX`), kept from
/// the import until the site it describes is saved.
pub struct ImportedPublish {
    /// `BUNNY_ZONE`: only a Bunny site of this zone gets the profile.
    pub zone: String,
    pub local_dir: PathBuf,
    /// The folder of the zone, without `/` at either end; empty for its root.
    pub prefix: String,
}

/// What is done with a saved site that a `.env` import asked a profile for (Publish makes it).
type AfterImport = Rc<dyn Fn(&SiteRecord, ImportedPublish)>;

/// The import waiting for its site, and what to do then. The UI thread's own: the import and the
/// save both end there.
#[derive(Default)]
struct ImportHook {
    pending: Option<ImportedPublish>,
    then: Option<AfterImport>,
}

thread_local! {
    static IMPORT_HOOK: RefCell<ImportHook> = RefCell::default();
}

/// Sets what is done with a saved site that a `.env` import asked a profile for. Publish calls it
/// once, when the window is wired.
pub fn on_imported_site_saved(then: impl Fn(&SiteRecord, ImportedPublish) + 'static) {
    IMPORT_HOOK.with(|hook| hook.borrow_mut().then = Some(Rc::new(then)));
}

/// A `.env` file was imported into the Add Site sheet: when it names a folder, the site gets a
/// publish profile once it is saved. A later import replaces an earlier one.
pub fn expect_imported_site(import: &EnvImport) {
    let pending = import.local_dir.clone().map(|local_dir| ImportedPublish {
        zone: import.zone.clone(),
        local_dir,
        prefix: import.prefix.clone(),
    });
    IMPORT_HOOK.with(|hook| hook.borrow_mut().pending = pending);
}

/// A site was saved from the Add Site sheet. When it is the Bunny zone a waiting `.env` import
/// describes, that import's profile is made. Any waiting import is let go: it was for this save.
pub fn site_saved(site: &SiteRecord) {
    let (pending, then) = IMPORT_HOOK.with(|hook| {
        let mut hook = hook.borrow_mut();
        (hook.pending.take(), hook.then.clone())
    });
    let (Some(pending), Some(then)) = (pending, then) else {
        return;
    };
    if site.provider == ProviderKind::Bunny && site.container == pending.zone {
        then(site, pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REG: &[&str] = &["de", "sg"];

    fn form(zone: &str, region: i32, password: &str) -> AddSiteForm {
        AddSiteForm {
            name: String::new(),
            zone: zone.into(),
            region_index: region,
            password: password.into(),
        }
    }

    #[test]
    fn the_sheet_arguments_land_in_their_own_fields() {
        let f = AddSiteForm::from_sheet("My site", "my-zone", 4, "pw-1");
        assert_eq!(f.name, "My site");
        assert_eq!(f.zone, "my-zone");
        assert_eq!(f.region_index, 4);
        assert_eq!(f.password, "pw-1");
    }

    #[test]
    fn empty_zone() {
        assert_eq!(
            form("", 1, "pw").validate(REG),
            Err(FieldError::Zone("Enter the storage zone name.".into()))
        );
    }

    #[test]
    fn zone_with_spaces() {
        assert_eq!(
            form("my zone", 1, "pw").validate(REG),
            Err(FieldError::Zone("Zone names can't contain spaces.".into()))
        );
    }

    #[test]
    fn bad_region() {
        assert_eq!(
            form("z", 5, "pw").validate(REG),
            Err(FieldError::Region("Choose a region.".into()))
        );
    }

    #[test]
    fn empty_password() {
        assert_eq!(
            form("z", 1, "").validate(REG),
            Err(FieldError::Password("Enter the zone's password.".into()))
        );
    }

    #[test]
    fn a_typed_name_is_kept_and_trimmed() {
        let v = AddSiteForm {
            name: "  example-shop.com  ".into(),
            ..form("exampleshop", 0, "pw")
        }
        .validate(REG)
        .unwrap();
        assert_eq!(
            (v.name.as_str(), v.region_code.as_str()),
            ("example-shop.com", "de")
        );
    }

    #[test]
    fn the_first_problem_wins_in_field_order() {
        assert!(matches!(
            form("", 9, "").validate(REG),
            Err(FieldError::Zone(_))
        ));
        assert!(matches!(
            form("z", 9, "").validate(REG),
            Err(FieldError::Region(_))
        ));
        assert!(matches!(
            form("z", 0, "").validate(REG),
            Err(FieldError::Password(_))
        ));
    }

    #[test]
    fn no_region_chosen_and_an_empty_region_list_are_region_errors() {
        let choose = Err(FieldError::Region("Choose a region.".into()));
        assert_eq!(form("z", -1, "pw").validate(REG), choose);
        assert_eq!(form("z", 0, "pw").validate(&[]), choose);
        assert_eq!(form("z", 2, "pw").validate(REG), choose);
    }

    #[test]
    fn any_whitespace_inside_the_zone_counts_as_a_space() {
        for zone in ["my\tzone", "my\u{a0}zone", "a b"] {
            assert!(
                matches!(
                    form(zone, 0, "pw").validate(REG),
                    Err(FieldError::Zone(m)) if m.contains("spaces")
                ),
                "{zone:?}"
            );
        }
        // Whitespace around the zone is trimmed, not an error.
        assert_eq!(form("\tz \n", 0, "pw").validate(REG).unwrap().zone, "z");
    }

    #[test]
    fn a_blank_zone_is_empty_not_spaces() {
        let enter = Err(FieldError::Zone("Enter the storage zone name.".into()));
        assert_eq!(form("   ", 0, "pw").validate(REG), enter);
    }

    #[test]
    fn the_password_is_passed_on_unchanged() {
        let v = form("z", 0, " p w ").validate(REG).unwrap();
        assert_eq!(v.password, " p w ");
    }

    #[test]
    fn debug_output_never_contains_the_password() {
        let f = form("z", 0, "s3cret-pw");
        let v = f.validate(REG).unwrap();
        for text in [
            format!("{f:?}"),
            format!("{v:?}"),
            format!("{f:#?}"),
            format!("{v:#?}"),
        ] {
            assert!(!text.contains("s3cret-pw"), "{text}");
            assert!(text.contains("zone"), "{text}");
        }
    }

    #[test]
    fn name_defaults_to_zone_and_trims() {
        let v = AddSiteForm {
            name: "  ".into(),
            zone: " exampleshop ".into(),
            region_index: 1,
            password: "pw".into(),
        }
        .validate(REG)
        .unwrap();
        assert_eq!(
            (v.name.as_str(), v.zone.as_str(), v.region_code.as_str()),
            ("exampleshop", "exampleshop", "sg")
        );
    }
}

#[cfg(test)]
#[path = "add_site_form_tests.rs"]
mod form_tests;
