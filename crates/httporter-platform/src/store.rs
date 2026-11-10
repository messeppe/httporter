// SPDX-License-Identifier: MIT OR Apache-2.0

//! Local SQLite database: saved sites, provider accounts, settings and the transfer journal.
//!
//! Holds connection details only (name, provider, container, region, host, S3 endpoint).
//! Passwords and API keys never enter this database: they live in the credential store (see
//! `creds`), keyed by site or account id.
//!
//! The `sites.container` column holds [`SiteRecord::container`]. It was called `zone` in
//! schema v1.
//!
//! Schema v3 adds the publish profiles, their history (the files of each publish too) and each
//! site's CDN link. Lists, the purge choice and the pull zones are stored as JSON.
//!
//! Schema v4 adds the persistent hash cache (local files' digests by path, size and time), the
//! sidebar's favorites and each profile's Cache-Control header. Site extras and the window's UI
//! state are JSON in the settings.
//!
//! Schema v5 (I6) adds each journal row's owner (`journal.owner`, see [`crate::journal`]), who
//! started each publish (`publish_history.source`) and each profile's I6 options
//! (`profiles.extras`, JSON [`httporter_core::ProfileExtras`]).
//!
//! The file is opened in WAL mode with a 5 s busy timeout, so the store and the transfer
//! journal ([`crate::journal`]) can each hold their own connection to it.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use httporter_core::{
    AccountId, CdnLink, CdnZone, DeleteGuard, Favorite, FileAction, FileOutcome, HISTORY_KEEP,
    HistoryFile, HistoryRecord, HistorySource, PlaceKind, ProfileId, ProfileRecord, ProviderKind,
    PublishResult, PurgeMode, PurgeResult, RemotePath, S3Preset, S3Site, SiteId, SiteRecord,
};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// How long a connection waits for another one's write to finish before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Schema migrations in order. Entry `i` upgrades the database from `user_version == i` to
/// `i + 1`. Append new entries; never edit a released one.
const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE sites (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        provider   TEXT NOT NULL,
        zone       TEXT NOT NULL,
        region     TEXT NOT NULL,
        host       TEXT NOT NULL,
        created_at TEXT NOT NULL
    )",
    // v2: S3 sites, settings, accounts and the transfer journal.
    "ALTER TABLE sites RENAME COLUMN zone TO container;
    ALTER TABLE sites ADD COLUMN s3_preset TEXT;
    ALTER TABLE sites ADD COLUMN s3_endpoint TEXT;
    ALTER TABLE sites ADD COLUMN s3_path_style INTEGER;
    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE accounts (
        id         TEXT PRIMARY KEY,
        provider   TEXT NOT NULL,
        name       TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    -- AUTOINCREMENT, not a bare INTEGER PRIMARY KEY: SQLite would otherwise hand out the
    -- highest ids again once their rows are deleted, and the journal promises never to.
    CREATE TABLE journal (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        batch      TEXT NOT NULL,
        site       TEXT NOT NULL,
        kind       TEXT NOT NULL,
        local      TEXT,
        remote     TEXT NOT NULL,
        size       INTEGER NOT NULL,
        folder     INTEGER NOT NULL,
        state      TEXT NOT NULL,
        error      TEXT,
        multipart  TEXT,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX journal_state ON journal (state);",
    // v3: publish profiles, their history and the CDN links.
    "CREATE TABLE profiles (
        id                TEXT PRIMARY KEY,
        site              TEXT NOT NULL,
        name              TEXT NOT NULL,
        local_dir         TEXT NOT NULL,
        remote_dir        TEXT NOT NULL,
        skip_rules        TEXT NOT NULL,
        remove_extras     INTEGER NOT NULL,
        purge             TEXT NOT NULL,
        guard_files       INTEGER NOT NULL,
        guard_percent     INTEGER NOT NULL,
        created_at        TEXT NOT NULL,
        last_published_at TEXT
    );
    CREATE TABLE publish_history (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        profile        TEXT NOT NULL,
        started_at     TEXT NOT NULL,
        finished_at    TEXT NOT NULL,
        result         TEXT NOT NULL,
        uploaded       INTEGER NOT NULL,
        deleted        INTEGER NOT NULL,
        skipped        INTEGER NOT NULL,
        failed         INTEGER NOT NULL,
        bytes          INTEGER NOT NULL,
        purge          TEXT NOT NULL,
        verified       INTEGER NOT NULL,
        remote_files   INTEGER NOT NULL,
        remote_folders INTEGER NOT NULL,
        regions        INTEGER NOT NULL
    );
    CREATE INDEX publish_history_profile ON publish_history (profile, id);
    CREATE TABLE publish_files (
        history INTEGER NOT NULL,
        path    TEXT NOT NULL,
        action  TEXT NOT NULL,
        outcome TEXT NOT NULL,
        error   TEXT,
        size    INTEGER NOT NULL
    );
    CREATE INDEX publish_files_history ON publish_files (history);
    CREATE TABLE cdn_links (
        site       TEXT PRIMARY KEY,
        account    TEXT NOT NULL,
        zones      TEXT NOT NULL,
        checked_at TEXT NOT NULL
    );",
    // v4: the persistent hash cache, the sidebar's favorites and each profile's Cache-Control.
    "CREATE TABLE hash_cache (
        path     TEXT PRIMARY KEY,
        size     INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL,
        sha256   BLOB NOT NULL,
        md5      BLOB NOT NULL,
        used_at  TEXT NOT NULL
    );
    CREATE TABLE favorites (
        id     INTEGER PRIMARY KEY AUTOINCREMENT,
        kind   TEXT NOT NULL,
        name   TEXT NOT NULL,
        local  TEXT,
        site   TEXT,
        remote TEXT
    );
    ALTER TABLE profiles ADD COLUMN cache_control TEXT;",
    // v5 (I6): who owns each journal row (the window and the CLI can run at once; NULL = an old
    // run, gone), who started each publish, and each profile's I6 options as JSON.
    "ALTER TABLE journal ADD COLUMN owner TEXT;
    ALTER TABLE publish_history ADD COLUMN source TEXT NOT NULL DEFAULT 'app';
    ALTER TABLE profiles ADD COLUMN extras TEXT NOT NULL DEFAULT '{}';",
];

/// The Transfers settings (Settings window, Transfers tab) are stored under this key.
pub const SETTINGS_TRANSFERS: &str = "transfers";

/// The settings key of a site's own conflict policy: `conflicts/upload/<id>` for uploads,
/// `conflicts/download/<id>` for downloads.
pub fn site_conflicts_key(id: &SiteId, upload: bool) -> String {
    let direction = if upload { "upload" } else { "download" };
    format!("conflicts/{direction}/{id}")
}

/// The settings key of a site's [`httporter_core::SiteExtras`]: `site_extras:<id>`.
pub fn site_extras_key(id: &SiteId) -> String {
    format!("site_extras:{id}")
}

/// Database failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite rejected the operation (includes adding a site whose id already exists).
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    /// The database folder could not be created.
    #[error("could not create the database folder: {0}")]
    Io(#[from] std::io::Error),
    /// The database was written by a newer version of HTTPorter.
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    NewerSchema { found: u32, supported: u32 },
    /// A stored row holds a value this build cannot interpret.
    #[error("stored data is invalid: {0}")]
    Corrupt(String),
    /// The site to update is not in the database.
    #[error("that site is not in the database")]
    NotFound,
    /// The publish profile to update is not in the database.
    #[error("that publish profile is not in the database")]
    ProfileNotFound,
    /// A site record whose provider and S3 details disagree: refused rather than saved wrong.
    #[error("the site's details are inconsistent: {0}")]
    InvalidSite(&'static str),
    /// A value could not be turned into text for storage.
    #[error("could not encode the value for storage: {0}")]
    Encode(String),
}

/// A stored setting read back: the whole value when it reads; otherwise each top-level field
/// that reads over the defaults, so that one damaged or retyped field costs only itself (ledger
/// 118); the defaults when the text is not JSON at all.
fn decode_setting<T: DeserializeOwned + Serialize + Default>(text: &str) -> T {
    use serde_json::Value;

    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return T::default();
    };
    if let Ok(read) = T::deserialize(&value) {
        return read;
    }
    let (Value::Object(stored), Ok(Value::Object(mut kept))) =
        (value, serde_json::to_value(T::default()))
    else {
        return T::default();
    };
    for (field, stored_value) in stored {
        let mut trial = kept.clone();
        trial.insert(field.clone(), stored_value.clone());
        if T::deserialize(&Value::Object(trial)).is_ok() {
            kept.insert(field, stored_value);
        }
    }
    T::deserialize(&Value::Object(kept)).unwrap_or_default()
}

/// `%APPDATA%\HTTPorter\httporter.db` (the per-user roaming config folder), or under
/// `HTTPORTER_DATA_DIR` (see [`crate::paths`]).
///
/// If the platform reports no config folder the path is relative to the working directory.
pub fn default_db_path() -> PathBuf {
    crate::paths::roaming_dir().join("httporter.db")
}

/// The sites that could be read, and how many rows could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteList {
    /// Ordered by name ignoring ASCII case.
    pub sites: Vec<SiteRecord>,
    /// Rows left out because this build cannot make sense of them (an unknown provider, an
    /// unreadable date, S3 details that are missing or out of place).
    pub unreadable: usize,
}

