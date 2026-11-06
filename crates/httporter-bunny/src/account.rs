// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Bunny account API: with the account API key, list the storage zones with their region,
//! storage hostname and passwords, and the hostnames of the pull zones in front of each. This is
//! what "Sign in with Bunny account key" uses to offer the zones to add as sites; the zone
//! passwords it returns are then what [`crate::BunnyConfig`] takes. The CDN purge (`cdn.rs`)
//! goes through it too.
//!
//! The account key has full access to the Bunny account (Bunny has no scoped keys), so it is
//! handled like a password. It travels only in the `AccessKey` header, marked sensitive, to the
//! API host: redirects are never followed. `Debug` shows neither it nor the zone passwords, and
//! no error quotes it or anything the server sent that could carry a password.
//!
//! Like the storage client, this makes one attempt per call; callers own retries (the errors say
//! whether one is worth it, see [`ProviderError::is_retryable`]).

use std::fmt;
use std::time::Duration;

use httporter_core::{HttpProtocol, ProviderError, ProxyMode, TransferSettings};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::client::{
    ACCESS_KEY, Connections, ERROR_BODY_MAX, Op, build_client, effective_proxy, map_400,
    map_status, network_error, parse_retry_after, read_capped, snippet,
};
use crate::regions::storage_host;

const API_BASE: &str = "https://api.bunny.net";
/// How long one request may take, from connecting to the last byte of the answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A pull zone's `OriginType` when its origin is a storage zone.
const ORIGIN_TYPE_STORAGE_ZONE: i64 = 2;

/// HTTP/2 when offered, kept-alive connections and the Windows proxy: how [`BunnyAccount::new`]
/// connects.
fn system_connections() -> Connections {
    Connections {
        http1_only: false,
        keep_alive: true,
        proxy: ProxyMode::System,
    }
}

/// A client for the Bunny account API.
pub struct BunnyAccount {
    client: reqwest::Client,
    /// The account API key, marked sensitive.
    api_key: HeaderValue,
    /// `scheme://host[:port]` with no trailing slash.
    base: String,
}

impl BunnyAccount {
    /// A client for `https://api.bunny.net`, reaching it the way the system does (the Windows
    /// proxy settings apply). Whitespace around the key, as pasted keys often have, is ignored.
    pub fn new(api_key: String) -> Result<Self, ProviderError> {
        Self::build(api_key, None, REQUEST_TIMEOUT)
    }

    /// A client for `https://api.bunny.net` that connects as the user's Transfers settings say:
    /// their proxy (Ruling Q41), protocol and keep-alive. Whitespace around the key is ignored,
    /// as for [`Self::new`].
    pub fn with_settings(api_key: String, s: &TransferSettings) -> Result<Self, ProviderError> {
        let connections = Connections {
            http1_only: s.protocol == HttpProtocol::Http1Only,
            keep_alive: s.keep_alive,
            proxy: s.proxy.clone(),
        };
        Self::build_with(api_key, None, REQUEST_TIMEOUT, connections)
    }

    /// A client for another endpoint, for tests and local servers. Such an endpoint is never
    /// reached through the Windows proxy (Ruling P15).
    pub fn with_base_url(api_key: String, base_url: String) -> Result<Self, ProviderError> {
        Self::build(api_key, Some(base_url), REQUEST_TIMEOUT)
    }

    /// A client that connects as [`Self::new`] does (the Windows proxy, unless `base_url` is
    /// given).
    fn build(
        api_key: String,
        base_url: Option<String>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Self::build_with(api_key, base_url, timeout, system_connections())
    }

