//! The Transfers settings (Settings window, Transfers tab), stored as JSON.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::RetryPolicy;

/// Which HTTP versions a client may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpProtocol {
    /// HTTP/2 where the server offers it, otherwise HTTP/1.1.
    Http2WhenAvailable,
    Http1Only,
}

/// How requests reach the internet.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyMode {
    /// Through the proxy set in Windows, if any.
    System,
    /// Straight out, whatever Windows says.
    None,
    /// Through the proxy at this URL, such as `http://proxy:8080`.
    Custom(String),
}

/// Shows a custom proxy's URL without the user name and password it may carry.
impl fmt::Debug for ProxyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str("System"),
            Self::None => f.write_str("None"),
            Self::Custom(url) => f
                .debug_tuple("Custom")
                .field(&without_userinfo(url))
                .finish(),
        }
    }
}

/// `url` with any `user:password@` before its host replaced by `***@`.
fn without_userinfo(url: &str) -> String {
    let start = url.find("://").map_or(0, |i| i + 3);
    let authority_end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| start + i);
    match url[start..authority_end].rfind('@') {
        Some(at) => format!("{}***{}", &url[..start], &url[start + at..]),
        None => url.to_owned(),
    }
}

/// What happens to a file that already exists where a transfer would put it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Bytes per second across all transfers together; `None` is no limit.
    pub bandwidth_limit: Option<u64>,
    pub protocol: HttpProtocol,
    /// Keep connections open between files.
    pub keep_alive: bool,
    /// 120, 60 or 300.
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    #[test]
    fn proxy_debug_hides_a_password_in_the_url() {
        let proxy = ProxyMode::Custom("http://proxyuser42:s3cret-pw@proxy.example:8080/x".into());
        let text = format!(
            "{:?}",
            TransferSettings {
                proxy,
                ..TransferSettings::default()
            }
        );
        assert!(!text.contains("s3cret-pw"), "{text}");
        assert!(!text.contains("proxyuser42"), "{text}");
        assert!(text.contains("proxy.example:8080"), "{text}");

        let plain = format!("{:?}", ProxyMode::Custom("http://proxy:8080".into()));
        assert_eq!(plain, r#"Custom("http://proxy:8080")"#);
        assert_eq!(format!("{:?}", ProxyMode::System), "System");
    }
}