/// A saved provider account, such as a Bunny account whose API key lets the app list zones.
/// It holds no secret: the key is in the credential store under [`crate::CredentialKey::account`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    pub id: AccountId,
    pub provider: ProviderKind,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// The database of sites, accounts and settings.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating, with parent folders, if needed) the database at `path` and brings its
    /// schema up to date.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            conn: open_connection(path)?,
        })
    }

    /// A throwaway database that lives only as long as the returned `Store`.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Ok(Self {
            conn: open_in_memory_connection()?,
        })
    }

    /// Inserts a site. Fails if a site with the same id already exists, and with
    /// [`StoreError::InvalidSite`] if its provider and S3 details disagree.
    pub fn add_site(&self, site: &SiteRecord) -> Result<(), StoreError> {
        let cols = SiteColumns::of(site)?;
        self.conn.execute(
            "INSERT INTO sites (id, name, provider, container, region, host, created_at,
                                s3_preset, s3_endpoint, s3_path_style)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                site.id.0,
                site.name,
                cols.provider,
                site.container,
                site.region,
                site.host,
                stamp(site.created_at),
                cols.s3_preset,
                cols.s3_endpoint,
                cols.s3_path_style,
            ],
        )?;
        Ok(())
    }

    /// Replaces every stored field of the site with the same id (all but the id itself).
    /// [`StoreError::NotFound`] if there is no such site, and [`StoreError::InvalidSite`] if
    /// its provider and S3 details disagree.
    pub fn update_site(&self, site: &SiteRecord) -> Result<(), StoreError> {
        let cols = SiteColumns::of(site)?;
        let changed = self.conn.execute(
            "UPDATE sites
             SET name = ?2, provider = ?3, container = ?4, region = ?5, host = ?6,
                 created_at = ?7, s3_preset = ?8, s3_endpoint = ?9, s3_path_style = ?10
             WHERE id = ?1",
            params![
                site.id.0,
                site.name,
                cols.provider,
                site.container,
                site.region,
                site.host,
                stamp(site.created_at),
                cols.s3_preset,
                cols.s3_endpoint,
                cols.s3_path_style,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// All the sites that can be read, ordered by name ignoring ASCII case. A row that cannot
    /// be read is left out and counted in [`SiteList::unreadable`], so one bad row never
    /// hides the others. A database failure is still an error.
    pub fn list_sites(&self) -> Result<SiteList, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, provider, container, region, host, created_at,
                    s3_preset, s3_endpoint, s3_path_style
             FROM sites ORDER BY name COLLATE NOCASE, id",
        )?;
        let mut rows = stmt.query([])?;
        let mut list = SiteList::default();
        while let Some(row) = rows.next()? {
            let record = match RawSite::read(row) {
                Ok(raw) => raw.into_record().ok(),
                Err(e) if is_row_decode_error(&e) => None,
                Err(e) => return Err(e.into()),
            };
            match record {
                Some(record) => list.sites.push(record),
                None => list.unreadable += 1,
            }
        }
        Ok(list)
    }

    /// Deletes a site, with its publish profiles, their history, its CDN link, its favorites, its
    /// extras and its remembered conflict choices, in one transaction. Removing an id that is not
    /// stored is not an error.
    pub fn remove_site(&self, id: &SiteId) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM publish_files WHERE history IN
                 (SELECT h.id FROM publish_history h JOIN profiles p ON p.id = h.profile
                  WHERE p.site = ?1)",
            params![id.0],
        )?;
        tx.execute(
            "DELETE FROM publish_history WHERE profile IN
                 (SELECT id FROM profiles WHERE site = ?1)",
            params![id.0],
        )?;
        tx.execute("DELETE FROM profiles WHERE site = ?1", params![id.0])?;
        tx.execute("DELETE FROM cdn_links WHERE site = ?1", params![id.0])?;
        tx.execute("DELETE FROM favorites WHERE site = ?1", params![id.0])?;
        for key in [
            site_extras_key(id),
            site_conflicts_key(id, true),
            site_conflicts_key(id, false),
        ] {
            tx.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        }
        tx.execute("DELETE FROM sites WHERE id = ?1", params![id.0])?;
        tx.commit()?;
        Ok(())
    }

    /// The setting stored under `key`, or `T::default()` when there is none or it cannot be
    /// read (missing, damaged, written in a shape this build no longer understands, or the
    /// database failed). A setting must never stop the app starting. Never write back what this
    /// returned: after a failed read it is the defaults, which would replace what is stored. Use
    /// [`Self::update_settings`], which writes only what changed (ledger 138, 144).
    pub fn settings<T: DeserializeOwned + Serialize + Default>(&self, key: &str) -> T {
        self.try_settings(key).unwrap_or_default()
    }

    /// The setting stored under `key`: `T::default()` when there is none, and the database's
    /// error when it could not be read (busy past the timeout, a disk error), which may pass.
    /// A stored value that is damaged loses only what is damaged (ledger 118): each top-level
    /// field that can't be read takes its default, and the others are kept.
    pub fn try_settings<T: DeserializeOwned + Serialize + Default>(
        &self,
        key: &str,
    ) -> Result<T, StoreError> {
        let text = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(text.map(|text| decode_setting(&text)).unwrap_or_default())
    }

    /// Writes to the setting under `key` only the top-level fields that differ between `before`
    /// (what the caller last read or wrote) and `after`, over what is stored now, read in the
    /// same transaction: the other fields stay as stored, even when the caller's copy of them is
    /// only the defaults of a read that failed (ledger 138, 144). With nothing readable stored,
    /// or a value that is not a set of fields, `after` is stored whole. A read that fails writes
    /// nothing.
    pub fn update_settings<T: Serialize>(
        &self,
        key: &str,
        before: &T,
        after: &T,
    ) -> Result<(), StoreError> {
        let encode =
            |value: &T| serde_json::to_value(value).map_err(|e| StoreError::Encode(e.to_string()));
        let (before, after) = (encode(before)?, encode(after)?);
        let tx = self.conn.unchecked_transaction()?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        let stored = stored.and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let merged = match (stored, &before, after) {
            (
                Some(serde_json::Value::Object(mut stored)),
                serde_json::Value::Object(before),
                serde_json::Value::Object(after),
            ) => {
                for (field, value) in after {
                    if before.get(&field) != Some(&value) {
                        stored.insert(field, value);
                    }
                }
                serde_json::Value::Object(stored)
            }
            (_, _, after) => after,
        };
        tx.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, merged.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Stores `value` as JSON under `key`, replacing what was there.
    pub fn put_settings<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StoreError> {
        let text = serde_json::to_string(value).map_err(|e| StoreError::Encode(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, text],
        )?;
        Ok(())
    }

    /// Forgets the setting stored under `key`. Removing a key that is not stored is not an
    /// error.
    pub fn remove_setting(&self, key: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM settings WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Inserts an account. Fails if an account with the same id already exists.
    pub fn add_account(&self, account: &AccountRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO accounts (id, provider, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                account.id.0,
                provider_to_text(account.provider),
                account.name,
                stamp(account.created_at),
            ],
        )?;
        Ok(())
    }

    /// The accounts, ordered by name ignoring ASCII case. A row this build cannot read is
    /// left out.
    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, provider, name, created_at FROM accounts
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let mut rows = stmt.query([])?;
        let mut accounts = Vec::new();
        while let Some(row) = rows.next()? {
            match RawAccount::read(row) {
                Ok(raw) => accounts.extend(raw.into_record()),
                Err(e) if is_row_decode_error(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(accounts)
    }

    /// Deletes an account. Removing an id that is not stored is not an error.
    pub fn remove_account(&self, id: &AccountId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM accounts WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    #[cfg(test)]
    fn column_names(&self, table: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
        let names = stmt
            .query_map(params![table], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }
}

/// Publish profiles, their history and the CDN links (schema v3). `remove_site` also deletes the
/// site's profiles, their history and its CDN link, in one transaction.
impl Store {
    /// Inserts a profile. Fails if a profile with the same id already exists.
    pub fn add_profile(&self, p: &ProfileRecord) -> Result<(), StoreError> {
        let cols = ProfileColumns::of(p)?;
        self.conn.execute(
            "INSERT INTO profiles (id, site, name, local_dir, remote_dir, skip_rules,
                                   remove_extras, purge, guard_files, guard_percent,
                                   created_at, last_published_at, cache_control, extras)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                p.id.0,
                p.site.0,
                p.name,
                cols.local_dir,
                p.remote_dir.as_str(),
                cols.skip_rules,
                p.remove_extras,
                cols.purge,
                int(p.guard.files)?,
                p.guard.percent,
                stamp(p.created_at),
                p.last_published_at.map(stamp),
                p.cache_control,
                cols.extras,
            ],
        )?;
        Ok(())
    }

    /// Replaces every stored field of the profile with the same id but the id and
    /// `last_published_at`, which only [`Self::add_history`] moves: an Edit Profile opened before
    /// a publish and saved after it must not put the old time back.
    /// [`StoreError::ProfileNotFound`] if there is no such profile.
    pub fn update_profile(&self, p: &ProfileRecord) -> Result<(), StoreError> {
        let cols = ProfileColumns::of(p)?;
        let changed = self.conn.execute(
            "UPDATE profiles
             SET site = ?2, name = ?3, local_dir = ?4, remote_dir = ?5, skip_rules = ?6,
                 remove_extras = ?7, purge = ?8, guard_files = ?9, guard_percent = ?10,
                 created_at = ?11, cache_control = ?12, extras = ?13
             WHERE id = ?1",
            params![
                p.id.0,
                p.site.0,
                p.name,
                cols.local_dir,
                p.remote_dir.as_str(),
                cols.skip_rules,
                p.remove_extras,
                cols.purge,
                int(p.guard.files)?,
                p.guard.percent,
                stamp(p.created_at),
                p.cache_control,
                cols.extras,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::ProfileNotFound);
        }
        Ok(())
    }

    /// Deletes the profile and its history. Removing an id that is not stored is not an error.
    pub fn remove_profile(&self, id: &ProfileId) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM publish_files WHERE history IN
                 (SELECT id FROM publish_history WHERE profile = ?1)",
            params![id.0],
        )?;
        tx.execute(
            "DELETE FROM publish_history WHERE profile = ?1",
            params![id.0],
        )?;
        tx.execute("DELETE FROM profiles WHERE id = ?1", params![id.0])?;
        tx.commit()?;
        Ok(())
    }

    /// The profiles, ordered by name ignoring ASCII case. A row this build cannot read is left
    /// out.
    pub fn list_profiles(&self) -> Result<Vec<ProfileRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, site, name, local_dir, remote_dir, skip_rules, remove_extras, purge,
                    guard_files, guard_percent, created_at, last_published_at, cache_control,
                    extras
             FROM profiles ORDER BY name COLLATE NOCASE, id",
        )?;
        let mut rows = stmt.query([])?;
        let mut profiles = Vec::new();
        while let Some(row) = rows.next()? {
            match RawProfile::read(row) {
                Ok(raw) => profiles.extend(raw.into_record()),
                Err(e) if is_row_decode_error(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(profiles)
    }

    /// Stores a publish and its files, sets the profile's `last_published_at` (to when the
    /// publish finished) and trims the profile's history to [`HISTORY_KEEP`], all in one
    /// transaction. `h.id` is ignored. Returns the new id.
    pub fn add_history(&self, h: &HistoryRecord) -> Result<i64, StoreError> {
        let purge = to_json(&h.purge)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO publish_history (profile, started_at, finished_at, result, uploaded,
                                          deleted, skipped, failed, bytes, purge, verified,
                                          remote_files, remote_folders, regions, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                h.profile.0,
                stamp(h.started_at),
                stamp(h.finished_at),
                result_to_text(h.result),
                int(h.uploaded)?,
                int(h.deleted)?,
                int(h.skipped)?,
                int(h.failed)?,
                int(h.bytes)?,
                purge,
                h.verified,
                int(h.remote_files)?,
                int(h.remote_folders)?,
                h.regions,
                h.source.as_str(),
            ],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut insert = tx.prepare(
                "INSERT INTO publish_files (history, path, action, outcome, error, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for file in &h.files {
                let (outcome, error) = outcome_to_columns(&file.outcome);
                insert.execute(params![
                    id,
                    file.path.as_str(),
                    action_to_text(file.action),
                    outcome,
                    error,
                    int(file.size)?,
                ])?;
            }
        }
        tx.execute(
            "UPDATE profiles SET last_published_at = ?2 WHERE id = ?1",
            params![h.profile.0, stamp(h.finished_at)],
        )?;
        // Everything older than the newest HISTORY_KEEP publishes of this profile.
        let keep = HISTORY_KEEP as i64;
        tx.execute(
            "DELETE FROM publish_files WHERE history IN
                 (SELECT id FROM publish_history WHERE profile = ?1
                  ORDER BY id DESC LIMIT -1 OFFSET ?2)",
            params![h.profile.0, keep],
        )?;
        tx.execute(
            "DELETE FROM publish_history WHERE id IN
                 (SELECT id FROM publish_history WHERE profile = ?1
                  ORDER BY id DESC LIMIT -1 OFFSET ?2)",
            params![h.profile.0, keep],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Replaces what a publish's purge did (Retry Purge). A publish no longer stored (trimmed,
    /// or its profile deleted) is not an error: there is nothing left to update.
    pub fn update_history_purge(&self, id: i64, purge: &PurgeResult) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE publish_history SET purge = ?2 WHERE id = ?1",
            params![id, to_json(purge)?],
        )?;
        Ok(())
    }

    /// Newest first, without files. `search` (trimmed; empty matches all) is a case-insensitive
    /// LIKE on the result, the stored date text and any file path. A row this build cannot read
    /// is left out.
    pub fn list_history(
        &self,
        profile: &ProfileId,
        search: &str,
    ) -> Result<Vec<HistoryRecord>, StoreError> {
        let search = search.trim();
        let pattern = format!("%{}%", escape_like(search));
        let mut stmt = self.conn.prepare(
            "SELECT h.id, h.profile, h.started_at, h.finished_at, h.result, h.uploaded,
                    h.deleted, h.skipped, h.failed, h.bytes, h.purge, h.verified,
                    h.remote_files, h.remote_folders, h.regions, h.source
             FROM publish_history h
             WHERE h.profile = ?1
               AND (?2 = ''
                    OR h.result LIKE ?3 ESCAPE '\\'
                    OR h.started_at LIKE ?3 ESCAPE '\\'
                    OR h.finished_at LIKE ?3 ESCAPE '\\'
                    OR EXISTS (SELECT 1 FROM publish_files f
                               WHERE f.history = h.id AND f.path LIKE ?3 ESCAPE '\\'))
             ORDER BY h.id DESC",
        )?;
        let mut rows = stmt.query(params![profile.0, search, pattern])?;
        let mut history = Vec::new();
        while let Some(row) = rows.next()? {
            match RawHistory::read(row) {
                Ok(raw) => history.extend(raw.into_record()),
                Err(e) if is_row_decode_error(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(history)
    }

    /// The files of a publish, in the order they were stored. A row this build cannot read is
    /// left out.
    pub fn history_files(&self, id: i64) -> Result<Vec<HistoryFile>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, action, outcome, error, size FROM publish_files
             WHERE history = ?1 ORDER BY rowid",
        )?;
        let mut rows = stmt.query(params![id])?;
        let mut files = Vec::new();
        while let Some(row) = rows.next()? {
            match RawHistoryFile::read(row) {
                Ok(raw) => files.extend(raw.into_file()),
                Err(e) if is_row_decode_error(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(files)
    }

    /// The site's CDN link. `None` when there is none, or when the stored one cannot be read
    /// (the app then looks the pull zones up again).
    pub fn cdn_link(&self, site: &SiteId) -> Result<Option<CdnLink>, StoreError> {
        let raw = self
            .conn
            .query_row(
                "SELECT account, zones, checked_at FROM cdn_links WHERE site = ?1",
                params![site.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0),
                        row.get::<_, String>(1),
                        row.get::<_, String>(2),
                    ))
                },
            )
            .optional()?;
        let Some((Ok(account), Ok(zones), Ok(checked_at))) = raw else {
            return Ok(None);
        };
        let link = serde_json::from_str::<Vec<CdnZone>>(&zones)
            .ok()
            .zip(parse_stamp(&checked_at).ok())
            .map(|(zones, checked_at)| CdnLink {
                account: AccountId(account),
                zones,
                checked_at,
            });
        Ok(link)
    }

    /// Stores the site's CDN link, replacing the one it had.
    pub fn put_cdn_link(&self, site: &SiteId, link: &CdnLink) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO cdn_links (site, account, zones, checked_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (site) DO UPDATE SET account = excluded.account,
                 zones = excluded.zones, checked_at = excluded.checked_at",
            params![
                site.0,
                link.account.0,
                to_json(&link.zones)?,
                stamp(link.checked_at)
            ],
        )?;
        Ok(())
    }

    /// Forgets every CDN link that purges with `account`'s key.
    pub fn remove_cdn_links_for(&self, account: &AccountId) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM cdn_links WHERE account = ?1",
            params![account.0],
        )?;
        Ok(())
    }
}

