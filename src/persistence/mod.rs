use crate::adapters::docker::DockerSnapshot;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const CAPSULE_SCHEMA_VERSION: u32 = 1;
const DATABASE_FILE_NAME: &str = "capsules.db";
const MAX_CAPSULE_NAME_CHARS: usize = 128;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCapsuleSnapshot {
    pub schema_version: u32,
    pub captured_at_unix_ms: i64,
    pub snapshot: Value,
}

impl StoredCapsuleSnapshot {
    pub fn new(snapshot: Value) -> Self {
        Self {
            schema_version: CAPSULE_SCHEMA_VERSION,
            captured_at_unix_ms: now_unix_ms(),
            snapshot,
        }
    }

    pub fn docker(&self) -> Result<DockerSnapshot, PersistenceError> {
        let value = self.snapshot.get("docker").cloned().ok_or_else(|| {
            PersistenceError::InvalidPayload("snapshot has no Docker section".to_owned())
        })?;
        serde_json::from_value(value).map_err(PersistenceError::Json)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleSummary {
    pub name: String,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub schema_version: u32,
}

#[derive(Debug)]
pub enum PersistenceError {
    Io(std::io::Error),
    Database(rusqlite::Error),
    Json(serde_json::Error),
    InvalidName(String),
    AlreadyExists(String),
    NotFound(String),
    InvalidPayload(String),
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "filesystem error: {error}"),
            Self::Database(error) => write!(formatter, "SQLite error: {error}"),
            Self::Json(error) => write!(formatter, "snapshot JSON error: {error}"),
            Self::InvalidName(message) => write!(formatter, "invalid capsule name: {message}"),
            Self::AlreadyExists(name) => write!(
                formatter,
                "capsule '{name}' already exists; use --force to replace its snapshot"
            ),
            Self::NotFound(name) => write!(formatter, "capsule '{name}' was not found"),
            Self::InvalidPayload(message) => {
                write!(formatter, "invalid capsule payload: {message}")
            }
        }
    }
}

impl Error for PersistenceError {}

impl From<std::io::Error> for PersistenceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for PersistenceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub struct CapsuleStore {
    connection: Connection,
    path: PathBuf,
}

impl CapsuleStore {
    pub fn open_default() -> Result<Self, PersistenceError> {
        Self::open_at(default_database_path()?)
    }

    pub fn open_at(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;\n\
             PRAGMA journal_mode = WAL;\n\
             CREATE TABLE IF NOT EXISTS capsules (\n\
                 id INTEGER PRIMARY KEY AUTOINCREMENT,\n\
                 name TEXT NOT NULL COLLATE NOCASE UNIQUE,\n\
                 created_at_unix_ms INTEGER NOT NULL,\n\
                 updated_at_unix_ms INTEGER NOT NULL,\n\
                 schema_version INTEGER NOT NULL,\n\
                 payload_json TEXT NOT NULL\n\
             );\n\
             CREATE INDEX IF NOT EXISTS idx_capsules_updated_at\n\
                 ON capsules(updated_at_unix_ms DESC);\n\
             PRAGMA user_version = 1;",
        )?;

        Ok(Self { connection, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(
        &mut self,
        name: &str,
        snapshot: &StoredCapsuleSnapshot,
        replace: bool,
    ) -> Result<CapsuleSummary, PersistenceError> {
        validate_capsule_name(name)?;
        let payload_json = serde_json::to_string(snapshot)?;
        let now = now_unix_ms();
        let transaction = self.connection.transaction()?;

        let existing = transaction
            .query_row(
                "SELECT created_at_unix_ms FROM capsules WHERE name = ?1 COLLATE NOCASE",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;

        let created_at = match existing {
            Some(created_at) if !replace => {
                return Err(PersistenceError::AlreadyExists(name.to_owned()));
            }
            Some(created_at) => {
                transaction.execute(
                    "UPDATE capsules\n\
                     SET name = ?1, updated_at_unix_ms = ?2, schema_version = ?3, payload_json = ?4\n\
                     WHERE name = ?1 COLLATE NOCASE",
                    params![name, now, snapshot.schema_version, payload_json],
                )?;
                created_at
            }
            None => {
                transaction.execute(
                    "INSERT INTO capsules\n\
                     (name, created_at_unix_ms, updated_at_unix_ms, schema_version, payload_json)\n\
                     VALUES (?1, ?2, ?2, ?3, ?4)",
                    params![name, now, snapshot.schema_version, payload_json],
                )?;
                now
            }
        };

        transaction.commit()?;
        Ok(CapsuleSummary {
            name: name.to_owned(),
            created_at_unix_ms: created_at,
            updated_at_unix_ms: now,
            schema_version: snapshot.schema_version,
        })
    }

    pub fn load(&self, name: &str) -> Result<StoredCapsuleSnapshot, PersistenceError> {
        validate_capsule_name(name)?;
        let payload = self
            .connection
            .query_row(
                "SELECT payload_json FROM capsules WHERE name = ?1 COLLATE NOCASE",
                [name],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| PersistenceError::NotFound(name.to_owned()))?;

        serde_json::from_str(&payload).map_err(PersistenceError::Json)
    }

    pub fn list(&self) -> Result<Vec<CapsuleSummary>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT name, created_at_unix_ms, updated_at_unix_ms, schema_version\n\
             FROM capsules\n\
             ORDER BY updated_at_unix_ms DESC, name COLLATE NOCASE ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(CapsuleSummary {
                name: row.get(0)?,
                created_at_unix_ms: row.get(1)?,
                updated_at_unix_ms: row.get(2)?,
                schema_version: row.get(3)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PersistenceError::Database)
    }

    pub fn delete(&mut self, name: &str) -> Result<(), PersistenceError> {
        validate_capsule_name(name)?;
        let transaction = self.connection.transaction()?;
        let deleted = transaction.execute(
            "DELETE FROM capsules WHERE name = ?1 COLLATE NOCASE",
            [name],
        )?;
        if deleted == 0 {
            return Err(PersistenceError::NotFound(name.to_owned()));
        }
        transaction.commit()?;
        Ok(())
    }
}

impl From<serde_json::Error> for PersistenceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn default_database_path() -> Result<PathBuf, PersistenceError> {
    if let Some(path) = env::var_os("CONTEXT_CAPSULE_DB") {
        if path.is_empty() {
            return Err(PersistenceError::InvalidPayload(
                "CONTEXT_CAPSULE_DB is set but empty".to_owned(),
            ));
        }
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        let base = env::var_os("LOCALAPPDATA").ok_or_else(|| {
            PersistenceError::InvalidPayload("LOCALAPPDATA is not available".to_owned())
        })?;
        return Ok(PathBuf::from(base)
            .join("ContextCapsule")
            .join(DATABASE_FILE_NAME));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME")
            .ok_or_else(|| PersistenceError::InvalidPayload("HOME is not available".to_owned()))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("ContextCapsule")
            .join(DATABASE_FILE_NAME));
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        if let Some(base) = env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(base)
                .join("context-capsule")
                .join(DATABASE_FILE_NAME));
        }

        let home = env::var_os("HOME")
            .ok_or_else(|| PersistenceError::InvalidPayload("HOME is not available".to_owned()))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("context-capsule")
            .join(DATABASE_FILE_NAME))
    }
}

