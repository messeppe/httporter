//! Local SQLite database of saved sites.
//!
//! Holds connection details only (name, provider, zone, region, host). Passwords and API
//! keys never enter this database: they live in the credential store (see `creds`), keyed
//! by site id.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use httporter_core::{ProviderKind, SiteId, SiteRecord};
use rusqlite::{Connection, params};

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
];

/// Site store failure.
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
    #[error("stored site data is invalid: {0}")]
    Corrupt(String),
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

/// The sites database.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating, with parent folders, if needed) the database at `path` and brings its
    /// schema up to date.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// A throwaway database that lives only as long as the returned `Store`.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self, StoreError> {
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Inserts a site. Fails if a site with the same id already exists.
    pub fn add_site(&self, site: &SiteRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO sites (id, name, provider, zone, region, host, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                site.id.0,
                site.name,
                provider_to_text(site.provider),
                site.zone,
                site.region,
                site.host,
                site.created_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            ],
        )?;
        Ok(())
    }

    /// All sites, ordered by name ignoring ASCII case.
    pub fn list_sites(&self) -> Result<Vec<SiteRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, provider, zone, region, host, created_at
             FROM sites ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(RawSite {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    provider: row.get(2)?,
                    zone: row.get(3)?,
                    region: row.get(4)?,
                    host: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(RawSite::into_record).collect()
    }

    /// Deletes a site. Removing an id that is not stored is not an error.
    pub fn remove_site(&self, id: &SiteId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM sites WHERE id = ?1", params![id.0])?;
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

/// A `sites` row as stored, before text columns are interpreted.
struct RawSite {
    id: String,
    name: String,
    provider: String,
    zone: String,
    region: String,
    host: String,
    created_at: String,
}

impl RawSite {
    fn into_record(self) -> Result<SiteRecord, StoreError> {
        let provider = provider_from_text(&self.provider)
            .ok_or_else(|| StoreError::Corrupt(format!("unknown provider '{}'", self.provider)))?;
        let created_at = DateTime::parse_from_rfc3339(&self.created_at)
            .map_err(|e| StoreError::Corrupt(format!("bad created_at '{}': {e}", self.created_at)))?
            .with_timezone(&Utc);
        Ok(SiteRecord {
            id: SiteId(self.id),
            name: self.name,
            provider,
            zone: self.zone,
            region: self.region,
            host: self.host,
            created_at,
        })
    }
}

fn provider_to_text(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Bunny => "bunny",
    }
}

fn provider_from_text(text: &str) -> Option<ProviderKind> {
    match text {
        "bunny" => Some(ProviderKind::Bunny),
        _ => None,
    }
}

fn user_version(conn: &Connection) -> Result<u32, rusqlite::Error> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Applies every migration newer than the database's `user_version`. Each step and its
/// version bump commit together, so a failed step leaves the previous version intact.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let supported = MIGRATIONS.len() as u32;
    let found = user_version(conn)?;
    if found > supported {
        return Err(StoreError::NewerSchema { found, supported });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as u32 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use httporter_core::{ProviderKind, SiteId, SiteRecord};

    use super::*;

    fn site(name: &str) -> SiteRecord {
        SiteRecord {
            id: SiteId::new(),
            name: name.to_owned(),
            provider: ProviderKind::Bunny,
            zone: "my-zone".to_owned(),
            region: "de".to_owned(),
            host: "storage.bunnycdn.com".to_owned(),
            created_at: Utc.with_ymd_and_hms(2026, 10, 29, 12, 30, 45).unwrap(),
        }
    }

    fn names(store: &Store) -> Vec<String> {
        store
            .list_sites()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
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
        assert_eq!(st.list_sites().unwrap(), [record]);
        assert_eq!(user_version(&st.conn).unwrap(), 1);
    }

    #[test]
    fn no_secret_columns() {
        let st = Store::open_in_memory().unwrap();
        let cols = st.column_names("sites").unwrap();
        assert!(
            cols.iter()
                .all(|c| !c.contains("pass") && !c.contains("key") && !c.contains("secret"))
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
        assert_eq!(st.list_sites().unwrap(), [record]);
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
    fn unrecognised_provider_text_is_reported_not_guessed() {
        let st = Store::open_in_memory().unwrap();
        st.add_site(&site("ok")).unwrap();
        st.conn
            .execute("UPDATE sites SET provider = 'nope'", [])
            .unwrap();
        assert!(matches!(st.list_sites(), Err(StoreError::Corrupt(_))));
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