    fn build_with(
        api_key: String,
        base_url: Option<String>,
        timeout: Duration,
        connections: Connections,
    ) -> Result<Self, ProviderError> {
        // The message must not repeat the key.
        let mut api_key = HeaderValue::from_str(api_key.trim()).map_err(|_| {
            ProviderError::Permanent("the account API key cannot be sent as an HTTP header".into())
        })?;
        api_key.set_sensitive(true);

        let connections = Connections {
            proxy: effective_proxy(&connections.proxy, base_url.as_deref()),
            ..connections
        };
        // Two requests, one after the other, to one host.
        let client = build_client(&connections, 2, Some(timeout))?;
        let base = match base_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => API_BASE.to_owned(),
        };
        Ok(Self {
            client,
            api_key,
            base,
        })
    }

    /// The storage zones of the account, in the order the API gives them. Each carries the
    /// pull zones whose origin is that storage zone (`OriginType` 2 and its `StorageZoneId`),
    /// again in API order.
    ///
    /// A zone the answer describes in a way that can't be used (no password, a storage hostname
    /// that is not a plain hostname, or no hostname and a region we don't know) does not fail
    /// the call: it is left out of [`ZoneList::zones`] and reported in [`ZoneList::unusable`],
    /// so one odd zone doesn't keep the others from being added. Everything else does fail it:
    /// a rejected key, no answer, or an answer that isn't a list of zones.
    ///
    /// It asks for page 0 of the zones, which Bunny answers with all of them as a plain array
    /// (spec 5.2).
    pub async fn list_zones(&self) -> Result<ZoneList, ProviderError> {
        let raw: Vec<RawZone> = self.get("/storagezone?page=0", "storage zone list").await?;
        let mut list = ZoneList::default();
        for zone in raw {
            match check_zone(zone) {
                Ok(zone) => list.zones.push(zone),
                Err(unusable) => list.unusable.push(unusable),
            }
        }
        // With no zone to look pull zones up for, don't ask.
        if !list.zones.is_empty() {
            let pull_zones: Vec<RawPullZone> = self.get("/pullzone", "pull zone list").await?;
            for zone in &mut list.zones {
                zone.pull_zones = pull_zones
                    .iter()
                    .filter(|pull_zone| pull_zone.serves(zone.id))
                    .filter_map(RawPullZone::info)
                    .collect();
            }
        }
        Ok(list)
    }

    /// One attempt at a `GET`, its answer parsed as `T`.
    async fn get<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
        what: &str,
    ) -> Result<T, ProviderError> {
        let response = self
            .client
            .get(format!("{}{path_and_query}", self.base))
            .header(ACCESS_KEY, self.api_key.clone())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            // A redirect lands here too: it is not followed, and says nothing we pass on.
            return Err(map_status(
                status,
                parse_retry_after(response.headers()),
                Op::Account,
            ));
        }
        let body = response.bytes().await.map_err(network_error)?;
        serde_json::from_slice(&body).map_err(|error| unreadable(what, &error))
    }

    /// One attempt at a `POST` whose answer is not needed, with `json` as its body if given. A
    /// 400 says what the server said (never anything we sent but the request itself).
    pub(crate) async fn post(
        &self,
        path_and_query: &str,
        json: Option<&'static str>,
    ) -> Result<(), ProviderError> {
        let mut request = self
            .client
            .post(format!("{}{path_and_query}", self.base))
            .header(ACCESS_KEY, self.api_key.clone())
            .header(ACCEPT, "application/json");
        if let Some(json) = json {
            request = request.header(CONTENT_TYPE, "application/json").body(json);
        }
        let mut response = request.send().await.map_err(network_error)?;
        let status = response.status();
        if status == StatusCode::BAD_REQUEST {
            let body = read_capped(&mut response, ERROR_BODY_MAX)
                .await
                .unwrap_or_default();
            return Err(map_400(&body, Op::Account));
        }
        if !status.is_success() {
            return Err(map_status(
                status,
                parse_retry_after(response.headers()),
                Op::Account,
            ));
        }
        // Read to the end, so the connection can carry the next request. The work is done
        // whatever happens to the rest of the answer.
        let _ = read_capped(&mut response, ERROR_BODY_MAX).await;
        Ok(())
    }
}

/// Zone tools (§8.8.4–8.8.5, I6). T5 confirms the fields from bunny.net's API reference (B6) and
/// cites it here.
impl BunnyAccount {
    /// Creates a storage zone (`POST /storagezone`) and, when `z.pull_zone` names one, a pull zone
    /// whose origin it is (`POST /pullzone`). The zone comes back as the zone list gives it, with
    /// its pull zone.
    pub async fn create_zone(&self, _z: &NewZone) -> Result<ZoneInfo, ProviderError> {
        todo!("T5")
    }

