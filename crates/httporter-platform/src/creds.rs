// SPDX-License-Identifier: MIT OR Apache-2.0

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

    /// What the key is for, read from its form.
    pub fn kind(&self) -> CredentialKind {
        if self.0.starts_with(SITE_PREFIX) {
            CredentialKind::Site
        } else if self.0.starts_with(ACCOUNT_PREFIX) {
            CredentialKind::Account
        } else {
            CredentialKind::Other
        }
    }

    /// The site or account id the key is for; for [`CredentialKind::Other`], the whole key.
    pub fn id(&self) -> &str {
        self.0
            .strip_prefix(SITE_PREFIX)
            .or_else(|| self.0.strip_prefix(ACCOUNT_PREFIX))
            .unwrap_or(&self.0)
    }
}

const SITE_PREFIX: &str = "HTTPorter/site/";
const ACCOUNT_PREFIX: &str = "HTTPorter/account/";

/// What a stored secret belongs to: a site, a provider account, or something else HTTPorter
/// wrote under its service name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialKind {
    Site,
    Account,
    Other,
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
    /// Every target this app wrote (service "HTTPorter"), and none of another app's.
    fn keys(&self) -> Result<Vec<CredentialKey>, CredError>;
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

    /// Lists Credential Manager's generic credentials whose target starts `HTTPorter/` and keeps
    /// only those in the exact form [`Self::set`] writes them (see [`CredentialKey::from_entry`]).
    /// Anything else under that prefix is left out, and so never deleted: the caller only ever
    /// deletes what this returns.
    fn keys(&self) -> Result<Vec<CredentialKey>, CredError> {
        use windows::Win32::Foundation::ERROR_NOT_FOUND;
        use windows::Win32::Security::Credentials::{
            CRED_TYPE_GENERIC, CREDENTIALW, CredEnumerateW, CredFree,
        };
        use windows::core::PCWSTR;

        let filter: Vec<u16> = ENUM_FILTER.encode_utf16().chain(Some(0)).collect();
        let mut count = 0u32;
        let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: a NUL-terminated filter that outlives the call; `count` and `list` are written
        // by the call, and `list` is freed below with `CredFree`.
        let listed =
            unsafe { CredEnumerateW(PCWSTR(filter.as_ptr()), None, &mut count, &mut list) };
        match listed {
            Ok(()) => {}
            // Nothing under the prefix: Windows says so as an error.
            Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(Vec::new()),
            Err(e) => return Err(CredError::Platform(e.message())),
        }
        let mut keys = Vec::new();
        if !list.is_null() {
            // SAFETY: on success `list` points at `count` credential pointers, all valid until
            // `CredFree`.
            let entries = unsafe { std::slice::from_raw_parts(list, count as usize) };
            for &entry in entries {
                if entry.is_null() {
                    continue;
                }
                // SAFETY: a credential of the list, valid until `CredFree`.
                let entry = unsafe { &*entry };
                // The listing carries every secret along; none is read here, and each is wiped
                // before the memory goes back (as `keyring` does after `CredReadW`).
                if !entry.CredentialBlob.is_null() && entry.CredentialBlobSize > 0 {
                    // SAFETY: the blob is `CredentialBlobSize` bytes of the list's own buffer.
                    unsafe {
                        std::ptr::write_bytes(
                            entry.CredentialBlob,
                            0,
                            entry.CredentialBlobSize as usize,
                        );
                    }
                }
                if entry.Type != CRED_TYPE_GENERIC {
                    continue;
                }
                // SAFETY: NUL-terminated strings of the list (or null), valid until `CredFree`.
                let text = |s: windows::core::PWSTR| {
                    (!s.is_null())
                        .then(|| unsafe { s.to_string() }.ok())
                        .flatten()
                };
                let (Some(target), Some(user)) = (text(entry.TargetName), text(entry.UserName))
                else {
                    continue;
                };
                keys.extend(CredentialKey::from_entry(&target, &user));
            }
            // SAFETY: the buffer `CredEnumerateW` allocated, freed once; nothing above keeps a
            // pointer into it.
            unsafe { CredFree(list as *const std::ffi::c_void) };
        }
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        keys.dedup();
        Ok(keys)
    }
}

