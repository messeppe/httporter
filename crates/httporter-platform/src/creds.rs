//! Credential store for site passwords, S3 keys and account keys. Secrets live only in
//! Windows Credential Manager (or, in tests, in memory); they never reach SQLite, logs or
//! `Debug` output.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use httporter_core::{AccountId, ProviderKind, SiteId};

/// Service name shown in Credential Manager entries written by HTTPorter.
const SERVICE: &str = "HTTPorter";

/// Credential store failure.
///
/// Deliberately does not wrap `keyring::Error`: that type is `Debug`-printable with
/// payloads that can hold secret material (`BadEncoding` carries the raw credential
/// blob). The conversion below keeps only the error kind and non-secret labels, so both
/// `Debug` and `Display` are safe to log.
#[derive(Debug, thiserror::Error)]
pub enum CredError {
    /// The stored value is not valid text (e.g. written by another program).
    /// The raw bytes are dropped: they are the secret.
    #[error("stored credential is not valid text")]
    BadEncoding,
    /// A credential attribute exceeds a platform limit. `attribute` is a fixed label chosen
    /// by `keyring` (such as "target" or "password encoded as UTF-16"), never a value.
    #[error("credential attribute '{attribute}' exceeds the platform limit of {limit}")]
    TooLong { attribute: String, limit: u32 },
    /// The platform store failed or is inaccessible. The text is the platform error's
    /// message, which on Windows is an error code.
    #[error("credential store failure: {0}")]
    Platform(String),
    /// Any other failure, described by a fixed label.
    #[error("credential store error: {0}")]
    Other(&'static str),
}

impl From<keyring::Error> for CredError {
    fn from(e: keyring::Error) -> Self {
        use keyring::Error as K;
        match e {
            K::BadEncoding(_) => Self::BadEncoding,
            K::TooLong(attribute, limit) => Self::TooLong { attribute, limit },
            K::PlatformFailure(e) | K::NoStorageAccess(e) => Self::Platform(e.to_string()),
            K::NoEntry => Self::Other("no matching entry"),
            K::Invalid(..) => Self::Other("invalid credential attribute"),
            K::Ambiguous(_) => Self::Other("ambiguous credential match"),
            // `keyring::Error` is non-exhaustive; an unknown variant may carry anything.
            _ => Self::Other("unrecognised credential store error"),
        }
    }
}

/// Identifies one stored secret. On Windows the key string is the Credential Manager
/// target name, e.g. `HTTPorter/site/<site id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialKey(String);

impl CredentialKey {
    pub fn site(id: &SiteId) -> Self {
        Self(format!("{SERVICE}/site/{id}"))
    }