/// The profile columns that are not stored as they are.
struct ProfileColumns<'a> {
    local_dir: &'a str,
    skip_rules: String,
    purge: String,
    extras: String,
}

impl<'a> ProfileColumns<'a> {
    fn of(p: &'a ProfileRecord) -> Result<Self, StoreError> {
        Ok(Self {
            // A Windows path can hold text that is not Unicode; such a folder is refused rather
            // than saved as some other folder.
            local_dir: p.local_dir.to_str().ok_or_else(|| {
                StoreError::Encode("the folder's name is not valid Unicode".into())
            })?,
            skip_rules: to_json(&p.skip_rules)?,
            purge: to_json(&p.purge)?,
            extras: to_json(&p.extras)?,
        })
    }
}

/// A `profiles` row as stored.
struct RawProfile {
    id: String,
    site: String,
    name: String,
    local_dir: String,
    remote_dir: String,
    skip_rules: String,
    remove_extras: bool,
    purge: String,
    guard_files: i64,
    guard_percent: u8,
    created_at: String,
    last_published_at: Option<String>,
    cache_control: Option<String>,
    extras: String,
}

impl RawProfile {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            site: row.get(1)?,
            name: row.get(2)?,
            local_dir: row.get(3)?,
            remote_dir: row.get(4)?,
            skip_rules: row.get(5)?,
            remove_extras: row.get(6)?,
            purge: row.get(7)?,
            guard_files: row.get(8)?,
            guard_percent: row.get(9)?,
            created_at: row.get(10)?,
            last_published_at: row.get(11)?,
            cache_control: row.get(12)?,
            extras: row.get(13)?,
        })
    }

    /// `None` when a column holds something this build does not understand.
    fn into_record(self) -> Option<ProfileRecord> {
        Some(ProfileRecord {
            id: ProfileId(self.id),
            site: SiteId(self.site),
            name: self.name,
            local_dir: PathBuf::from(self.local_dir),
            remote_dir: RemotePath::parse(&self.remote_dir).ok()?,
            skip_rules: serde_json::from_str(&self.skip_rules).ok()?,
            remove_extras: self.remove_extras,
            purge: serde_json::from_str::<PurgeMode>(&self.purge).ok()?,
            guard: DeleteGuard {
                files: count(self.guard_files)?,
                percent: self.guard_percent,
            },
            created_at: parse_stamp(&self.created_at).ok()?,
            last_published_at: match self.last_published_at {
                Some(text) => Some(parse_stamp(&text).ok()?),
                None => None,
            },
            cache_control: self.cache_control.filter(|value| !value.trim().is_empty()),
            // Options this build can't read are off rather than the profile lost: each one
            // missing reads as its default, and text that isn't JSON at all as all of them.
            extras: serde_json::from_str(&self.extras).unwrap_or_default(),
        })
    }
}

/// A `publish_history` row as stored.
struct RawHistory {
    id: i64,
    profile: String,
    started_at: String,
    finished_at: String,
    result: String,
    uploaded: i64,
    deleted: i64,
    skipped: i64,
    failed: i64,
    bytes: i64,
    purge: String,
    verified: bool,
    remote_files: i64,
    remote_folders: i64,
    regions: u32,
    source: String,
}

impl RawHistory {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            profile: row.get(1)?,
            started_at: row.get(2)?,
            finished_at: row.get(3)?,
            result: row.get(4)?,
            uploaded: row.get(5)?,
            deleted: row.get(6)?,
            skipped: row.get(7)?,
            failed: row.get(8)?,
            bytes: row.get(9)?,
            purge: row.get(10)?,
            verified: row.get(11)?,
            remote_files: row.get(12)?,
            remote_folders: row.get(13)?,
            regions: row.get(14)?,
            source: row.get(15)?,
        })
    }

    /// Without its files. `None` when a column holds something this build does not understand.
    fn into_record(self) -> Option<HistoryRecord> {
        Some(HistoryRecord {
            id: self.id,
            profile: ProfileId(self.profile),
            started_at: parse_stamp(&self.started_at).ok()?,
            finished_at: parse_stamp(&self.finished_at).ok()?,
            result: result_from_text(&self.result)?,
            uploaded: count(self.uploaded)?,
            deleted: count(self.deleted)?,
            skipped: count(self.skipped)?,
            failed: count(self.failed)?,
            bytes: count(self.bytes)?,
            purge: serde_json::from_str(&self.purge).ok()?,
            verified: self.verified,
            remote_files: count(self.remote_files)?,
            remote_folders: count(self.remote_folders)?,
            regions: self.regions,
            files: Vec::new(),
            source: HistorySource::parse(&self.source),
        })
    }
}

/// A `publish_files` row as stored.
struct RawHistoryFile {
    path: String,
    action: String,
    outcome: String,
    error: Option<String>,
    size: i64,
}

impl RawHistoryFile {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            path: row.get(0)?,
            action: row.get(1)?,
            outcome: row.get(2)?,
            error: row.get(3)?,
            size: row.get(4)?,
        })
    }

    /// `None` when a column holds something this build does not understand.
    fn into_file(self) -> Option<HistoryFile> {
        Some(HistoryFile {
            path: RemotePath::parse(&self.path).ok()?,
            action: action_from_text(&self.action)?,
            outcome: outcome_from_columns(&self.outcome, self.error)?,
            size: count(self.size)?,
        })
    }
}

/// The stored names of a publish's result, which `list_history` searches. They are part of the
/// file format: never rename one.
fn result_to_text(result: PublishResult) -> &'static str {
    match result {
        PublishResult::Ok => "ok",
        PublishResult::Partial => "partial",
        PublishResult::Failed => "failed",
        PublishResult::Cancelled => "cancelled",
    }
}

fn result_from_text(text: &str) -> Option<PublishResult> {
    match text {
        "ok" => Some(PublishResult::Ok),
        "partial" => Some(PublishResult::Partial),
        "failed" => Some(PublishResult::Failed),
        "cancelled" => Some(PublishResult::Cancelled),
        _ => None,
    }
}

fn action_to_text(action: FileAction) -> &'static str {
    match action {
        FileAction::Upload => "upload",
        FileAction::Delete => "delete",
    }
}

fn action_from_text(text: &str) -> Option<FileAction> {
    match text {
        "upload" => Some(FileAction::Upload),
        "delete" => Some(FileAction::Delete),
        _ => None,
    }
}

/// A file's `outcome` and `error` columns: the error is why the file failed or was not tried.
fn outcome_to_columns(outcome: &FileOutcome) -> (&'static str, Option<&str>) {
    match outcome {
        FileOutcome::Done => ("done", None),
        FileOutcome::Failed(error) => ("failed", Some(error)),
        FileOutcome::Cancelled => ("cancelled", None),
        FileOutcome::NotRun(why) => ("not_run", Some(why)),
    }
}

fn outcome_from_columns(outcome: &str, error: Option<String>) -> Option<FileOutcome> {
    match outcome {
        "done" => Some(FileOutcome::Done),
        "failed" => Some(FileOutcome::Failed(error.unwrap_or_default())),
        "cancelled" => Some(FileOutcome::Cancelled),
        "not_run" => Some(FileOutcome::NotRun(error.unwrap_or_default())),
        _ => None,
    }
}

/// A count as stored: SQLite's integers are signed.
fn int(n: u64) -> Result<i64, StoreError> {
    i64::try_from(n).map_err(|_| StoreError::Encode(format!("{n} is too large to save")))
}

/// A stored count, `None` if it is negative.
fn count(n: i64) -> Option<u64> {
    u64::try_from(n).ok()
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|e| StoreError::Encode(e.to_string()))
}