fn validate_capsule_name(name: &str) -> Result<(), PersistenceError> {
    if name.trim().is_empty() {
        return Err(PersistenceError::InvalidName(
            "name cannot be empty".to_owned(),
        ));
    }
    if name.chars().count() > MAX_CAPSULE_NAME_CHARS {
        return Err(PersistenceError::InvalidName(format!(
            "name cannot exceed {MAX_CAPSULE_NAME_CHARS} characters"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(PersistenceError::InvalidName(
            "name cannot contain control characters".to_owned(),
        ));
    }
    Ok(())
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_database(name: &str) -> PathBuf {
        let unique = format!(
            "context-capsule-{name}-{}-{}.db",
            std::process::id(),
            now_unix_ms()
        );
        env::temp_dir().join(unique)
    }

    fn snapshot(marker: &str) -> StoredCapsuleSnapshot {
        StoredCapsuleSnapshot {
            schema_version: CAPSULE_SCHEMA_VERSION,
            captured_at_unix_ms: 123,
            snapshot: serde_json::json!({
                "marker": marker,
                "docker": DockerSnapshot::not_requested(),
            }),
        }
    }

    #[test]
    fn sqlite_round_trip_list_and_delete() {
        let path = temporary_database("round-trip");
        {
            let mut store = CapsuleStore::open_at(&path).expect("open store");
            store
                .save("Workspace", &snapshot("first"), false)
                .expect("save capsule");

            let loaded = store.load("workspace").expect("case-insensitive load");
            assert_eq!(loaded.snapshot["marker"], "first");

            let listed = store.list().expect("list capsules");
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].name, "Workspace");

            store.delete("WORKSPACE").expect("delete capsule");
            assert!(matches!(
                store.load("Workspace"),
                Err(PersistenceError::NotFound(_))
            ));
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn duplicate_requires_force_and_force_replaces_payload() {
        let path = temporary_database("replace");
        {
            let mut store = CapsuleStore::open_at(&path).expect("open store");
            store
                .save("demo", &snapshot("one"), false)
                .expect("first save");
            assert!(matches!(
                store.save("DEMO", &snapshot("two"), false),
                Err(PersistenceError::AlreadyExists(_))
            ));

            store
                .save("demo", &snapshot("two"), true)
                .expect("forced save");
            assert_eq!(store.load("demo").expect("load").snapshot["marker"], "two");
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_names_are_rejected_before_database_work() {
        assert!(validate_capsule_name("").is_err());
        assert!(validate_capsule_name("   ").is_err());
        assert!(validate_capsule_name("bad\nname").is_err());
        assert!(validate_capsule_name(&"x".repeat(129)).is_err());
        assert!(validate_capsule_name("a normal workspace").is_ok());
    }

    #[test]
    fn docker_section_is_typed_when_loading_snapshot() {
        let expected = DockerSnapshot::not_requested();
        let stored = StoredCapsuleSnapshot {
            schema_version: 1,
            captured_at_unix_ms: 1,
            snapshot: serde_json::json!({ "docker": expected }),
        };

        assert_eq!(stored.docker().expect("docker snapshot"), expected);
    }
}