/// The Credential Manager filter for HTTPorter's targets (`CredEnumerateW`: a prefix and `*`).
/// Windows compares target names without regard to case, so this also lists `httporter/…`;
/// [`CredentialKey::from_entry`] then keeps only the exact form.
const ENUM_FILTER: &str = "HTTPorter/*";

/// The longest site or account id [`CredentialKey::from_entry`] accepts. Ids are UUIDs (36).
const ID_MAX: usize = 64;

impl CredentialKey {
    /// The key of a Credential Manager entry, if HTTPorter wrote it, read back by the form
    /// `keyring` 3.6.3 gives it (`windows.rs`: `WinCredential::new_with_target`, then
    /// `save_credential`). [`KeyringStore::entry`] passes the key as both the target and the
    /// user, so `keyring` writes a generic credential whose target name *and* user name are
    /// exactly `HTTPorter/site/<id>` or `HTTPorter/account/<id>` (the service name is not stored
    /// on Windows when a target is given). So an entry is HTTPorter's only when:
    ///
    /// - its target starts with `HTTPorter/site/` or `HTTPorter/account/`, in that case;
    /// - the id after it is 1 to [`ID_MAX`] ASCII letters, digits, `-` or `_` (no `/`);
    /// - its user name is its target.
    ///
    /// Anything else (another program's `httporter/…`, an entry edited by hand) is not HTTPorter's
    /// to delete.
    fn from_entry(target: &str, user: &str) -> Option<Self> {
        if user != target {
            return None;
        }
        let id = target
            .strip_prefix(SITE_PREFIX)
            .or_else(|| target.strip_prefix(ACCOUNT_PREFIX))?;
        let id_ok = !id.is_empty()
            && id.len() <= ID_MAX
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        id_ok.then(|| Self(target.to_owned()))
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

    fn keys(&self) -> Result<Vec<CredentialKey>, CredError> {
        let prefix = format!("{SERVICE}/");
        let mut keys: Vec<CredentialKey> = self
            .secrets()
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .map(|key| CredentialKey(key.clone()))
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(keys)
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

    /// Only the exact form `KeyringStore::set` writes is read back as HTTPorter's key.
    #[test]
    fn only_httporters_own_entry_form_is_a_key() {
        let id = "0b9f6c1e-2d4a-4c8e-9f1a-3b5d7e9f1a2c";
        for target in [
            format!("HTTPorter/site/{id}"),
            format!("HTTPorter/account/{id}"),
            "HTTPorter/site/site_1".to_owned(),
        ] {
            let key = CredentialKey::from_entry(&target, &target).expect(&target);
            assert_eq!(key.as_str(), target);
            assert_ne!(key.kind(), CredentialKind::Other);
        }
        let site = format!("HTTPorter/site/{id}");
        for (target, user) in [
            // Another user name: not written by `KeyringStore`.
            (site.as_str(), "someone"),
            (site.as_str(), ""),
            // Another case, prefix, kind or shape of id.
            ("httporter/site/abc", "httporter/site/abc"),
            ("HTTPORTER/site/abc", "HTTPORTER/site/abc"),
            ("HTTPorter/Site/abc", "HTTPorter/Site/abc"),
            ("HTTPorter/cdn/abc", "HTTPorter/cdn/abc"),
            ("HTTPorter/abc", "HTTPorter/abc"),
            ("HTTPorter", "HTTPorter"),
            ("HTTPorterX/site/abc", "HTTPorterX/site/abc"),
            ("Other/HTTPorter/site/abc", "Other/HTTPorter/site/abc"),
            ("HTTPorter/site/", "HTTPorter/site/"),
            ("HTTPorter/site/a/b", "HTTPorter/site/a/b"),
            ("HTTPorter/site/a b", "HTTPorter/site/a b"),
            ("HTTPorter/site/a*", "HTTPorter/site/a*"),
            ("HTTPorter/site/é", "HTTPorter/site/é"),
        ] {
            assert_eq!(CredentialKey::from_entry(target, user), None, "{target}");
        }
        let long = format!("HTTPorter/site/{}", "a".repeat(ID_MAX + 1));
        assert_eq!(CredentialKey::from_entry(&long, &long), None);
    }

    /// Credential Manager entries a test makes for itself, removed even if an assertion fails.
    #[cfg(windows)]
    struct Decoys(Vec<keyring::Entry>);

    #[cfg(windows)]
    impl Drop for Decoys {
        fn drop(&mut self) {
            for entry in &self.0 {
                let _ = entry.delete_credential();
            }
        }
    }

    /// `KeyringStore::keys` lists HTTPorter's key, and none of the look-alikes that Windows' own
    /// filter lets through (another case, another kind, another user name). Only entries made
    /// here are written and removed; the others are only listed.
    #[cfg(windows)]
    #[test]
    fn keyring_keys_lists_only_httporters_own_targets() {
        let ours = CredentialKey::site(&SiteId::new());
        let _cleanup = DeleteOnDrop(ours.clone());
        KeyringStore.set(&ours, "pw-test").unwrap();

        let id = || SiteId::new().0;
        let decoy_targets = [
            (format!("httporter/site/{}", id()), None),
            (format!("HTTPorter/decoy/{}", id()), None),
            (format!("HTTPorterDecoy/site/{}", id()), None),
            (format!("HTTPorter/site/{}", id()), Some("someone-else")),
        ];
        let mut decoys = Decoys(Vec::new());
        for (target, user) in &decoy_targets {
            let entry =
                keyring::Entry::new_with_target(target, "decoy", user.unwrap_or(target)).unwrap();
            entry.set_password("decoy").unwrap();
            decoys.0.push(entry);
        }

        let keys = KeyringStore.keys().unwrap();
        assert!(keys.contains(&ours), "HTTPorter's own key is listed");
        for (target, _) in &decoy_targets {
            assert!(
                !keys
                    .iter()
                    .any(|key| key.as_str().eq_ignore_ascii_case(target)),
                "a look-alike was listed: {target}"
            );
        }
        assert!(
            keys.iter().all(|key| key.kind() != CredentialKind::Other),
            "only site and account keys are listed"
        );
        drop(decoys);

        KeyringStore.delete(&ours).unwrap();
        assert!(!KeyringStore.keys().unwrap().contains(&ours));
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

    #[test]
    fn a_key_says_its_kind_and_id() {
        let site = CredentialKey::site(&SiteId("s-1".into()));
        assert_eq!((site.kind(), site.id()), (CredentialKind::Site, "s-1"));
        let account = CredentialKey::account(&AccountId("a-1".into()));
        assert_eq!(
            (account.kind(), account.id()),
            (CredentialKind::Account, "a-1")
        );
        let other = CredentialKey("HTTPorter/legacy".into());
        assert_eq!(
            (other.kind(), other.id()),
            (CredentialKind::Other, "HTTPorter/legacy")
        );
    }

    #[test]
    fn memory_keys_lists_only_httporters_sorted() {
        let store = MemoryStore::default();
        assert!(store.keys().unwrap().is_empty());
        let site = CredentialKey::site(&SiteId("b".into()));
        let account = CredentialKey::account(&AccountId("a".into()));
        store.set(&site, "pw").unwrap();
        store.set(&account, "key").unwrap();
        store
            .set(&CredentialKey("Other/site/x".into()), "no")
            .unwrap();
        assert_eq!(store.keys().unwrap(), [account.clone(), site]);
        store.delete(&account).unwrap();
        assert_eq!(store.keys().unwrap().len(), 1);
    }
}
