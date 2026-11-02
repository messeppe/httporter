//! The Transfers settings (Settings window, Transfers tab), stored as JSON. Every stored enum
//! is written in snake_case, so renaming a Rust variant can't break settings already saved.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::RetryPolicy;

/// The shortest request timeout used, whatever is stored.
const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Which HTTP versions a client may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpProtocol {
    /// HTTP/2 where the server offers it, otherwise HTTP/1.1.
    Http2WhenAvailable,
    Http1Only,
}

/// How requests reach the internet.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// Through the proxy set in Windows, if any.
    System,
    /// Straight out, whatever Windows says.
    None,
    /// Through the proxy at this URL, such as `http://proxy:8080`.
    Custom(String),
}

/// A custom proxy's URL is shown only when it holds no `@`. One that does may carry a user
/// name and password (perhaps with an unencoded `/`, `?` or `#` in it), so it is left out whole.
impl fmt::Debug for ProxyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str("System"),
            Self::None => f.write_str("None"),
            Self::Custom(url) if url.contains('@') => {
                f.debug_tuple("Custom").field(&"<redacted>").finish()
            }
            Self::Custom(url) => f.debug_tuple("Custom").field(url).finish(),
        }
    }
}

/// What happens to a file that already exists where a transfer would put it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Ask once per batch, in the conflict sheet.
    Ask,
    Replace,
    /// Replace only files whose contents differ.
    ReplaceIfDifferent,
    Skip,
    /// Keep the existing file and give the new one another name.
    KeepBoth,
}

/// The Transfers settings. Fields missing from stored JSON (written by an older build) take their
/// defaults.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct TransferSettings {
    /// Uploads and downloads at once, per site.
    pub simultaneous: u32,
    /// Halve the transfers at once when a server asks to slow down, then ramp back up.
    pub adaptive: bool,
    /// Retries of a failed transfer: 5, 3 or 0.
    pub retries: u8,
    /// Bytes per second across all transfers together. `None` and `Some(0)` both mean no limit
    /// ([`Self::bandwidth_limit_bytes`]).
    pub bandwidth_limit: Option<u64>,
    pub protocol: HttpProtocol,
    /// Keep connections open between files.
    pub keep_alive: bool,
    /// 120, 60 or 300. Anything below 5 is treated as 5 ([`Self::request_timeout`]).
    pub request_timeout_secs: u32,
    pub proxy: ProxyMode,
    /// Send a checksum with every upload, so the server rejects a damaged copy.
    pub verify_checksums: bool,
    pub upload_conflicts: ConflictPolicy,
    pub download_conflicts: ConflictPolicy,
}

impl Default for TransferSettings {
    /// As Settings.dc.html shows them.
    fn default() -> Self {
        Self {
            simultaneous: 32,
            adaptive: true,
            retries: 5,
            bandwidth_limit: None,
            protocol: HttpProtocol::Http2WhenAvailable,
            keep_alive: true,
            request_timeout_secs: 120,
            proxy: ProxyMode::System,
            verify_checksums: true,
            upload_conflicts: ConflictPolicy::Ask,
            download_conflicts: ConflictPolicy::Ask,
        }
    }
}