/// `text` with LIKE's wildcards and its escape character (`\`) escaped, so that it matches only
/// itself.
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// The columns a site's provider and S3 details are stored in.
struct SiteColumns<'a> {
    provider: &'static str,
    s3_preset: Option<&'static str>,
    s3_endpoint: Option<&'a str>,
    s3_path_style: Option<bool>,
}

impl<'a> SiteColumns<'a> {
    /// Refuses a record whose provider and S3 details disagree.
    fn of(site: &'a SiteRecord) -> Result<Self, StoreError> {
        match (site.provider, &site.s3) {
            (ProviderKind::Bunny, None) => Ok(Self {
                provider: provider_to_text(ProviderKind::Bunny),
                s3_preset: None,
                s3_endpoint: None,
                s3_path_style: None,
            }),
            (ProviderKind::S3, Some(s3)) => Ok(Self {
                provider: provider_to_text(ProviderKind::S3),
                s3_preset: Some(preset_to_text(s3.preset)),
                s3_endpoint: Some(&s3.endpoint),
                s3_path_style: Some(s3.path_style),
            }),
            (ProviderKind::Bunny, Some(_)) => Err(StoreError::InvalidSite(
                "a Bunny site can't have S3 details",
            )),
            (ProviderKind::S3, None) => {
                Err(StoreError::InvalidSite("an S3 site needs its S3 details"))
            }
        }
    }
}

/// A `sites` row as stored, before text columns are interpreted.
struct RawSite {
    id: String,
    name: String,
    provider: String,
    container: String,
    region: String,
    host: String,
    created_at: String,
    s3_preset: Option<String>,
    s3_endpoint: Option<String>,
    s3_path_style: Option<bool>,
}

impl RawSite {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            provider: row.get(2)?,
            container: row.get(3)?,
            region: row.get(4)?,
            host: row.get(5)?,
            created_at: row.get(6)?,
            s3_preset: row.get(7)?,
            s3_endpoint: row.get(8)?,
            s3_path_style: row.get(9)?,
        })
    }

    fn into_record(self) -> Result<SiteRecord, StoreError> {
        let provider = provider_from_text(&self.provider)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown provider '{}'", self.provider)))?;
        let s3 = match (
            provider,
            self.s3_preset,
            self.s3_endpoint,
            self.s3_path_style,
        ) {
            (ProviderKind::Bunny, None, None, None) => None,
            (ProviderKind::S3, Some(preset), Some(endpoint), Some(path_style)) => Some(S3Site {
                preset: preset_from_text(&preset)
                    .ok_or_else(|| StoreError::Corrupt(format!("unknown S3 preset '{preset}'")))?,
                endpoint,
                path_style,
            }),
            _ => {
                return Err(StoreError::Corrupt(
                    "the provider and the S3 details do not match".into(),
                ));
            }
        };
        Ok(SiteRecord {
            id: SiteId(self.id),
            name: self.name,
            provider,
            container: self.container,
            region: self.region,
            host: self.host,
            s3,
            created_at: parse_stamp(&self.created_at)?,
        })
    }
}

/// An `accounts` row as stored.
struct RawAccount {
    id: String,
    provider: String,
    name: String,
    created_at: String,
}

impl RawAccount {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            provider: row.get(1)?,
            name: row.get(2)?,
            created_at: row.get(3)?,
        })
    }

    /// `None` when the provider or the time is not one this build understands.
    fn into_record(self) -> Option<AccountRecord> {
        Some(AccountRecord {
            id: AccountId(self.id),
            provider: provider_from_text(&self.provider)?,
            name: self.name,
            created_at: parse_stamp(&self.created_at).ok()?,
        })
    }
}

fn provider_to_text(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Bunny => "bunny",
        ProviderKind::S3 => "s3",
    }
}

fn provider_from_text(text: &str) -> Option<ProviderKind> {
    match text {
        "bunny" => Some(ProviderKind::Bunny),
        "s3" => Some(ProviderKind::S3),
        _ => None,
    }
}

/// The names stored for each preset. They are part of the file format: never rename one.
fn preset_to_text(preset: S3Preset) -> &'static str {
    match preset {
        S3Preset::AwsS3 => "aws_s3",
        S3Preset::CloudflareR2 => "cloudflare_r2",
        S3Preset::BackblazeB2 => "backblaze_b2",
        S3Preset::Wasabi => "wasabi",
        S3Preset::DigitalOceanSpaces => "digitalocean_spaces",
        S3Preset::Minio => "minio",
        S3Preset::Other => "other",
    }
}

fn preset_from_text(text: &str) -> Option<S3Preset> {
    match text {
        "aws_s3" => Some(S3Preset::AwsS3),
        "cloudflare_r2" => Some(S3Preset::CloudflareR2),
        "backblaze_b2" => Some(S3Preset::BackblazeB2),
        "wasabi" => Some(S3Preset::Wasabi),
        "digitalocean_spaces" => Some(S3Preset::DigitalOceanSpaces),
        "minio" => Some(S3Preset::Minio),
        "other" => Some(S3Preset::Other),
        _ => None,
    }
}

/// A time as stored: RFC 3339 in UTC, with as many fractional digits as it has.
pub(crate) fn stamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn parse_stamp(text: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(text)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|e| StoreError::Corrupt(format!("bad time '{text}': {e}")))
}

/// True for an error about the value in one row (wrong type, not text, out of range) as
/// opposed to the database itself failing.
pub(crate) fn is_row_decode_error(e: &rusqlite::Error) -> bool {
    use rusqlite::Error as E;
    matches!(
        e,
        E::InvalidColumnType(..)
            | E::FromSqlConversionFailure(..)
            | E::IntegralValueOutOfRange(..)
            | E::Utf8Error(..)
    )
}

