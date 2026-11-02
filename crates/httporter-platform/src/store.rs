//! Local SQLite database: saved sites, provider accounts, settings and the transfer journal.
//!
//! Holds connection details only (name, provider, container, region, host, S3 endpoint).
//! Passwords and API keys never enter this database: they live in the credential store (see
//! `creds`), keyed by site or account id.
//!
//! The `sites.container` column holds [`SiteRecord::container`]. It was called `zone` in
//! schema v1.
//!
//! The file is opened in WAL mode with a 5 s busy timeout, so the store and the transfer
//! journal ([`crate::journal`]) can each hold their own connection to it.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use httporter_core::{AccountId, ProviderKind, S3Preset, S3Site, SiteId, SiteRecord};
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
];

/// The Transfers settings (Settings window, Transfers tab) are stored under this key.
pub const SETTINGS_TRANSFERS: &str = "transfers";

/// The settings key of a site's own conflict policy: `conflicts/upload/<id>` for uploads,
/// `conflicts/download/<id>` for downloads.
pub fn site_conflicts_key(id: &SiteId, upload: bool) -> String {
    let direction = if upload { "upload" } else { "download" };
    format!("conflicts/{direction}/{id}")
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
    /// A site record whose provider and S3 details disagree: refused rather than saved wrong.
    #[error("the site's details are inconsistent: {0}")]
    InvalidSite(&'static str),
    /// A value could not be turned into text for storage.
    #[error("could not encode the value for storage: {0}")]
    Encode(String),
}

/// `%APPDATA%\HTTPorter\httporter.db` (the per-user roaming config folder).
///
/// If the platform reports no config folder the path is relative to the working directory.
pub fn default_db_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("HTTPorter")
        .join("httporter.db")
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

    /// Deletes a site. Removing an id that is not stored is not an error.
    pub fn remove_site(&self, id: &SiteId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM sites WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    /// The setting stored under `key`, or `T::default()` when there is none or it cannot be
    /// read (missing, damaged, or written in a shape this build no longer understands). A
    /// setting must never stop the app starting.
    pub fn settings<T: DeserializeOwned + Default>(&self, key: &str) -> T {
        self.conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
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
        assert_eq!(user_version(&st.conn).unwrap(), 2);
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
        assert_eq!(user_version(&st.conn).unwrap(), 2);

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
            assert_eq!(user_version(&conn).unwrap(), 2, "old_file: {old_file}");
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
        assert_eq!(user_version(&conn).unwrap(), 2);
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
                assert_eq!(user_version(&store.conn).unwrap(), 2, "round {round}");
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
                "updated_at"
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
                "journal",
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
}
