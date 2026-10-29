//! Storage-password store. Secrets live only in Windows Credential Manager (or, in tests,
//! in memory); they never reach SQLite, logs or `Debug` output.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use httporter_core::SiteId;

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

    pub fn as_str(&self) -> &str {
        &self.0
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
    use httporter_core::SiteId;

    #[test]
    fn site_key_format() {
        assert_eq!(
            CredentialKey::site(&SiteId("abc".into())).as_str(),
            "HTTPorter/site/abc"
        );
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
