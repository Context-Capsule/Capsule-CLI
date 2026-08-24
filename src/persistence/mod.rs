use crate::adapters::docker::DockerSnapshot;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
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
const DATABASE_SCHEMA_VERSION: u32 = 2;

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
pub struct CapsuleReference {
    pub name: String,
    pub revision: Option<u32>,
}

impl CapsuleReference {
    pub fn display(&self) -> String {
        match self.revision {
            Some(revision) => format!("{}@{revision}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleSummary {
    pub name: String,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub schema_version: u32,
    pub current_revision: u32,
    pub revision_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRevisionSummary {
    pub name: String,
    pub revision: u32,
    pub created_at_unix_ms: i64,
    pub schema_version: u32,
    pub current: bool,
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
                "capsule '{name}' already exists; use 'capsule update {name}' or 'capsule save {name} --force' to create a new revision"
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

impl From<serde_json::Error> for PersistenceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
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
             CREATE TABLE IF NOT EXISTS capsule_revisions (\n\
                 id INTEGER PRIMARY KEY AUTOINCREMENT,\n\
                 capsule_id INTEGER NOT NULL,\n\
                 revision INTEGER NOT NULL,\n\
                 created_at_unix_ms INTEGER NOT NULL,\n\
                 schema_version INTEGER NOT NULL,\n\
                 payload_json TEXT NOT NULL,\n\
                 FOREIGN KEY(capsule_id) REFERENCES capsules(id) ON DELETE CASCADE,\n\
                 UNIQUE(capsule_id, revision)\n\
             );\n\
             CREATE INDEX IF NOT EXISTS idx_capsule_revisions_capsule_revision\n\
                 ON capsule_revisions(capsule_id, revision DESC);\n\
             INSERT OR IGNORE INTO capsule_revisions\n\
                 (capsule_id, revision, created_at_unix_ms, schema_version, payload_json)\n\
             SELECT id, 1, updated_at_unix_ms, schema_version, payload_json\n\
             FROM capsules;\n\
             PRAGMA user_version = 2;",
        )?;

        Ok(Self { connection, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn health_check(&self) -> Result<(), PersistenceError> {
        let result: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if result.eq_ignore_ascii_case("ok") {
            Ok(())
        } else {
            Err(PersistenceError::InvalidPayload(format!(
                "SQLite quick_check returned '{result}'"
            )))
        }
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
                "SELECT id, created_at_unix_ms FROM capsules WHERE name = ?1 COLLATE NOCASE",
                [name],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;

        let (capsule_id, created_at, revision) = match existing {
            Some(_) if !replace => {
                return Err(PersistenceError::AlreadyExists(name.to_owned()));
            }
            Some((capsule_id, created_at)) => {
                let revision = next_revision(&transaction, capsule_id)?;
                transaction.execute(
                    "INSERT INTO capsule_revisions\n\
                     (capsule_id, revision, created_at_unix_ms, schema_version, payload_json)\n\
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        capsule_id,
                        revision,
                        now,
                        snapshot.schema_version,
                        payload_json
                    ],
                )?;
                transaction.execute(
                    "UPDATE capsules\n\
                     SET name = ?1, updated_at_unix_ms = ?2, schema_version = ?3, payload_json = ?4\n\
                     WHERE id = ?5",
                    params![
                        name,
                        now,
                        snapshot.schema_version,
                        payload_json,
                        capsule_id
                    ],
                )?;
                (capsule_id, created_at, revision)
            }
            None => {
                transaction.execute(
                    "INSERT INTO capsules\n\
                     (name, created_at_unix_ms, updated_at_unix_ms, schema_version, payload_json)\n\
                     VALUES (?1, ?2, ?2, ?3, ?4)",
                    params![name, now, snapshot.schema_version, payload_json],
                )?;
                let capsule_id = transaction.last_insert_rowid();
                transaction.execute(
                    "INSERT INTO capsule_revisions\n\
                     (capsule_id, revision, created_at_unix_ms, schema_version, payload_json)\n\
                     VALUES (?1, 1, ?2, ?3, ?4)",
                    params![capsule_id, now, snapshot.schema_version, payload_json],
                )?;
                (capsule_id, now, 1)
            }
        };

        let revision_count = revision_count_for(&transaction, capsule_id)?;
        transaction.commit()?;
        Ok(CapsuleSummary {
            name: name.to_owned(),
            created_at_unix_ms: created_at,
            updated_at_unix_ms: now,
            schema_version: snapshot.schema_version,
            current_revision: revision,
            revision_count,
        })
    }

    pub fn load(&self, reference: &str) -> Result<StoredCapsuleSnapshot, PersistenceError> {
        let reference = parse_capsule_reference(reference)?;
        let payload = match reference.revision {
            Some(revision) => self
                .connection
                .query_row(
                    "SELECT r.payload_json\n\
                     FROM capsule_revisions r\n\
                     JOIN capsules c ON c.id = r.capsule_id\n\
                     WHERE c.name = ?1 COLLATE NOCASE AND r.revision = ?2",
                    params![reference.name, revision],
                    |row| row.get::<_, String>(0),
                )
                .optional()?,
            None => self
                .connection
                .query_row(
                    "SELECT payload_json FROM capsules WHERE name = ?1 COLLATE NOCASE",
                    [reference.name.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?,
        }
        .ok_or_else(|| PersistenceError::NotFound(reference.display()))?;

        serde_json::from_str(&payload).map_err(PersistenceError::Json)
    }

    pub fn list(&self) -> Result<Vec<CapsuleSummary>, PersistenceError> {
        let mut statement = self.connection.prepare(
            "SELECT c.name, c.created_at_unix_ms, c.updated_at_unix_ms, c.schema_version,\n\
                    COALESCE(MAX(r.revision), 1) AS current_revision,\n\
                    COUNT(r.id) AS revision_count\n\
             FROM capsules c\n\
             LEFT JOIN capsule_revisions r ON r.capsule_id = c.id\n\
             GROUP BY c.id, c.name, c.created_at_unix_ms, c.updated_at_unix_ms, c.schema_version\n\
             ORDER BY c.updated_at_unix_ms DESC, c.name COLLATE NOCASE ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(CapsuleSummary {
                name: row.get(0)?,
                created_at_unix_ms: row.get(1)?,
                updated_at_unix_ms: row.get(2)?,
                schema_version: row.get(3)?,
                current_revision: row.get(4)?,
                revision_count: row.get(5)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PersistenceError::Database)
    }

    pub fn history(&self, name: &str) -> Result<Vec<CapsuleRevisionSummary>, PersistenceError> {
        let reference = parse_capsule_reference(name)?;
        if reference.revision.is_some() {
            return Err(PersistenceError::InvalidName(
                "history expects a capsule name without @revision".to_owned(),
            ));
        }

        let capsule = self
            .connection
            .query_row(
                "SELECT id FROM capsules WHERE name = ?1 COLLATE NOCASE",
                [reference.name.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| PersistenceError::NotFound(reference.name.clone()))?;

        let current_revision: u32 = self.connection.query_row(
            "SELECT COALESCE(MAX(revision), 1) FROM capsule_revisions WHERE capsule_id = ?1",
            [capsule],
            |row| row.get(0),
        )?;
        let mut statement = self.connection.prepare(
            "SELECT revision, created_at_unix_ms, schema_version\n\
             FROM capsule_revisions\n\
             WHERE capsule_id = ?1\n\
             ORDER BY revision DESC",
        )?;
        let rows = statement.query_map([capsule], |row| {
            let revision: u32 = row.get(0)?;
            Ok(CapsuleRevisionSummary {
                name: reference.name.clone(),
                revision,
                created_at_unix_ms: row.get(1)?,
                schema_version: row.get(2)?,
                current: revision == current_revision,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PersistenceError::Database)
    }

    pub fn delete(&mut self, name: &str) -> Result<(), PersistenceError> {
        let reference = parse_capsule_reference(name)?;
        if reference.revision.is_some() {
            return Err(PersistenceError::InvalidName(
                "delete removes a capsule and all revisions; pass the capsule name without @revision"
                    .to_owned(),
            ));
        }
        let transaction = self.connection.transaction()?;
        let deleted = transaction.execute(
            "DELETE FROM capsules WHERE name = ?1 COLLATE NOCASE",
            [reference.name.as_str()],
        )?;
        if deleted == 0 {
            return Err(PersistenceError::NotFound(reference.name));
        }
        transaction.commit()?;
        Ok(())
    }
}

fn next_revision(transaction: &Transaction<'_>, capsule_id: i64) -> Result<u32, PersistenceError> {
    let current: u32 = transaction.query_row(
        "SELECT COALESCE(MAX(revision), 0) FROM capsule_revisions WHERE capsule_id = ?1",
        [capsule_id],
        |row| row.get(0),
    )?;
    current
        .checked_add(1)
        .ok_or_else(|| PersistenceError::InvalidPayload("capsule revision overflow".to_owned()))
}

fn revision_count_for(
    transaction: &Transaction<'_>,
    capsule_id: i64,
) -> Result<u32, PersistenceError> {
    transaction
        .query_row(
            "SELECT COUNT(*) FROM capsule_revisions WHERE capsule_id = ?1",
            [capsule_id],
            |row| row.get(0),
        )
        .map_err(PersistenceError::Database)
}

pub fn parse_capsule_reference(value: &str) -> Result<CapsuleReference, PersistenceError> {
    if let Some((name, suffix)) = value.rsplit_once('@') {
        if !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit()) {
            validate_capsule_name(name)?;
            let revision = suffix.parse::<u32>().map_err(|_| {
                PersistenceError::InvalidName(format!("revision '{suffix}' is out of range"))
            })?;
            if revision == 0 {
                return Err(PersistenceError::InvalidName(
                    "revision numbers start at 1".to_owned(),
                ));
            }
            return Ok(CapsuleReference {
                name: name.to_owned(),
                revision: Some(revision),
            });
        }
    }

    validate_capsule_name(value)?;
    Ok(CapsuleReference {
        name: value.to_owned(),
        revision: None,
    })
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
    fn sqlite_round_trip_list_history_and_delete() {
        let path = temporary_database("round-trip");
        {
            let mut store = CapsuleStore::open_at(&path).expect("open store");
            let summary = store
                .save("Workspace", &snapshot("first"), false)
                .expect("save capsule");
            assert_eq!(summary.current_revision, 1);
            assert_eq!(summary.revision_count, 1);

            let loaded = store.load("workspace").expect("case-insensitive load");
            assert_eq!(loaded.snapshot["marker"], "first");

            let history = store.history("WORKSPACE").expect("history");
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].revision, 1);
            assert!(history[0].current);

            let listed = store.list().expect("list capsules");
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].name, "Workspace");
            assert_eq!(listed[0].current_revision, 1);

            store.delete("WORKSPACE").expect("delete capsule");
            assert!(matches!(
                store.load("Workspace"),
                Err(PersistenceError::NotFound(_))
            ));
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn forced_save_creates_immutable_revision_instead_of_destroying_history() {
        let path = temporary_database("revisions");
        {
            let mut store = CapsuleStore::open_at(&path).expect("open store");
            store
                .save("demo", &snapshot("one"), false)
                .expect("first save");
            assert!(matches!(
                store.save("DEMO", &snapshot("two"), false),
                Err(PersistenceError::AlreadyExists(_))
            ));

            let summary = store
                .save("demo", &snapshot("two"), true)
                .expect("new revision");
            assert_eq!(summary.current_revision, 2);
            assert_eq!(summary.revision_count, 2);
            assert_eq!(
                store.load("demo").expect("current").snapshot["marker"],
                "two"
            );
            assert_eq!(
                store.load("demo@1").expect("revision one").snapshot["marker"],
                "one"
            );
            assert_eq!(
                store.load("demo@2").expect("revision two").snapshot["marker"],
                "two"
            );

            let history = store.history("demo").expect("history");
            assert_eq!(
                history.iter().map(|item| item.revision).collect::<Vec<_>>(),
                vec![2, 1]
            );
            assert!(history[0].current);
            assert!(!history[1].current);
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn existing_v1_database_is_backfilled_as_revision_one() {
        let path = temporary_database("migration");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE capsules (\n\
                         id INTEGER PRIMARY KEY AUTOINCREMENT,\n\
                         name TEXT NOT NULL COLLATE NOCASE UNIQUE,\n\
                         created_at_unix_ms INTEGER NOT NULL,\n\
                         updated_at_unix_ms INTEGER NOT NULL,\n\
                         schema_version INTEGER NOT NULL,\n\
                         payload_json TEXT NOT NULL\n\
                     );\n\
                     PRAGMA user_version = 1;",
                )
                .unwrap();
            let payload = serde_json::to_string(&snapshot("legacy")).unwrap();
            connection
                .execute(
                    "INSERT INTO capsules\n\
                     (name, created_at_unix_ms, updated_at_unix_ms, schema_version, payload_json)\n\
                     VALUES ('legacy', 10, 20, 1, ?1)",
                    [payload],
                )
                .unwrap();
        }

        {
            let mut store = CapsuleStore::open_at(&path).expect("migrated store");
            assert_eq!(store.load("legacy@1").unwrap().snapshot["marker"], "legacy");
            let history = store.history("legacy").unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].created_at_unix_ms, 20);
            store.save("legacy", &snapshot("new"), true).unwrap();
            assert_eq!(store.load("legacy@1").unwrap().snapshot["marker"], "legacy");
            assert_eq!(store.load("legacy@2").unwrap().snapshot["marker"], "new");
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn capsule_reference_parsing_preserves_non_numeric_at_names() {
        assert_eq!(
            parse_capsule_reference("demo@12").unwrap(),
            CapsuleReference {
                name: "demo".to_owned(),
                revision: Some(12)
            }
        );
        assert_eq!(
            parse_capsule_reference("team@work").unwrap(),
            CapsuleReference {
                name: "team@work".to_owned(),
                revision: None
            }
        );
        assert!(parse_capsule_reference("demo@0").is_err());
        assert!(parse_capsule_reference("@2").is_err());
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

    #[test]
    fn database_health_check_reports_ok_for_initialized_store() {
        let path = temporary_database("health");
        let store = CapsuleStore::open_at(&path).unwrap();
        store.health_check().unwrap();
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn database_schema_is_upgraded_to_version_two() {
        let path = temporary_database("user-version");
        let store = CapsuleStore::open_at(&path).unwrap();
        let version: u32 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, DATABASE_SCHEMA_VERSION);
        drop(store);
        let _ = fs::remove_file(path);
    }
}