    /// The zone's 404 handling and replication regions.
    pub async fn zone_settings(&self, _zone_id: u64) -> Result<ZoneSettings, ProviderError> {
        todo!("T5")
    }

    /// Changes the zone's 404 handling and adds replication regions (`POST /storagezone/{id}`).
    /// Regions can only be added: Bunny doesn't remove one.
    pub async fn update_zone_settings(
        &self,
        _zone_id: u64,
        _s: &ZoneSettings,
    ) -> Result<(), ProviderError> {
        todo!("T5")
    }
}

/// A storage zone to create.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewZone {
    /// Lowercase letters, digits and hyphens (checked before sending).
    pub name: String,
    /// The main region's code (`"de"`, `"sg"`, …).
    pub region: String,
    /// Replication region codes.
    pub replication: Vec<String>,
    /// The name of a pull zone to create in front of it, if any.
    pub pull_zone: Option<String>,
}

/// A storage zone's settings that HTTPorter changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ZoneSettings {
    /// The file served for a missing path (`/404.html`), if any.
    pub custom_404_path: Option<String>,
    /// Serve that file with status 200 instead of 404.
    pub rewrite_404_to_200: bool,
    /// Replication region codes.
    pub replication: Vec<String>,
}

impl fmt::Debug for BunnyAccount {
    /// Leaves the key out so a client can be logged safely.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BunnyAccount")
            .field("base", &self.base)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// What [`BunnyAccount::list_zones`] found: the zones that can be added, and those that can't.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneList {
    /// The usable zones, in the order the API gave them.
    pub zones: Vec<ZoneInfo>,
    /// The zones left out, in the order the API gave them.
    pub unusable: Vec<UnusableZone>,
}

/// A storage zone the account has but that can't be added as a site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusableZone {
    /// The zone's name, cleaned of control characters, for showing to the user.
    pub name: String,
    /// A short plain phrase: `"no password"`, `"invalid storage hostname"` or
    /// `"unknown region"`. It never contains a password or the account key.
    pub reason: String,
}

/// A storage zone of the account, with what it takes to add it as a site.
#[derive(Clone, PartialEq, Eq)]
pub struct ZoneInfo {
    pub id: u64,
    pub name: String,
    /// The region code in lower case, for example `"sg"`.
    pub region: String,
    /// The storage API hostname: the zone's own `StorageHostname`, or the region's.
    pub host: String,
    /// The zone password, which gives read and write access to the zone's files.
    pub password: String,
    /// The password for read-only access; empty when the API gave none.
    pub read_only_password: String,
    /// The pull zones serving this storage zone.
    pub pull_zones: Vec<PullZoneInfo>,
}

impl fmt::Debug for ZoneInfo {
    /// Leaves out both passwords so a zone can be logged safely.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZoneInfo")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("region", &self.region)
            .field("host", &self.host)
            .field("password", &"<redacted>")
            .field("read_only_password", &"<redacted>")
            .field("pull_zones", &self.pull_zones)
            .finish()
    }
}

/// A pull zone that serves a storage zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullZoneInfo {
    pub id: u64,
    pub name: String,
    /// Every hostname the pull zone answers to in the order the API gives them: the system one
    /// (`*.b-cdn.net`) and any custom domains.
    pub hostnames: Vec<String>,
}

/// An element of the storage zone list. Only what we use; Bunny's reference leaves fields out
/// that live answers have, and answers can have more than we know. No `Debug`: it holds the
/// passwords.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawZone {
    id: u64,
    name: String,
    password: String,
    read_only_password: Option<String>,
    region: Option<String>,
    storage_hostname: Option<String>,
}

/// An element of the pull zone list. Everything but the hostnames is optional, because a pull
/// zone of another kind (a URL origin, say) is read too, and must not fail the list.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawPullZone {
    id: Option<i64>,
    name: Option<String>,
    origin_type: Option<i64>,
    storage_zone_id: Option<i64>,
    hostnames: Option<Vec<RawHostname>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawHostname {
    value: Option<String>,
}

