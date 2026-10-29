//! Storage-password store. Secrets live only in Windows Credential Manager (or, in tests,
//! in memory); they never reach SQLite, logs or `Debug` output.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use httporter_core::SiteId;

/// Service name shown in Credential Manager entries written by HTTPorter.
const SERVICE: &str = "HTTPorter";

#[derive(Debug, thiserror::Error)]
pub enum CredError {
    #[error("credential store error: {0}")]
    Keyring(#[from] keyring::Error),
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
    }
}