fn user_version(conn: &Connection) -> Result<u32, rusqlite::Error> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Opens (creating, with parent folders, if needed) the database file at `path`: WAL mode,
/// busy timeout, schema up to date. The store and the transfer journal each call this for
/// their own connection.
pub(crate) fn open_connection(path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    refuse_newer_schema(&conn)?;
    enable_wal(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// A private in-memory database (no WAL: there is no file), with the schema up to date.
pub(crate) fn open_in_memory_connection() -> Result<Connection, StoreError> {
    let mut conn = Connection::open_in_memory()?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn refuse_newer_schema(conn: &Connection) -> Result<(), StoreError> {
    let supported = MIGRATIONS.len() as u32;
    let found = user_version(conn)?;
    if found > supported {
        return Err(StoreError::NewerSchema { found, supported });
    }
    Ok(())
}

/// Switches the file to write-ahead logging, which lets one connection write while others
/// read, and survives being asked again by a connection that finds it already on.
fn enable_wal(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    // Changing the mode needs a moment when no other connection is using the file. Several
    // openers racing on a new file can see "busy" at once even with a busy timeout, so ask
    // again for a while. Whatever mode results is kept: a file system that can't do WAL
    // (some network shares) simply stays in its own mode.
    let mut attempts = 0;
    loop {
        match conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
        {
            Ok(_) => return Ok(()),
            Err(e) if is_busy(&e) && attempts < 40 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Applies every migration newer than the database's `user_version`. Each step and its
/// version bump commit together, so a failed step leaves the previous version intact.
///
/// Each step starts with `BEGIN IMMEDIATE` and reads the version only once it holds the write
/// lock. Two openers racing (two threads, or two copies of the app) therefore run a step once:
/// the second waits, then sees the first one's finished work instead of repeating it.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    migrate_with(conn, |_| {})
}

/// A moment in [`migrate_with`] where a test can step in, as another opener would.
#[derive(Clone, Copy)]
enum MigrateStage {
    /// About to ask for the write lock.
    BeforeLock,
    /// Holding the write lock, before the version is read.
    Locked,
}

/// [`migrate`], calling `at` at each [`MigrateStage`]. Production passes a closure that does
/// nothing; tests use it to make a race happen on purpose rather than by luck.
fn migrate_with(conn: &mut Connection, mut at: impl FnMut(MigrateStage)) -> Result<(), StoreError> {
    let supported = MIGRATIONS.len() as u32;
    // Already current: no need to take the write lock just to look.
    if user_version(conn)? == supported {
        return Ok(());
    }
    loop {
        at(MigrateStage::BeforeLock);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        at(MigrateStage::Locked);
        let found = user_version(&tx)?;
        if found > supported {
            return Err(StoreError::NewerSchema { found, supported });
        }
        let Some(sql) = MIGRATIONS.get(found as usize) else {
            return Ok(());
        };
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", found + 1)?;
        tx.commit()?;
    }
}

/// Schema v4: the persistent hash cache and the sidebar's favorites.
impl Store {
    /// The digests kept for `path` at this size and modification time (and marks them used).
    #[allow(clippy::type_complexity)]
    pub fn hash_get(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: u64,
    ) -> Result<Option<([u8; 32], [u8; 16])>, StoreError> {
        let (Some(key), Ok(size), Ok(mtime)) =
            (path.to_str(), i64::try_from(size), i64::try_from(mtime_ns))
        else {
            return Ok(None);
        };
        let found: Option<(Vec<u8>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT sha256, md5 FROM hash_cache
                 WHERE path = ?1 AND size = ?2 AND mtime_ns = ?3",
                params![key, size, mtime],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((sha, md5)) = found else {
            return Ok(None);
        };
        let (Ok(sha), Ok(md5)) = (<[u8; 32]>::try_from(sha), <[u8; 16]>::try_from(md5)) else {
            return Ok(None);
        };
        self.conn.execute(
            "UPDATE hash_cache SET used_at = ?2 WHERE path = ?1",
            params![key, used_stamp(Utc::now())],
        )?;
        Ok(Some((sha, md5)))
    }

    /// Keeps the digests of `path` at this size and modification time. A path that is not
    /// Unicode is not kept.
    pub fn hash_put(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: u64,
        sha256: &[u8; 32],
        md5: &[u8; 16],
    ) -> Result<(), StoreError> {
        let Some(key) = path.to_str() else {
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO hash_cache (path, size, mtime_ns, sha256, md5, used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (path) DO UPDATE SET size = excluded.size, mtime_ns = excluded.mtime_ns,
                 sha256 = excluded.sha256, md5 = excluded.md5, used_at = excluded.used_at",
            params![
                key,
                int(size)?,
                int(mtime_ns)?,
                &sha256[..],
                &md5[..],
                used_stamp(Utc::now()),
            ],
        )?;
        Ok(())
    }

    /// Forgets the digests not used since `unused_since`; how many.
    pub fn hash_prune(&self, unused_since: DateTime<Utc>) -> Result<u64, StoreError> {
        let gone = self.conn.execute(
            "DELETE FROM hash_cache WHERE used_at < ?1",
            params![used_stamp(unused_since)],
        )?;
        Ok(gone as u64)
    }

    /// How many files' digests are kept.
    pub fn hash_count(&self) -> Result<u64, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM hash_cache", [], |row| row.get(0))?;
        Ok(count(n).unwrap_or(0))
    }

    /// Forgets every kept digest.
    pub fn hash_clear(&self) -> Result<(), StoreError> {
        self.conn.execute("DELETE FROM hash_cache", [])?;
        Ok(())
    }

    /// The sidebar's favorites, in the order they were added. A row this build cannot read is
    /// left out.
    pub fn favorites(&self) -> Result<Vec<Favorite>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, kind, name, local, site, remote FROM favorites ORDER BY id")?;
        let mut rows = stmt.query([])?;
        let mut favorites = Vec::new();
        while let Some(row) = rows.next()? {
            match read_favorite(row) {
                Ok(raw) => favorites.extend(favorite_of(raw)),
                Err(e) if is_row_decode_error(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(favorites)
    }

    /// Stores `f` (its `id` is ignored); the new id.
    pub fn add_favorite(&self, f: &Favorite) -> Result<i64, StoreError> {
        let local = match &f.local {
            Some(path) => Some(path.to_str().ok_or_else(|| {
                StoreError::Encode("the folder's name is not valid Unicode".into())
            })?),
            None => None,
        };
        self.conn.execute(
            "INSERT INTO favorites (kind, name, local, site, remote) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                place_to_text(f.kind),
                f.name,
                local,
                f.site.as_ref().map(|id| id.0.as_str()),
                f.remote.as_ref().map(RemotePath::as_str),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Removing an id that is not stored is not an error.
    pub fn remove_favorite(&self, id: i64) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM favorites WHERE id = ?1", params![id])?;
        Ok(())
    }
}

/// A `favorites` row as stored: id, kind, name, local, site, remote.
type RawFavorite = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn read_favorite(row: &Row<'_>) -> rusqlite::Result<RawFavorite> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

/// `None` when a column holds something this build does not understand.
fn favorite_of((id, kind, name, local, site, remote): RawFavorite) -> Option<Favorite> {
    let kind = match kind.as_str() {
        "local" => PlaceKind::Local,
        "remote" => PlaceKind::Remote,
        _ => return None,
    };
    let remote = match remote {
        Some(text) => Some(RemotePath::parse(&text).ok()?),
        None => None,
    };
    Some(Favorite {
        id,
        kind,
        name,
        local: local.map(PathBuf::from),
        site: site.map(SiteId),
        remote,
    })
}

fn place_to_text(kind: PlaceKind) -> &'static str {
    match kind {
        PlaceKind::Local => "local",
        PlaceKind::Remote => "remote",
    }
}

/// A hash cache entry's last use, to the second, so that the stored texts sort as the times do.
fn used_stamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use chrono::{DateTime, TimeZone, Utc};
    use httporter_core::{
        AccountId, ConflictPolicy, ProviderKind, ProxyMode, S3Preset, S3Site, SiteId, SiteRecord,
        TransferSettings,
    };

    use super::*;

    fn site(name: &str) -> SiteRecord {
        SiteRecord {
            id: SiteId::new(),
            name: name.to_owned(),
            provider: ProviderKind::Bunny,
            container: "my-zone".to_owned(),
            region: "de".to_owned(),
            host: "storage.bunnycdn.com".to_owned(),
            s3: None,
            created_at: Utc.with_ymd_and_hms(2026, 10, 29, 12, 30, 45).unwrap(),
        }
    }

    fn s3_site(name: &str, preset: S3Preset, path_style: bool) -> SiteRecord {
        SiteRecord {
            id: SiteId::new(),
            name: name.to_owned(),
            provider: ProviderKind::S3,
            container: "media-archive".to_owned(),
            region: "auto".to_owned(),
            host: "acc.r2.cloudflarestorage.com".to_owned(),
            s3: Some(S3Site {
                preset,
                endpoint: "https://acc.r2.cloudflarestorage.com".to_owned(),
                path_style,
            }),
            created_at: Utc.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap(),
        }
    }

    fn names(store: &Store) -> Vec<String> {
        store
            .list_sites()
            .unwrap()
            .sites
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// The database the first release (I1) wrote: `zone`, no S3 columns, no other tables.
    const V1_SCHEMA: &str = "CREATE TABLE sites (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        provider   TEXT NOT NULL,
        zone       TEXT NOT NULL,
        region     TEXT NOT NULL,
        host       TEXT NOT NULL,
        created_at TEXT NOT NULL
    )";

    fn make_v1_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO sites VALUES ('id-1', 'Main site', 'bunny', 'main-zone', 'de', \
             'storage.bunnycdn.com', '2026-10-29T12:30:45Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sites VALUES ('id-2', 'Backups', 'bunny', 'backup-zone', 'ny', \
             'ny.storage.bunnycdn.com', '2026-10-30T08:00:00Z')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }

    #[test]
    fn add_list_sorted_and_remove() {
        let st = Store::open_in_memory().unwrap();
        let zeta = site("zeta");
        let alpha = site("Alpha");
        st.add_site(&zeta).unwrap();
        st.add_site(&alpha).unwrap();
        assert_eq!(names(&st), ["Alpha", "zeta"]);

        st.remove_site(&alpha.id).unwrap();
        assert_eq!(names(&st), ["zeta"]);
    }

    #[test]
    fn reopening_keeps_data_and_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        // The parent folder does not exist yet: `open` must create it.
        let path = dir.path().join("nested").join("httporter.db");
        let record = site("Main site");

        let st = Store::open(&path).unwrap();
        st.add_site(&record).unwrap();
        drop(st);

        let st = Store::open(&path).unwrap();
        assert_eq!(st.list_sites().unwrap().sites, [record]);
        assert_eq!(user_version(&st.conn).unwrap(), MIGRATIONS.len() as u32);
    }

    #[test]
    fn migration_v1_to_v2_keeps_sites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        make_v1_db(&path);

        let st = Store::open(&path).unwrap();
        let list = st.list_sites().unwrap();
        assert_eq!(list.unreadable, 0);
        assert_eq!(list.sites.len(), 2);
        // Sorted by name: Backups, Main site.
        let (backups, main) = (&list.sites[0], &list.sites[1]);
        assert_eq!(main.id, SiteId("id-1".into()));
        assert_eq!(main.name, "Main site");
        assert_eq!(main.provider, ProviderKind::Bunny);
        assert_eq!(main.container, "main-zone", "container is the old zone");
        assert_eq!(main.region, "de");
        assert_eq!(main.host, "storage.bunnycdn.com");
        assert_eq!(main.s3, None);
        assert_eq!(
            main.created_at,
            Utc.with_ymd_and_hms(2026, 10, 29, 12, 30, 45).unwrap()
        );
        assert_eq!(backups.container, "backup-zone");
        assert_eq!(backups.s3, None);
        assert_eq!(user_version(&st.conn).unwrap(), MIGRATIONS.len() as u32);

        // The new tables exist and the migrated database takes new data.
        st.add_site(&s3_site("R2", S3Preset::CloudflareR2, true))
            .unwrap();
        st.put_settings("k", &1_u32).unwrap();
        st.add_account(&AccountRecord {
            id: AccountId::new(),
            provider: ProviderKind::Bunny,
            name: "Bunny".into(),
            created_at: Utc::now(),
        })
        .unwrap();
        assert_eq!(names(&st).len(), 3);
    }

    /// I6: a v4 database (I5) moves to v5 with its rows intact: the profile's options are off,
    /// the publish was the window's, and the journal row has no owner, so it is offered again.
    /// The new fields then round-trip.
    #[test]
    fn migration_v4_to_v5_keeps_rows_and_extras_round_trip() {
        use httporter_core::{Journal, ProfileExtras, Schedule, ScheduleRepeat};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..4] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO profiles VALUES ('p-1', 's-1', 'Site', 'D:\\site', '/', '[]', 0,
                     '\"changed\"', 100, 10, '2026-11-01T10:00:00Z', NULL, NULL);
                 INSERT INTO publish_history (profile, started_at, finished_at, result, uploaded,
                     deleted, skipped, failed, bytes, purge, verified, remote_files,
                     remote_folders, regions)
                 VALUES ('p-1', '2026-11-01T10:00:00Z', '2026-11-01T10:01:00Z', 'ok', 3, 0, 0,
                     0, 300, '\"off\"', 1, 3, 1, 1);
                 INSERT INTO journal (batch, site, kind, local, remote, size, folder, state,
                     error, multipart, updated_at)
                 VALUES ('b', 's-1', 'upload', 'D:\\site\\a.txt', '/a.txt', 1, 0, 'queued',
                     NULL, NULL, '2026-11-01T10:00:00Z');",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 4).unwrap();
        }

        let st = Store::open(&path).unwrap();
        assert_eq!(user_version(&st.conn).unwrap(), MIGRATIONS.len() as u32);
        let mut profile = st.list_profiles().unwrap().remove(0);
        assert_eq!(profile.extras, ProfileExtras::default());
        let history = st.list_history(&profile.id, "").unwrap();
        assert_eq!(history[0].source, HistorySource::App);
        let journal = crate::SqliteJournal::open(&path)
            .unwrap()
            .with_owner(crate::journal::process_owner());
        assert_eq!(journal.unfinished().unwrap().len(), 1);

        profile.extras = ProfileExtras {
            watch: true,
            unattended_deletes: true,
            schedule: Some(Schedule {
                enabled: true,
                repeat: ScheduleRepeat::Weekly {
                    days: [true, false, true, false, true, false, false],
                },
                at_minutes: 6 * 60,
                purge: Some(PurgeMode::Zone),
            }),
        };
        st.update_profile(&profile).unwrap();
        assert_eq!(st.list_profiles().unwrap(), [profile.clone()]);
        let mut scheduled = history[0].clone();
        scheduled.source = HistorySource::Scheduled;
        st.add_history(&scheduled).unwrap();
        assert_eq!(
            st.list_history(&profile.id, "").unwrap()[0].source,
            HistorySource::Scheduled
        );
    }

    #[test]
    fn opens_with_wal_and_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(&dir.path().join("httporter.db")).unwrap();
        let mode: String = st
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let timeout: i64 = st
            .conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    fn bare_connection(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.busy_timeout(BUSY_TIMEOUT).unwrap();
        conn
    }

    /// Ledger 109, first half: whoever migrates first, the late opener must notice and carry
    /// on, not repeat a step ("table sites already exists") from a version it read earlier.
    #[test]
    fn migration_rereads_the_version_after_taking_the_write_lock() {
        for old_file in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("httporter.db");
            if old_file {
                make_v1_db(&path);
            }
            let mut conn = bare_connection(&path);
            let mut raced = false;
            migrate_with(&mut conn, |stage| {
                if matches!(stage, MigrateStage::BeforeLock) && !raced {
                    raced = true;
                    migrate(&mut bare_connection(&path)).unwrap();
                }
            })
            .unwrap();
            assert!(raced);
            assert_eq!(
                user_version(&conn).unwrap(),
                MIGRATIONS.len() as u32,
                "old_file: {old_file}"
            );
        }
    }

    /// Ledger 109, second half: the write lock is already held when the version is read
    /// (`BEGIN IMMEDIATE`), so nobody can change the schema between the read and the step.
    #[test]
    fn migration_holds_the_write_lock_from_before_it_reads_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        let mut conn = bare_connection(&path);
        let mut checked = 0;
        migrate_with(&mut conn, |stage| {
            if matches!(stage, MigrateStage::Locked) {
                let other = bare_connection(&path);
                other.busy_timeout(Duration::from_millis(20)).unwrap();
                let err = other.execute_batch("BEGIN IMMEDIATE").unwrap_err();
                assert!(is_busy(&err), "{err}");
                checked += 1;
            }
        })
        .unwrap();
        assert!(checked > 0);
        assert_eq!(user_version(&conn).unwrap(), MIGRATIONS.len() as u32);
    }

    /// Ledger 109, end to end: several opens at once (a fresh file, then an old one) all
    /// succeed. The race window is small, so the two tests above pin down the mechanism.
    #[test]
    fn migration_runs_in_immediate_transaction() {
        const OPENERS: usize = 4;
        for round in 0..6 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("httporter.db");
            // Alternate between a brand-new file and an old one that needs the v2 step.
            if round % 2 == 1 {
                make_v1_db(&path);
            }
            let start = Barrier::new(OPENERS);
            let opened: Vec<Result<Store, StoreError>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..OPENERS)
                    .map(|_| {
                        scope.spawn(|| {
                            start.wait();
                            Store::open(&path)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for store in opened {
                let store = store.unwrap_or_else(|e| panic!("round {round}: {e}"));
                assert_eq!(
                    user_version(&store.conn).unwrap(),
                    MIGRATIONS.len() as u32,
                    "round {round}"
                );
            }
        }
    }

    /// Ledger 108: a column that could hold a secret must not slip in unnoticed. The exact
    /// column list of every table is pinned, and so is the list of tables.
    #[test]
    fn no_secret_columns_exact() {
        let st = Store::open_in_memory().unwrap();
        assert_eq!(
            st.column_names("sites").unwrap(),
            [
                "id",
                "name",
                "provider",
                "container",
                "region",
                "host",
                "created_at",
                "s3_preset",
                "s3_endpoint",
                "s3_path_style"
            ]
        );
        assert_eq!(
            st.column_names("accounts").unwrap(),
            ["id", "provider", "name", "created_at"]
        );
        assert_eq!(st.column_names("settings").unwrap(), ["key", "value"]);
        assert_eq!(
            st.column_names("profiles").unwrap(),
            [
                "id",
                "site",
                "name",
                "local_dir",
                "remote_dir",
                "skip_rules",
                "remove_extras",
                "purge",
                "guard_files",
                "guard_percent",
                "created_at",
                "last_published_at",
                "cache_control",
                "extras"
            ]
        );
        assert_eq!(
            st.column_names("hash_cache").unwrap(),
            ["path", "size", "mtime_ns", "sha256", "md5", "used_at"]
        );
        assert_eq!(
            st.column_names("favorites").unwrap(),
            ["id", "kind", "name", "local", "site", "remote"]
        );
        assert_eq!(
            st.column_names("publish_history").unwrap(),
            [
                "id",
                "profile",
                "started_at",
                "finished_at",
                "result",
                "uploaded",
                "deleted",
                "skipped",
                "failed",
                "bytes",
                "purge",
                "verified",
                "remote_files",
                "remote_folders",
                "regions",
                "source"
            ]
        );
        assert_eq!(
            st.column_names("publish_files").unwrap(),
            ["history", "path", "action", "outcome", "error", "size"]
        );
        assert_eq!(
            st.column_names("cdn_links").unwrap(),
            ["site", "account", "zones", "checked_at"]
        );
        assert_eq!(
            st.column_names("journal").unwrap(),
            [
                "id",
                "batch",
                "site",
                "kind",
                "local",
                "remote",
                "size",
                "folder",
                "state",
                "error",
                "multipart",
                "updated_at",
                "owner"
            ]
        );
        let mut tables: Vec<String> = st
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        tables.sort();
        assert_eq!(
            tables,
            [
                "accounts",
                "cdn_links",
                "favorites",
                "hash_cache",
                "journal",
                "profiles",
                "publish_files",
                "publish_history",
                "settings",
                "sites",
                "sqlite_sequence"
            ]
        );
    }

    #[test]
    fn default_path_is_under_appdata() {
        assert!(default_db_path().ends_with("HTTPorter\\httporter.db"));
    }

    #[test]
    fn every_field_round_trips_including_subsecond_time() {
        let st = Store::open_in_memory().unwrap();
        let mut record = site("Round trip");
        record.created_at = DateTime::from_timestamp(1_790_000_000, 123_456_789).unwrap();
        st.add_site(&record).unwrap();
        assert_eq!(st.list_sites().unwrap().sites, [record]);
    }

    #[test]
    fn s3_site_round_trips() {
        let st = Store::open_in_memory().unwrap();
        let presets = [
            S3Preset::AwsS3,
            S3Preset::CloudflareR2,
            S3Preset::BackblazeB2,
            S3Preset::Wasabi,
            S3Preset::DigitalOceanSpaces,
            S3Preset::Minio,
            S3Preset::Other,
        ];
        let mut expected = Vec::new();
        for (i, preset) in presets.into_iter().enumerate() {
            // Both addressing styles, and a host and region that differ from the endpoint.
            let mut record = s3_site(&format!("site {i}"), preset, i % 2 == 0);
            record.region = format!("region-{i}");
            st.add_site(&record).unwrap();
            expected.push(record);
        }
        let list = st.list_sites().unwrap();
        assert_eq!(list.unreadable, 0);
        assert_eq!(list.sites, expected);
    }

    /// The stored preset names are part of the file format: pinned like core's JSON names.
    #[test]
    fn stored_s3_preset_and_provider_names_are_pinned() {
        let st = Store::open_in_memory().unwrap();
        let cases = [
            (S3Preset::AwsS3, "aws_s3"),
            (S3Preset::CloudflareR2, "cloudflare_r2"),
            (S3Preset::BackblazeB2, "backblaze_b2"),
            (S3Preset::Wasabi, "wasabi"),
            (S3Preset::DigitalOceanSpaces, "digitalocean_spaces"),
            (S3Preset::Minio, "minio"),
            (S3Preset::Other, "other"),
        ];
        for (preset, text) in cases {
            let record = s3_site(text, preset, true);
            st.add_site(&record).unwrap();
            let (provider, stored, path_style): (String, String, i64) = st
                .conn
                .query_row(
                    "SELECT provider, s3_preset, s3_path_style FROM sites WHERE id = ?1",
                    params![record.id.0],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(
                (provider.as_str(), stored.as_str(), path_style),
                ("s3", text, 1)
            );
        }
        let bunny = site("bunny");
        st.add_site(&bunny).unwrap();
        let (provider, preset): (String, Option<String>) = st
            .conn
            .query_row(
                "SELECT provider, s3_preset FROM sites WHERE id = ?1",
                params![bunny.id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((provider.as_str(), preset), ("bunny", None));
    }

    /// Task 3 minor: a record whose provider and S3 details disagree is refused with a clear
    /// error (not the old "S3 sites can't be saved yet"), on add and on update.
    #[test]
    fn inconsistent_site_records_are_refused_with_a_clear_error() {
        let st = Store::open_in_memory().unwrap();
        let mut bunny_with_s3 = site("bunny");
        bunny_with_s3.s3 = s3_site("x", S3Preset::Wasabi, true).s3;
        let mut s3_without_details = s3_site("bucket", S3Preset::Wasabi, true);
        s3_without_details.s3 = None;

        for bad in [&bunny_with_s3, &s3_without_details] {
            let err = st.add_site(bad).unwrap_err();
            assert!(matches!(err, StoreError::InvalidSite(_)), "{err:?}");
            assert!(!err.to_string().contains("yet"), "{err}");
        }
        assert!(names(&st).is_empty());

        let good = site("good");
        st.add_site(&good).unwrap();
        let mut broken = good.clone();
        broken.provider = ProviderKind::S3;
        let err = st.update_site(&broken).unwrap_err();
        assert!(matches!(err, StoreError::InvalidSite(_)), "{err:?}");
        assert_eq!(st.list_sites().unwrap().sites, [good]);
    }

    #[test]
    fn removing_an_unknown_site_is_not_an_error() {
        let st = Store::open_in_memory().unwrap();
        st.remove_site(&SiteId::new()).unwrap();
    }

    #[test]
    fn adding_the_same_id_twice_fails_and_keeps_the_first() {
        let st = Store::open_in_memory().unwrap();
        let first = site("first");
        let mut clash = site("second");
        clash.id = first.id.clone();
        st.add_site(&first).unwrap();
        assert!(st.add_site(&clash).is_err());
        assert_eq!(names(&st), ["first"]);
    }

    #[test]
    fn update_site_changes_every_field() {
        let st = Store::open_in_memory().unwrap();
        let original = site("Main site");
        let other = site("Other");
        st.add_site(&original).unwrap();
        st.add_site(&other).unwrap();

        // Bunny to S3: everything but the id changes.
        let to_s3 = SiteRecord {
            id: original.id.clone(),
            name: "Renamed".into(),
            provider: ProviderKind::S3,
            container: "new-bucket".into(),
            region: "eu-central-1".into(),
            host: "s3.eu-central-1.amazonaws.com".into(),
            s3: Some(S3Site {
                preset: S3Preset::AwsS3,
                endpoint: "https://s3.eu-central-1.amazonaws.com".into(),
                path_style: false,
            }),
            created_at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
        };
        st.update_site(&to_s3).unwrap();
        let sites = st.list_sites().unwrap().sites;
        assert_eq!(sites, [other.clone(), to_s3.clone()]);

        // And back: the S3 columns are cleared, not left behind.
        let back = SiteRecord {
            provider: ProviderKind::Bunny,
            s3: None,
            name: "Back again".into(),
            ..to_s3
        };
        st.update_site(&back).unwrap();
        assert_eq!(st.list_sites().unwrap().sites, [back.clone(), other]);
        let leftovers: i64 = st
            .conn
            .query_row(
                "SELECT count(*) FROM sites WHERE id = ?1 AND (s3_preset IS NOT NULL \
                 OR s3_endpoint IS NOT NULL OR s3_path_style IS NOT NULL)",
                params![back.id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn update_unknown_site_is_not_found() {
        let st = Store::open_in_memory().unwrap();
        let kept = site("kept");
        st.add_site(&kept).unwrap();
        assert!(matches!(
            st.update_site(&site("ghost")),
            Err(StoreError::NotFound)
        ));
        assert_eq!(st.list_sites().unwrap().sites, [kept]);
    }

    #[test]
    fn unrecognised_provider_text_is_skipped_not_guessed() {
        let st = Store::open_in_memory().unwrap();
        st.add_site(&site("ok")).unwrap();
        st.conn
            .execute("UPDATE sites SET provider = 'nope'", [])
            .unwrap();
        let list = st.list_sites().unwrap();
        assert!(list.sites.is_empty());
        assert_eq!(list.unreadable, 1);
    }

    /// Ledger 111: one bad row must not hide the sites around it, and it is counted so the
    /// app can say so.
    #[test]
    fn undecodable_row_is_skipped_and_counted() {
        let st = Store::open_in_memory().unwrap();
        let good: Vec<_> = ["a", "b", "c"].map(site).into();
        let bad_provider = site("bad provider");
        let bad_date = site("bad date");
        let no_endpoint = s3_site("no endpoint", S3Preset::Minio, true);
        let blob_name = site("blob name");
        let bunny_with_s3_columns = site("stray s3 columns");
        for record in good.iter().chain([
            &bad_provider,
            &bad_date,
            &no_endpoint,
            &blob_name,
            &bunny_with_s3_columns,
        ]) {
            st.add_site(record).unwrap();
        }
        let exec = |sql: &str, id: &SiteId| st.conn.execute(sql, params![id.0]).unwrap();
        exec(
            "UPDATE sites SET provider = 'nope' WHERE id = ?1",
            &bad_provider.id,
        );
        exec(
            "UPDATE sites SET created_at = 'yesterday' WHERE id = ?1",
            &bad_date.id,
        );
        exec(
            "UPDATE sites SET s3_endpoint = NULL WHERE id = ?1",
            &no_endpoint.id,
        );
        exec(
            "UPDATE sites SET name = x'00ff' WHERE id = ?1",
            &blob_name.id,
        );
        exec(
            "UPDATE sites SET s3_preset = 'wasabi' WHERE id = ?1",
            &bunny_with_s3_columns.id,
        );

        let list = st.list_sites().unwrap();
        assert_eq!(list.unreadable, 5);
        assert_eq!(list.sites, good);
    }

    #[test]
    fn settings_default_when_missing_or_corrupt() {
        let st = Store::open_in_memory().unwrap();
        assert_eq!(
            st.settings::<TransferSettings>(SETTINGS_TRANSFERS),
            TransferSettings::default()
        );

        for corrupt in [
            "{not json",
            "[1, 2]",
            "\"text\"",
            "",
            r#"{"simultaneous":"many"}"#,
        ] {
            st.conn
                .execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                    params![SETTINGS_TRANSFERS, corrupt],
                )
                .unwrap();
            assert_eq!(
                st.settings::<TransferSettings>(SETTINGS_TRANSFERS),
                TransferSettings::default(),
                "{corrupt:?}"
            );
        }

        // Written by an older build that lacked most fields: those take their defaults.
        st.conn
            .execute(
                "UPDATE settings SET value = '{\"simultaneous\":4}' WHERE key = ?1",
                params![SETTINGS_TRANSFERS],
            )
            .unwrap();
        let older = st.settings::<TransferSettings>(SETTINGS_TRANSFERS);
        assert_eq!(older.simultaneous, 4);
        assert_eq!(older.retries, TransferSettings::default().retries);
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        let st = Store::open(&path).unwrap();

        // A custom proxy without credentials only: a URL with user:password never reaches
        // SQLite (Ruling Q19).
        let changed = TransferSettings {
            simultaneous: 8,
            adaptive: false,
            retries: 3,
            bandwidth_limit: Some(2_000_000),
            keep_alive: false,
            request_timeout_secs: 300,
            proxy: ProxyMode::Custom("http://proxy.example:8080".into()),
            verify_checksums: false,
            upload_conflicts: ConflictPolicy::Skip,
            download_conflicts: ConflictPolicy::KeepBoth,
            ..TransferSettings::default()
        };
        st.put_settings(SETTINGS_TRANSFERS, &changed).unwrap();
        assert_eq!(st.settings::<TransferSettings>(SETTINGS_TRANSFERS), changed);

        // Writing again replaces; other keys are separate; reopening keeps it.
        let replaced = TransferSettings {
            simultaneous: 2,
            ..changed
        };
        st.put_settings(SETTINGS_TRANSFERS, &replaced).unwrap();
        let id = SiteId::new();
        st.put_settings(&site_conflicts_key(&id, true), &ConflictPolicy::Replace)
            .unwrap();
        drop(st);

        let st = Store::open(&path).unwrap();
        assert_eq!(
            st.settings::<TransferSettings>(SETTINGS_TRANSFERS),
            replaced
        );
        assert_eq!(
            st.settings::<Option<ConflictPolicy>>(&site_conflicts_key(&id, true)),
            Some(ConflictPolicy::Replace)
        );
        assert_eq!(
            st.settings::<Option<ConflictPolicy>>(&site_conflicts_key(&id, false)),
            None
        );
        let rows: i64 = st
            .conn
            .query_row("SELECT count(*) FROM settings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[test]
    fn settings_keys_are_pinned() {
        assert_eq!(SETTINGS_TRANSFERS, "transfers");
        let id = SiteId("abc".into());
        assert_eq!(site_conflicts_key(&id, true), "conflicts/upload/abc");
        assert_eq!(site_conflicts_key(&id, false), "conflicts/download/abc");
    }

    fn account(name: &str, provider: ProviderKind) -> AccountRecord {
        AccountRecord {
            id: AccountId::new(),
            provider,
            name: name.to_owned(),
            created_at: DateTime::from_timestamp(1_790_000_000, 987_654_321).unwrap(),
        }
    }

    #[test]
    fn accounts_add_list_sorted_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        let st = Store::open(&path).unwrap();
        assert!(st.list_accounts().unwrap().is_empty());

        let zeta = account("zeta", ProviderKind::Bunny);
        let alpha = account("Alpha", ProviderKind::S3);
        st.add_account(&zeta).unwrap();
        st.add_account(&alpha).unwrap();
        assert!(st.add_account(&zeta).is_err(), "same id twice");
        drop(st);

        let st = Store::open(&path).unwrap();
        assert_eq!(st.list_accounts().unwrap(), [alpha.clone(), zeta.clone()]);
        st.remove_account(&alpha.id).unwrap();
        assert_eq!(st.list_accounts().unwrap(), [zeta]);
        // Removing one that is gone is not an error.
        st.remove_account(&alpha.id).unwrap();
        st.remove_account(&AccountId::new()).unwrap();
    }

    #[test]
    fn an_undecodable_account_row_is_skipped() {
        let st = Store::open_in_memory().unwrap();
        let good = account("good", ProviderKind::Bunny);
        let bad = account("bad", ProviderKind::Bunny);
        st.add_account(&good).unwrap();
        st.add_account(&bad).unwrap();
        st.conn
            .execute(
                "UPDATE accounts SET provider = 'nope' WHERE id = ?1",
                params![bad.id.0],
            )
            .unwrap();
        assert_eq!(st.list_accounts().unwrap(), [good]);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);

        assert!(matches!(
            Store::open(&path),
            Err(StoreError::NewerSchema { found: 99, .. })
        ));
    }

    // --- T10: schema v3 and v4, profiles, history, CDN links, hash cache, favorites ---------

    use httporter_core::ProfileExtras;

    fn profile_of(site: &SiteId, name: &str) -> ProfileRecord {
        ProfileRecord {
            id: ProfileId::new(),
            site: site.clone(),
            name: name.to_owned(),
            local_dir: std::path::PathBuf::from(r"D:\site\public"),
            remote_dir: RemotePath::parse("www").unwrap(),
            skip_rules: vec![".env".into(), "node_modules/".into()],
            remove_extras: false,
            purge: PurgeMode::Changed,
            guard: DeleteGuard::default(),
            created_at: Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap(),
            last_published_at: None,
            cache_control: None,
            extras: ProfileExtras::default(),
        }
    }

    fn publish_of(profile: &ProfileId, minute: u32, files: Vec<HistoryFile>) -> HistoryRecord {
        HistoryRecord {
            id: 0,
            profile: profile.clone(),
            started_at: Utc.with_ymd_and_hms(2026, 11, 5, 10, 0, 0).unwrap()
                + chrono::Duration::minutes(i64::from(minute)),
            finished_at: Utc.with_ymd_and_hms(2026, 11, 5, 10, 0, 30).unwrap()
                + chrono::Duration::minutes(i64::from(minute)),
            result: PublishResult::Ok,
            uploaded: 1,
            deleted: 0,
            skipped: 0,
            failed: 0,
            bytes: 10,
            purge: PurgeResult::Urls(1),
            verified: true,
            remote_files: 3,
            remote_folders: 1,
            regions: 2,
            files,
            source: HistorySource::App,
        }
    }

    fn hfile(path: &str, action: FileAction, outcome: FileOutcome) -> HistoryFile {
        HistoryFile {
            path: RemotePath::parse(path).unwrap(),
            action,
            outcome,
            size: 42,
        }
    }

    fn link(account: &AccountId) -> CdnLink {
        CdnLink {
            account: account.clone(),
            zones: vec![CdnZone {
                id: 7,
                name: "pull".into(),
                hostnames: vec!["cdn.example".into(), "www.example.com".into()],
            }],
            checked_at: Utc.with_ymd_and_hms(2026, 11, 6, 8, 0, 0).unwrap(),
        }
    }

    #[test]
    fn migration_v2_to_v3_on_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..2] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO sites VALUES ('s-1', 'Main', 'bunny', 'main-zone', 'de',
                     'storage.bunnycdn.com', '2026-10-29T12:30:45Z', NULL, NULL, NULL);
                 INSERT INTO settings VALUES ('k', '1');",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let st = Store::open(&path).unwrap();
        assert_eq!(user_version(&st.conn).unwrap(), MIGRATIONS.len() as u32);
        assert_eq!(names(&st), ["Main"]);
        assert_eq!(st.settings::<u32>("k"), 1);
        let site = SiteId("s-1".into());
        let profile = profile_of(&site, "Blog");
        st.add_profile(&profile).unwrap();
        st.add_history(&publish_of(&profile.id, 0, Vec::new()))
            .unwrap();
        st.put_cdn_link(&site, &link(&AccountId::new())).unwrap();
        assert_eq!(st.list_profiles().unwrap().len(), 1);
    }

    #[test]
    fn migration_v3_to_v4_on_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("httporter.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..3] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(
                r#"INSERT INTO profiles VALUES ('p-1', 's-1', 'Blog', 'D:\site', 'www',
                     '[".env"]', 1, '"zone"', 50, 20, '2026-11-01T10:00:00Z',
                     '2026-11-02T10:00:00Z');"#,
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let st = Store::open(&path).unwrap();
        assert_eq!(user_version(&st.conn).unwrap(), MIGRATIONS.len() as u32);
        let profile = st.list_profiles().unwrap().remove(0);
        assert_eq!(profile.name, "Blog");
        assert_eq!(profile.skip_rules, [".env"]);
        assert!(profile.remove_extras);
        assert_eq!(profile.purge, PurgeMode::Zone);
        assert_eq!(
            profile.guard,
            DeleteGuard {
                files: 50,
                percent: 20
            }
        );
        assert_eq!(profile.cache_control, None, "the new column starts empty");
        assert!(profile.last_published_at.is_some());
        assert!(st.favorites().unwrap().is_empty());
        assert_eq!(st.hash_count().unwrap(), 0);
    }

    #[test]
    fn profiles_add_list_update_and_remove() {
        let st = Store::open_in_memory().unwrap();
        let site = SiteId::new();
        let zeta = profile_of(&site, "zeta");
        let mut alpha = profile_of(&site, "Alpha");
        alpha.cache_control = Some("max-age=60".into());
        st.add_profile(&zeta).unwrap();
        st.add_profile(&alpha).unwrap();
        assert!(st.add_profile(&alpha).is_err(), "the same id twice");
        assert_eq!(st.list_profiles().unwrap(), [alpha.clone(), zeta.clone()]);

        // A publish moves last_published_at; an edit saved later must not put it back.
        st.add_history(&publish_of(&alpha.id, 0, Vec::new()))
            .unwrap();
        let mut edited = alpha.clone();
        edited.name = "Alpha 2".into();
        edited.remove_extras = true;
        edited.skip_rules = Vec::new();
        edited.purge = PurgeMode::Off;
        edited.cache_control = None;
        st.update_profile(&edited).unwrap();
        let stored = st
            .list_profiles()
            .unwrap()
            .into_iter()
            .find(|p| p.id == alpha.id)
            .unwrap();
        assert_eq!(
            stored.last_published_at,
            Some(Utc.with_ymd_and_hms(2026, 11, 5, 10, 0, 30).unwrap())
        );
        assert_eq!(
            ProfileRecord {
                last_published_at: None,
                ..stored
            },
            edited
        );

        assert!(matches!(
            st.update_profile(&profile_of(&site, "ghost")),
            Err(StoreError::ProfileNotFound)
        ));
        st.remove_profile(&alpha.id).unwrap();
        st.remove_profile(&alpha.id).unwrap();
        assert!(st.list_history(&alpha.id, "").unwrap().is_empty());
        assert_eq!(st.list_profiles().unwrap(), [zeta]);
    }

    #[test]
    fn add_history_sets_last_published_and_trims_at_201() {
        let st = Store::open_in_memory().unwrap();
        let site = SiteId::new();
        let profile = profile_of(&site, "Blog");
        let other = profile_of(&site, "Other");
        st.add_profile(&profile).unwrap();
        st.add_profile(&other).unwrap();
        st.add_history(&publish_of(&other.id, 0, Vec::new()))
            .unwrap();
        let mut ids = Vec::new();
        for minute in 0..=HISTORY_KEEP as u32 {
            let files = vec![hfile("www/a.txt", FileAction::Upload, FileOutcome::Done)];
            ids.push(
                st.add_history(&publish_of(&profile.id, minute, files))
                    .unwrap(),
            );
        }
        let history = st.list_history(&profile.id, "").unwrap();
        assert_eq!(history.len(), HISTORY_KEEP);
        assert_eq!(history[0].id, *ids.last().unwrap(), "newest first");
        assert!(history.iter().all(|h| h.id != ids[0]), "the oldest went");
        assert!(history.iter().all(|h| h.files.is_empty()));
        assert!(st.history_files(ids[0]).unwrap().is_empty());
        assert_eq!(st.history_files(ids[1]).unwrap().len(), 1);
        assert_eq!(
            st.list_history(&other.id, "").unwrap().len(),
            1,
            "per profile"
        );
        let stored = st
            .list_profiles()
            .unwrap()
            .into_iter()
            .find(|p| p.id == profile.id)
            .unwrap();
        assert_eq!(stored.last_published_at, Some(history[0].finished_at));
    }

    #[test]
    fn history_files_round_trip_search_and_purge_update() {
        let st = Store::open_in_memory().unwrap();
        let profile = profile_of(&SiteId::new(), "Blog");
        st.add_profile(&profile).unwrap();
        let files = vec![
            hfile("www/blog/index.html", FileAction::Upload, FileOutcome::Done),
            hfile(
                "www/blog/big.png",
                FileAction::Upload,
                FileOutcome::Failed("Access Denied".into()),
            ),
            hfile(
                "www/old 100%.txt",
                FileAction::Delete,
                FileOutcome::Cancelled,
            ),
            hfile(
                "www/kept.txt",
                FileAction::Delete,
                FileOutcome::NotRun("Not deleted because uploads failed.".into()),
            ),
        ];
        let mut partial = publish_of(&profile.id, 0, files.clone());
        partial.result = PublishResult::Partial;
        partial.purge = PurgeResult::Failed("1 of 3 failed: boom".into());
        partial.source = HistorySource::Watch;
        let first = st.add_history(&partial).unwrap();
        let second = st
            .add_history(&publish_of(
                &profile.id,
                5,
                vec![hfile(
                    "www/css/site.css",
                    FileAction::Upload,
                    FileOutcome::Done,
                )],
            ))
            .unwrap();

        assert_eq!(st.history_files(first).unwrap(), files);
        let stored = &st.list_history(&profile.id, "").unwrap()[1];
        assert_eq!(
            HistoryRecord {
                files: files.clone(),
                ..stored.clone()
            },
            HistoryRecord {
                id: first,
                ..partial.clone()
            }
        );

        let ids = |search: &str| -> Vec<i64> {
            st.list_history(&profile.id, search)
                .unwrap()
                .iter()
                .map(|h| h.id)
                .collect()
        };
        assert_eq!(ids("BLOG/"), [first], "by path, any case");
        assert_eq!(ids("site.css"), [second]);
        assert_eq!(ids("partial"), [first], "by result");
        assert_eq!(ids("100%"), [first], "% is matched as itself");
        assert_eq!(ids("%"), [first]);
        assert_eq!(ids("  "), [second, first]);
        assert!(ids("nowhere").is_empty());

        st.update_history_purge(first, &PurgeResult::Zone).unwrap();
        st.update_history_purge(9999, &PurgeResult::Zone).unwrap();
        let after = st.list_history(&profile.id, "partial").unwrap();
        assert_eq!(after[0].purge, PurgeResult::Zone);
    }

    #[test]
    fn cdn_links_round_trip_and_go_with_their_account() {
        let st = Store::open_in_memory().unwrap();
        let (a, b) = (SiteId::new(), SiteId::new());
        let (one, two) = (AccountId::new(), AccountId::new());
        assert_eq!(st.cdn_link(&a).unwrap(), None);
        st.put_cdn_link(&a, &link(&one)).unwrap();
        st.put_cdn_link(&b, &link(&two)).unwrap();
        assert_eq!(st.cdn_link(&a).unwrap(), Some(link(&one)));
        let mut replaced = link(&two);
        replaced.zones.clear();
        st.put_cdn_link(&a, &replaced).unwrap();
        assert_eq!(st.cdn_link(&a).unwrap(), Some(replaced));

        st.remove_cdn_links_for(&two).unwrap();
        assert_eq!(st.cdn_link(&a).unwrap(), None);
        assert_eq!(st.cdn_link(&b).unwrap(), None);
        st.put_cdn_link(&b, &link(&one)).unwrap();
        st.remove_cdn_links_for(&two).unwrap();
        assert_eq!(st.cdn_link(&b).unwrap(), Some(link(&one)));

        st.conn
            .execute("UPDATE cdn_links SET zones = 'not json'", [])
            .unwrap();
        assert_eq!(st.cdn_link(&b).unwrap(), None, "unreadable reads as none");
    }

    #[test]
    fn remove_site_takes_everything_of_the_site_along() {
        let st = Store::open_in_memory().unwrap();
        let gone = site("Gone");
        let kept = site("Kept");
        st.add_site(&gone).unwrap();
        st.add_site(&kept).unwrap();
        let mut ids = Vec::new();
        for s in [&gone, &kept] {
            let profile = profile_of(&s.id, "P");
            st.add_profile(&profile).unwrap();
            let files = vec![hfile("www/a.txt", FileAction::Upload, FileOutcome::Done)];
            ids.push((
                profile.id.clone(),
                st.add_history(&publish_of(&profile.id, 0, files)).unwrap(),
            ));
            st.put_cdn_link(&s.id, &link(&AccountId::new())).unwrap();
            st.add_favorite(&Favorite {
                id: 0,
                kind: PlaceKind::Remote,
                name: "www".into(),
                local: None,
                site: Some(s.id.clone()),
                remote: Some(RemotePath::parse("www").unwrap()),
            })
            .unwrap();
            st.put_settings(&site_extras_key(&s.id), &1_u32).unwrap();
            st.put_settings(&site_conflicts_key(&s.id, true), &2_u32)
                .unwrap();
            st.put_settings(&site_conflicts_key(&s.id, false), &3_u32)
                .unwrap();
        }

        st.remove_site(&gone.id).unwrap();
        assert_eq!(names(&st), ["Kept"]);
        let (gone_profile, gone_history) = &ids[0];
        let (kept_profile, kept_history) = &ids[1];
        let profiles = st.list_profiles().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, *kept_profile);
        assert!(st.list_history(gone_profile, "").unwrap().is_empty());
        assert!(st.history_files(*gone_history).unwrap().is_empty());
        assert_eq!(st.history_files(*kept_history).unwrap().len(), 1);
        assert_eq!(st.cdn_link(&gone.id).unwrap(), None);
        assert!(st.cdn_link(&kept.id).unwrap().is_some());
        let favorites = st.favorites().unwrap();
        assert_eq!(favorites.len(), 1);
        assert_eq!(favorites[0].site, Some(kept.id.clone()));
        for key in [
            site_extras_key(&gone.id),
            site_conflicts_key(&gone.id, true),
            site_conflicts_key(&gone.id, false),
        ] {
            assert_eq!(st.settings::<u32>(&key), 0, "{key}");
        }
        assert_eq!(st.settings::<u32>(&site_extras_key(&kept.id)), 1);
        assert_eq!(st.settings::<u32>(&site_conflicts_key(&kept.id, false)), 3);
    }

    #[test]
    fn the_hash_cache_keeps_digests_by_size_and_time() {
        let st = Store::open_in_memory().unwrap();
        let path = std::path::Path::new(r"D:\site\a.txt");
        let (sha, md5) = ([1u8; 32], [2u8; 16]);
        assert_eq!(st.hash_get(path, 10, 5).unwrap(), None);
        st.hash_put(path, 10, 5, &sha, &md5).unwrap();
        assert_eq!(st.hash_get(path, 10, 5).unwrap(), Some((sha, md5)));
        assert_eq!(st.hash_get(path, 11, 5).unwrap(), None, "another size");
        assert_eq!(st.hash_get(path, 10, 6).unwrap(), None, "another time");
        // A new digest of the same path replaces the old one.
        st.hash_put(path, 11, 6, &[3u8; 32], &[4u8; 16]).unwrap();
        assert_eq!(st.hash_get(path, 10, 5).unwrap(), None);
        assert_eq!(
            st.hash_get(path, 11, 6).unwrap(),
            Some(([3u8; 32], [4u8; 16]))
        );
        st.hash_put(std::path::Path::new(r"D:\b.txt"), 1, 1, &sha, &md5)
            .unwrap();
        assert_eq!(st.hash_count().unwrap(), 2);
        assert_eq!(
            st.hash_get(path, u64::MAX, 6).unwrap(),
            None,
            "a size SQLite can't hold"
        );

        let day = chrono::Duration::days(1);
        assert_eq!(st.hash_prune(Utc::now() - day).unwrap(), 0);
        assert_eq!(st.hash_prune(Utc::now() + day).unwrap(), 2);
        st.hash_put(path, 10, 5, &sha, &md5).unwrap();
        st.hash_clear().unwrap();
        assert_eq!(st.hash_count().unwrap(), 0);
    }

    #[test]
    fn favorites_add_list_in_order_and_remove() {
        let st = Store::open_in_memory().unwrap();
        let local = Favorite {
            id: 0,
            kind: PlaceKind::Local,
            name: "Site".into(),
            local: Some(std::path::PathBuf::from(r"D:\site")),
            site: None,
            remote: None,
        };
        let remote = Favorite {
            id: 0,
            kind: PlaceKind::Remote,
            name: "Blog".into(),
            local: None,
            site: Some(SiteId("s-1".into())),
            remote: Some(RemotePath::parse("www/blog").unwrap()),
        };
        let a = st.add_favorite(&remote).unwrap();
        let b = st.add_favorite(&local).unwrap();
        assert_eq!(
            st.favorites().unwrap(),
            [
                Favorite {
                    id: a,
                    ..remote.clone()
                },
                Favorite { id: b, ..local }
            ]
        );
        st.remove_favorite(b).unwrap();
        st.remove_favorite(b).unwrap();
        assert_eq!(st.favorites().unwrap(), [Favorite { id: a, ..remote }]);
    }
}