impl TransferSettings {
    /// The first `retries` of the standard waits ([`RetryPolicy::with_retries`]).
    pub fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::with_retries(self.retries)
    }

    /// How many transfers may run at once on a site whose provider allows at most `cap`:
    /// `simultaneous`, but never more than `cap` and never fewer than one.
    pub fn transfer_limit(&self, cap: usize) -> usize {
        let wanted = usize::try_from(self.simultaneous).unwrap_or(usize::MAX);
        wanted.clamp(1, cap.max(1))
    }

    /// The bandwidth limit in bytes per second, or `None` for no limit (stored as `None` or
    /// `Some(0)`).
    pub fn bandwidth_limit_bytes(&self) -> Option<u64> {
        self.bandwidth_limit.filter(|&limit| limit > 0)
    }

    /// How long one request may take: `request_timeout_secs`, but at least 5 s.
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.request_timeout_secs)).max(MIN_REQUEST_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings.dc.html, Transfers tab, as it first opens.
    #[test]
    fn transfer_settings_defaults_match_the_mockup() {
        let s = TransferSettings::default();
        assert_eq!(s.simultaneous, 32);
        assert!(s.adaptive);
        assert_eq!(s.retries, 5);
        assert_eq!(s.bandwidth_limit, None);
        assert_eq!(s.protocol, HttpProtocol::Http2WhenAvailable);
        assert!(s.keep_alive);
        assert_eq!(s.request_timeout_secs, 120);
        assert_eq!(s.proxy, ProxyMode::System);
        assert!(s.verify_checksums);
        assert_eq!(s.upload_conflicts, ConflictPolicy::Ask);
        assert_eq!(s.download_conflicts, ConflictPolicy::Ask);
    }

    #[test]
    fn retry_policy_follows_retries() {
        let with = |retries| TransferSettings {
            retries,
            ..TransferSettings::default()
        };
        assert_eq!(
            with(3).retry_policy().delays,
            [1, 2, 4].map(Duration::from_secs).to_vec()
        );
        assert!(with(0).retry_policy().delays.is_empty());
        assert_eq!(
            TransferSettings::default().retry_policy(),
            RetryPolicy::default()
        );
    }

    #[test]
    fn transfer_limit_clamps() {
        let with = |simultaneous| TransferSettings {
            simultaneous,
            ..TransferSettings::default()
        };
        assert_eq!(with(200).transfer_limit(96), 96);
        assert_eq!(with(0).transfer_limit(96), 1);
        assert_eq!(with(32).transfer_limit(128), 32);
        // A provider that reports no room at all still gets one transfer.
        assert_eq!(with(32).transfer_limit(0), 1);
    }

    #[test]
    fn settings_round_trip_json() {
        let custom = TransferSettings {
            simultaneous: 64,
            adaptive: false,
            retries: 3,
            bandwidth_limit: Some(5_000_000),
            protocol: HttpProtocol::Http1Only,
            keep_alive: false,
            request_timeout_secs: 300,
            proxy: ProxyMode::Custom("http://proxy:8080".into()),
            verify_checksums: false,
            upload_conflicts: ConflictPolicy::ReplaceIfDifferent,
            download_conflicts: ConflictPolicy::KeepBoth,
        };
        for settings in [TransferSettings::default(), custom] {
            let json = serde_json::to_string(&settings).unwrap();
            let back: TransferSettings = serde_json::from_str(&json).unwrap();
            assert_eq!(back, settings, "{json}");
        }
        for proxy in [ProxyMode::System, ProxyMode::None] {
            let json = serde_json::to_string(&proxy).unwrap();
            assert_eq!(serde_json::from_str::<ProxyMode>(&json).unwrap(), proxy);
        }
        for policy in [
            ConflictPolicy::Ask,
            ConflictPolicy::Replace,
            ConflictPolicy::ReplaceIfDifferent,
            ConflictPolicy::Skip,
            ConflictPolicy::KeepBoth,
        ] {
            let json = serde_json::to_string(&policy).unwrap();
            assert_eq!(
                serde_json::from_str::<ConflictPolicy>(&json).unwrap(),
                policy
            );
        }
    }

    /// Settings saved by an older build lack the fields added since: those take their
    /// defaults, and the rest of what the user chose is kept.
    #[test]
    fn missing_fields_take_their_defaults() {
        let s: TransferSettings = serde_json::from_str(r#"{"simultaneous": 8}"#).unwrap();
        assert_eq!(
            s,
            TransferSettings {
                simultaneous: 8,
                ..TransferSettings::default()
            }
        );
    }

    fn json<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    /// Ruling Q20: the stored names are pinned, so renaming a Rust variant can't break
    /// settings already saved.
    #[test]
    fn stored_names_are_snake_case() {
        assert_eq!(
            json(&TransferSettings::default()),
            concat!(
                r#"{"simultaneous":32,"adaptive":true,"retries":5,"bandwidth_limit":null,"#,
                r#""protocol":"http2_when_available","keep_alive":true,"#,
                r#""request_timeout_secs":120,"proxy":"system","verify_checksums":true,"#,
                r#""upload_conflicts":"ask","download_conflicts":"ask"}"#
            )
        );
        assert_eq!(json(&HttpProtocol::Http1Only), r#""http1_only""#);
        assert_eq!(json(&ProxyMode::None), r#""none""#);
        assert_eq!(
            json(&ProxyMode::Custom("http://proxy:8080".into())),
            r#"{"custom":"http://proxy:8080"}"#
        );
        assert_eq!(json(&ConflictPolicy::Replace), r#""replace""#);
        assert_eq!(
            json(&ConflictPolicy::ReplaceIfDifferent),
            r#""replace_if_different""#
        );
        assert_eq!(json(&ConflictPolicy::Skip), r#""skip""#);
        assert_eq!(json(&ConflictPolicy::KeepBoth), r#""keep_both""#);
    }

    #[test]
    fn a_bandwidth_limit_of_zero_is_no_limit() {
        let with = |bandwidth_limit| TransferSettings {
            bandwidth_limit,
            ..TransferSettings::default()
        };
        assert_eq!(with(None).bandwidth_limit_bytes(), None);
        assert_eq!(with(Some(0)).bandwidth_limit_bytes(), None);
        assert_eq!(with(Some(1)).bandwidth_limit_bytes(), Some(1));
        assert_eq!(
            with(Some(5_000_000)).bandwidth_limit_bytes(),
            Some(5_000_000)
        );
    }

    #[test]
    fn the_request_timeout_is_at_least_five_seconds() {
        let with = |request_timeout_secs| TransferSettings {
            request_timeout_secs,
            ..TransferSettings::default()
        };
        assert_eq!(
            TransferSettings::default().request_timeout(),
            Duration::from_secs(120)
        );
        assert_eq!(with(300).request_timeout(), Duration::from_secs(300));
        assert_eq!(with(5).request_timeout(), Duration::from_secs(5));
        assert_eq!(with(4).request_timeout(), Duration::from_secs(5));
        assert_eq!(with(0).request_timeout(), Duration::from_secs(5));
    }

    /// A proxy URL with an `@` anywhere may carry a password, perhaps one with an unencoded
    /// `/`, `?` or `#`: the whole URL is left out.
    #[test]
    fn proxy_debug_hides_a_password_in_the_url() {
        for url in [
            "http://proxyuser42:s3cret-pw@proxy.example:8080/x",
            "http://proxyuser42:s3c/ret?p#w@proxy.example:8080",
            "proxyuser42:s3cret-pw@proxy.example:8080",
        ] {
            let text = format!(
                "{:?}",
                TransferSettings {
                    proxy: ProxyMode::Custom(url.into()),
                    ..TransferSettings::default()
                }
            );
            assert!(text.contains(r#"proxy: Custom("<redacted>")"#), "{text}");
            assert!(!text.contains("proxyuser42"), "{text}");
            assert!(!text.contains("s3c"), "{text}");
        }

        let plain = format!("{:?}", ProxyMode::Custom("http://proxy:8080".into()));
        assert_eq!(plain, r#"Custom("http://proxy:8080")"#);
        assert_eq!(format!("{:?}", ProxyMode::System), "System");
        assert_eq!(format!("{:?}", ProxyMode::None), "None");
    }
}