impl RawPullZone {
    /// Its origin is the storage zone `zone_id`.
    fn serves(&self, zone_id: u64) -> bool {
        self.origin_type == Some(ORIGIN_TYPE_STORAGE_ZONE)
            && self.storage_zone_id.and_then(|id| u64::try_from(id).ok()) == Some(zone_id)
    }

    /// `None` when it has no usable id, which nothing could address it by.
    fn info(&self) -> Option<PullZoneInfo> {
        Some(PullZoneInfo {
            id: u64::try_from(self.id?).ok()?,
            name: self.name.clone().unwrap_or_default(),
            hostnames: self
                .hostnames
                .iter()
                .flatten()
                .filter_map(|hostname| hostname.value.as_deref())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }
}

/// The zone as a [`ZoneInfo`] without its pull zones, or why it can't be used.
fn check_zone(zone: RawZone) -> Result<ZoneInfo, UnusableZone> {
    if zone.password.is_empty() {
        return Err(unusable(&zone.name, "no password"));
    }
    let region = zone.region.unwrap_or_default().trim().to_ascii_lowercase();
    let host = match zone
        .storage_hostname
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
    {
        // The password will be sent to this host, so it must be one and not a URL.
        Some(host) if is_hostname(host) => host.to_owned(),
        Some(_) => return Err(unusable(&zone.name, "invalid storage hostname")),
        None => storage_host(&region).ok_or_else(|| unusable(&zone.name, "unknown region"))?,
    };
    Ok(ZoneInfo {
        id: zone.id,
        name: zone.name,
        region,
        host,
        password: zone.password,
        read_only_password: zone.read_only_password.unwrap_or_default(),
        pull_zones: Vec::new(),
    })
}

/// Letters, digits, hyphens and dots, and not starting with a dot or hyphen: nothing that could
/// turn `https://{host}` into another address (a path, a port, credentials).
fn is_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with(['.', '-'])
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// A zone we cannot use. The name is server text, so it is cleaned like any other; the reason
/// is one of ours, and no password or hostname is repeated.
fn unusable(name: &str, reason: &str) -> UnusableZone {
    UnusableZone {
        name: snippet(name).unwrap_or_default(),
        reason: reason.to_owned(),
    }
}

fn unreadable(what: &str, error: &serde_json::Error) -> ProviderError {
    if error.is_data() {
        // Valid JSON, but not the shape of a list. Serde names the problem, never a value that
        // could be a password (those are strings, which is what it expects of them).
        ProviderError::Permanent(format!(
            "unexpected response from server (unrecognised {what}: {error})"
        ))
    } else {
        ProviderError::Permanent(format!(
            "unexpected response from server ({what} is not JSON)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn the_key_header_is_sensitive_and_the_default_endpoint_is_the_api() {
        let account = BunnyAccount::new("  key \n".into()).unwrap();

        assert!(account.api_key.is_sensitive());
        assert_eq!(account.api_key.to_str().unwrap(), "key");
        assert_eq!(account.base, "https://api.bunny.net");
    }

    #[test]
    fn a_base_url_loses_its_trailing_slash() {
        let account =
            BunnyAccount::with_base_url("key".into(), "http://127.0.0.1:1234//".into()).unwrap();

        assert_eq!(account.base, "http://127.0.0.1:1234");
    }

    #[test]
    fn only_plain_hostnames_pass() {
        for good in [
            "storage.bunnycdn.com",
            "sg.storage.bunnycdn.com",
            "a-b.example.org",
            "localhost",
        ] {
            assert!(is_hostname(good), "{good}");
        }
        for bad in [
            "",
            ".example.com",
            "-example.com",
            "example.com/path",
            "user@example.com",
            "example.com:8080",
            "exa mple.com",
            "example.com?x=1",
            "exämple.com",
            &"a".repeat(254),
        ] {
            assert!(!is_hostname(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_request_that_takes_too_long_is_a_network_error() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([]))
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let account =
            BunnyAccount::build("key".into(), Some(server.uri()), Duration::from_millis(200))
                .unwrap();

        let err = account.list_zones().await.unwrap_err();

        assert!(matches!(err, ProviderError::Network(_)), "{err:?}");
        assert!(err.is_retryable());
    }
}