    /// The key of a provider account's secret, such as a Bunny account API key:
    /// `HTTPorter/account/<account id>`.
    pub fn account(id: &AccountId) -> Self {
        Self(format!("{SERVICE}/account/{id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The secret a site signs in with, as one credential-store value.
///
/// A Bunny site's value is the storage zone password as it is: exactly what the first release
/// stored, so existing sites keep working. An S3 site's value is a small JSON object holding
/// both halves of its key pair.
///
/// `Debug` shows no secret field, so a stray `{:?}` can't leak one.
#[derive(Clone, PartialEq, Eq)]
pub enum SiteSecret {
    BunnyPassword(String),
    S3 {
        access_key_id: String,
        secret_access_key: String,
    },
}

impl SiteSecret {
    /// The text to hand to [`CredentialStore::set`].
    pub fn to_stored(&self) -> String {
        match self {
            Self::BunnyPassword(password) => password.clone(),
            Self::S3 {
                access_key_id,
                secret_access_key,
            } => serde_json::json!({
                "access_key_id": access_key_id,
                "secret_access_key": secret_access_key,
            })
            .to_string(),
        }
    }

    /// Reads back what [`Self::to_stored`] wrote (or, for a Bunny site, what the first release
    /// wrote). The error names no part of `text`, so it is safe to log.
    pub fn from_stored(kind: ProviderKind, text: &str) -> Result<Self, CredError> {
        match kind {
            ProviderKind::Bunny => Ok(Self::BunnyPassword(text.to_owned())),
            ProviderKind::S3 => Self::parse_s3(text).ok_or(CredError::Other(
                "stored S3 credential is not in the expected form",
            )),
        }
    }

    /// An object with both key strings; anything else (also a JSON array of two strings) is
    /// not what [`Self::to_stored`] writes. Unknown extra fields are ignored. Nothing of
    /// `text` goes into an error: parse errors can quote it, and it is the secret.
    fn parse_s3(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let field = |name: &str| Some(object.get(name)?.as_str()?.to_owned());
        Some(Self::S3 {
            access_key_id: field("access_key_id")?,
            secret_access_key: field("secret_access_key")?,
        })
    }
}

// Deliberately not derived: `Debug` must never print a secret.
impl fmt::Debug for SiteSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hidden = format_args!("<redacted>");
        match self {
            Self::BunnyPassword(_) => f.debug_tuple("BunnyPassword").field(&hidden).finish(),
            Self::S3 { .. } => f
                .debug_struct("S3")
                .field("access_key_id", &hidden)
                .field("secret_access_key", &hidden)
                .finish(),
        }
    }
}

pub trait CredentialStore: Send + Sync {
    fn set(&self, key: &CredentialKey, secret: &str) -> Result<(), CredError>;
    fn get(&self, key: &CredentialKey) -> Result<Option<String>, CredError>;
    /// Deleting a key that does not exist is not an error.
    fn delete(&self, key: &CredentialKey) -> Result<(), CredError>;
}

/// Windows Credential Manager, via the `keyring` crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(key: &CredentialKey) -> Result<keyring::Entry, CredError> {
        // Target name == key, so the Credential Manager target is exactly the key string.
        Ok(keyring::Entry::new_with_target(
            key.as_str(),
            SERVICE,
            key.as_str(),
        )?)
    }
}

impl CredentialStore for KeyringStore {
    fn set(&self, key: &CredentialKey, secret: &str) -> Result<(), CredError> {
        Ok(Self::entry(key)?.set_password(secret)?)
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<String>, CredError> {
        match Self::entry(key)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete(&self, key: &CredentialKey) -> Result<(), CredError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore {
    secrets: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    fn secrets(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        // A poisoned lock only means another test thread panicked; the map is still valid.
        self.secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// Deliberately not derived: `Debug` must never print the stored secrets.
impl fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStore")
            .field("entries", &self.secrets().len())
            .finish_non_exhaustive()
    }
}

impl CredentialStore for MemoryStore {
    fn set(&self, key: &CredentialKey, secret: &str) -> Result<(), CredError> {
        self.secrets()
            .insert(key.as_str().to_owned(), secret.to_owned());
        Ok(())
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<String>, CredError> {
        Ok(self.secrets().get(key.as_str()).cloned())
    }

    fn delete(&self, key: &CredentialKey) -> Result<(), CredError> {
        self.secrets().remove(key.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httporter_core::{AccountId, ProviderKind, SiteId};

    #[test]
    fn site_key_format() {
        assert_eq!(
            CredentialKey::site(&SiteId("abc".into())).as_str(),
            "HTTPorter/site/abc"
        );
    }

    #[test]
    fn account_key_format() {
        assert_eq!(
            CredentialKey::account(&AccountId("acc-1".into())).as_str(),
            "HTTPorter/account/acc-1"
        );
        // Sites and accounts never share a key, even with the same id text.
        assert_ne!(
            CredentialKey::account(&AccountId("x".into())),
            CredentialKey::site(&SiteId("x".into()))
        );
    }

    /// Ledger 89: deleting what is not there is fine, so removing a site whose secret was
    /// never saved (or already removed) can't fail.
    #[test]
    fn deleting_a_missing_credential_is_ok() {
        let store = MemoryStore::default();
        let key = CredentialKey::account(&AccountId::new());
        store.delete(&key).unwrap();
        store.set(&key, "secret").unwrap();
        store.delete(&key).unwrap();
        store.delete(&key).unwrap();
        assert_eq!(store.get(&key).unwrap(), None);
    }

    #[test]
    fn site_secret_round_trip_and_debug_redacts() {
        let bunny = SiteSecret::BunnyPassword("pw-bunny-7f3a".into());
        // An I1 credential is the bare password: it must still read back as one.
        assert_eq!(bunny.to_stored(), "pw-bunny-7f3a");
        assert_eq!(
            SiteSecret::from_stored(ProviderKind::Bunny, "pw-bunny-7f3a").unwrap(),
            bunny
        );

        // Characters that need escaping in JSON survive.
        for (id, secret) in [
            ("AKIAEXAMPLE0001", r#"wJalr/XUtn+FEMI\K7MDENG"bPxRfiCY"#),
            (r#"id"with\quotes"#, "tab\tnewline\n"),
            ("AKIA", "näive 😀 secret"),
            ("", ""),
        ] {
            let s3 = SiteSecret::S3 {
                access_key_id: id.into(),
                secret_access_key: secret.into(),
            };
            let stored = s3.to_stored();
            assert_eq!(
                SiteSecret::from_stored(ProviderKind::S3, &stored).unwrap(),
                s3,
                "{stored}"
            );
        }

        // Debug (and so `{:?}` in logs and test failures) shows neither the key nor the secret.
        let s3 = SiteSecret::S3 {
            access_key_id: "AKIAEXAMPLE0001".into(),
            secret_access_key: "s3cr3t-value-9d2".into(),
        };
        for (secret, hidden) in [
            (&bunny, vec!["pw-bunny-7f3a"]),
            (&s3, vec!["AKIAEXAMPLE0001", "s3cr3t-value-9d2"]),
        ] {
            for text in [format!("{secret:?}"), format!("{secret:#?}")] {
                for needle in &hidden {
                    assert!(!text.contains(needle), "leaked {needle}: {text}");
                }
                assert!(text.contains("redacted"), "{text}");
            }
        }
    }

    #[test]
    fn a_malformed_stored_s3_secret_is_an_error_that_does_not_echo_it() {
        for text in [
            "",
            "just-a-password",
            "{\"access_key_id\":\"AKIA-leaky\"}",
            "{\"access_key_id\":1,\"secret_access_key\":\"s-leaky\"}",
            "[\"AKIA-leaky\",\"s-leaky\"]",
        ] {
            let err = SiteSecret::from_stored(ProviderKind::S3, text).unwrap_err();
            let shown = format!("{err:?}{err}");
            assert!(!shown.contains("leaky"), "{shown}");
            assert!(!shown.contains("just-a-password"), "{shown}");
        }
    }

    #[test]
    fn memory_roundtrip_and_delete() {
        let store = MemoryStore::default();
        let key = CredentialKey::site(&SiteId::new());
        assert_eq!(store.get(&key).unwrap(), None);
        store.set(&key, "pw-test").unwrap();
        assert_eq!(store.get(&key).unwrap().as_deref(), Some("pw-test"));
        store.set(&key, "pw-test-2").unwrap();
        assert_eq!(store.get(&key).unwrap().as_deref(), Some("pw-test-2"));
        store.delete(&key).unwrap();
        assert_eq!(store.get(&key).unwrap(), None);
        store.delete(&key).unwrap();
    }

    /// Removes the temporary Credential Manager entry even if an assertion fails.
    #[cfg(windows)]
    struct DeleteOnDrop(CredentialKey);

    #[cfg(windows)]
    impl Drop for DeleteOnDrop {
        fn drop(&mut self) {
            let _ = KeyringStore.delete(&self.0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn keyring_roundtrip_on_windows() {
        let key = CredentialKey::site(&SiteId::new());
        let _cleanup = DeleteOnDrop(key.clone());
        let s = KeyringStore;
        s.set(&key, "pw-test").unwrap();
        assert_eq!(s.get(&key).unwrap().as_deref(), Some("pw-test"));
        s.delete(&key).unwrap();
        assert_eq!(s.get(&key).unwrap(), None);
        // Deleting an entry that is already gone is fine (ledger 89).
        s.delete(&key).unwrap();

        // Same temporary entry: another program stored an odd-length (not UTF-16) blob.
        // Reading it must fail without exposing the blob in `Debug` or `Display`.
        let blob = b"odd-length-blob";
        KeyringStore::entry(&key).unwrap().set_secret(blob).unwrap();
        let result = s.get(&key);
        assert!(matches!(result, Err(CredError::BadEncoding)));
        let err = result.unwrap_err();
        assert_eq!(format!("{err:?}"), "BadEncoding");
        assert!(!format!("{err}").contains("odd-length-blob"));
    }

    /// `keyring::Error::BadEncoding` carries the raw credential blob, i.e. the secret.
    /// Neither `Debug` nor `Display` of `CredError` may expose it. (The blob prints as
    /// decimal numbers under `Debug`, so checking for the text alone is not enough.)
    #[test]
    fn bad_encoding_error_does_not_expose_secret_bytes() {
        let blob = b"secret".to_vec();
        let err = CredError::from(keyring::Error::BadEncoding(blob.clone()));
        assert!(matches!(err, CredError::BadEncoding));
        assert_eq!(format!("{err:?}"), "BadEncoding");
        for text in [format!("{err:?}"), format!("{err}")] {
            assert!(!text.contains("secret"), "leaked text: {text}");
            assert!(!text.contains(&format!("{blob:?}")), "leaked bytes: {text}");
            assert!(!text.contains("115, 101"), "leaked bytes: {text}");
        }
    }

    /// The other `keyring::Error` variants carry no secret: fixed attribute labels,
    /// platform error codes, or (for `Ambiguous`) credential metadata that is dropped.
    #[test]
    fn other_keyring_errors_map_to_safe_variants() {
        let too_long = CredError::from(keyring::Error::TooLong("target".into(), 32767));
        assert!(matches!(
            &too_long,
            CredError::TooLong { attribute, limit: 32767 } if attribute == "target"
        ));
        assert_eq!(
            too_long.to_string(),
            "credential attribute 'target' exceeds the platform limit of 32767"
        );

        let platform = CredError::from(keyring::Error::PlatformFailure("code 5".into()));
        assert!(matches!(&platform, CredError::Platform(m) if m == "code 5"));
        let no_access = CredError::from(keyring::Error::NoStorageAccess("locked".into()));
        assert!(matches!(&no_access, CredError::Platform(m) if m == "locked"));

        let invalid = keyring::Error::Invalid("target".into(), "pw-in-reason".into());
        let ambiguous = keyring::Error::Ambiguous(Vec::new());
        for e in [invalid, ambiguous, keyring::Error::NoEntry] {
            let err = CredError::from(e);
            assert!(matches!(err, CredError::Other(_)));
            assert!(!format!("{err:?}{err}").contains("pw-in-reason"));
        }
    }
}
