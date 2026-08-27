use crate::persistence::{PersistenceError, default_database_path, parse_capsule_reference};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    error::Error,
    fmt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_NOTE_CHARS: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationNote {
    pub capsule_name: String,
    pub revision: u32,
    pub message: String,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug)]
pub enum ContinuationNoteError {
    Persistence(PersistenceError),
    Database(rusqlite::Error),
    InvalidMessage(String),
    NotFound(String),
}

impl fmt::Display for ContinuationNoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persistence(error) => write!(formatter, "{error}"),
            Self::Database(error) => write!(formatter, "SQLite error: {error}"),
            Self::InvalidMessage(message) => write!(formatter, "invalid continuation note: {message}"),
            Self::NotFound(reference) => write!(formatter, "capsule '{reference}' was not found"),
        }
    }
}

impl Error for ContinuationNoteError {}

impl From<PersistenceError> for ContinuationNoteError {
    fn from(error: PersistenceError) -> Self {
        Self::Persistence(error)
    }
}

impl From<rusqlite::Error> for ContinuationNoteError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub fn set(reference: &str, message: &str) -> Result<ContinuationNote, ContinuationNoteError> {
    let path = default_database_path()?;
    set_at(&path, reference, message)
}

pub fn get(reference: &str) -> Result<Option<ContinuationNote>, ContinuationNoteError> {
    let path = default_database_path()?;
    get_at(&path, reference)
}

fn set_at(
    path: &Path,
    reference: &str,
    message: &str,
) -> Result<ContinuationNote, ContinuationNoteError> {
    let message = validate_message(message)?;
    let connection = open(path)?;
    let (capsule_id, capsule_name, revision) = resolve_reference(&connection, reference)?;
    let updated_at_unix_ms = now_unix_ms();

    connection.execute(
        "INSERT INTO capsule_continuation_notes
         (capsule_id, revision, message, updated_at_unix_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(capsule_id, revision) DO UPDATE SET
             message = excluded.message,
             updated_at_unix_ms = excluded.updated_at_unix_ms",
        params![capsule_id, revision, message, updated_at_unix_ms],
    )?;

    Ok(ContinuationNote {
        capsule_name,
        revision,
        message: message.to_owned(),
        updated_at_unix_ms,
    })
}

fn get_at(
    path: &Path,
    reference: &str,
) -> Result<Option<ContinuationNote>, ContinuationNoteError> {
    let connection = open(path)?;
    let (capsule_id, capsule_name, revision) = resolve_reference(&connection, reference)?;
    let row = connection
        .query_row(
            "SELECT message, updated_at_unix_ms
             FROM capsule_continuation_notes
             WHERE capsule_id = ?1 AND revision = ?2",
            params![capsule_id, revision],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;

    Ok(row.map(|(message, updated_at_unix_ms)| ContinuationNote {
        capsule_name,
        revision,
        message,
        updated_at_unix_ms,
    }))
}

fn open(path: &Path) -> Result<Connection, ContinuationNoteError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS capsule_continuation_notes (
             capsule_id INTEGER NOT NULL,
             revision INTEGER NOT NULL,
             message TEXT NOT NULL,
             updated_at_unix_ms INTEGER NOT NULL,
             PRIMARY KEY(capsule_id, revision),
             FOREIGN KEY(capsule_id) REFERENCES capsules(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_capsule_continuation_notes_capsule_revision
             ON capsule_continuation_notes(capsule_id, revision);",
    )?;
    Ok(connection)
}

fn resolve_reference(
    connection: &Connection,
    reference: &str,
) -> Result<(i64, String, u32), ContinuationNoteError> {
    let parsed = parse_capsule_reference(reference)?;
    let capsule = connection
        .query_row(
            "SELECT id, name FROM capsules WHERE name = ?1 COLLATE NOCASE",
            [parsed.name.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(|| ContinuationNoteError::NotFound(parsed.display()))?;

    let revision = match parsed.revision {
        Some(revision) => {
            let exists = connection
                .query_row(
                    "SELECT 1
                     FROM capsule_revisions
                     WHERE capsule_id = ?1 AND revision = ?2",
                    params![capsule.0, revision],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(ContinuationNoteError::NotFound(parsed.display()));
            }
            revision
        }
        None => connection.query_row(
            "SELECT COALESCE(MAX(revision), 1)
             FROM capsule_revisions
             WHERE capsule_id = ?1",
            [capsule.0],
            |row| row.get(0),
        )?,
    };

    Ok((capsule.0, capsule.1, revision))
}

fn validate_message(message: &str) -> Result<&str, ContinuationNoteError> {
    let message = message.trim();
    if message.is_empty() {
        return Err(ContinuationNoteError::InvalidMessage(
            "message cannot be empty".to_owned(),
        ));
    }
    if message.chars().count() > MAX_NOTE_CHARS {
        return Err(ContinuationNoteError::InvalidMessage(format!(
            "message cannot exceed {MAX_NOTE_CHARS} characters"
        )));
    }
    if message.chars().any(|character| character == '\0') {
        return Err(ContinuationNoteError::InvalidMessage(
            "message cannot contain NUL characters".to_owned(),
        ));
    }
    Ok(message)
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
    use crate::{
        adapters::docker::DockerSnapshot,
        persistence::{CAPSULE_SCHEMA_VERSION, CapsuleStore, StoredCapsuleSnapshot},
    };
    use serde_json::json;
    use std::{env, fs, path::PathBuf};

    fn temporary_database(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "context-capsule-notes-{name}-{}-{}.db",
            std::process::id(),
            now_unix_ms()
        ))
    }

    fn snapshot(marker: &str) -> StoredCapsuleSnapshot {
        StoredCapsuleSnapshot {
            schema_version: CAPSULE_SCHEMA_VERSION,
            captured_at_unix_ms: 123,
            snapshot: json!({
                "marker": marker,
                "docker": DockerSnapshot::not_requested(),
            }),
        }
    }

    #[test]
    fn note_is_attached_to_exact_revision_without_mutating_snapshot_history() {
        let path = temporary_database("revision");
        {
            let mut store = CapsuleStore::open_at(&path).unwrap();
            store.save("demo", &snapshot("one"), false).unwrap();
            set_at(&path, "demo", "continue from revision one").unwrap();

            store.save("demo", &snapshot("two"), true).unwrap();
            set_at(&path, "demo", "continue from revision two").unwrap();

            assert_eq!(
                get_at(&path, "demo@1").unwrap().unwrap().message,
                "continue from revision one"
            );
            let current = get_at(&path, "demo").unwrap().unwrap();
            assert_eq!(current.revision, 2);
            assert_eq!(current.message, "continue from revision two");

            assert_eq!(
                store.load("demo@1").unwrap().snapshot["marker"],
                "one"
            );
            assert_eq!(
                store.load("demo@2").unwrap().snapshot["marker"],
                "two"
            );
            assert_eq!(store.history("demo").unwrap().len(), 2);
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn updating_note_does_not_create_a_capsule_revision() {
        let path = temporary_database("update");
        {
            let mut store = CapsuleStore::open_at(&path).unwrap();
            store.save("demo", &snapshot("one"), false).unwrap();

            set_at(&path, "demo", "first note").unwrap();
            set_at(&path, "DEMO@1", "replacement note").unwrap();

            assert_eq!(store.history("demo").unwrap().len(), 1);
            assert_eq!(
                get_at(&path, "demo").unwrap().unwrap().message,
                "replacement note"
            );
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn notes_are_deleted_with_the_capsule() {
        let path = temporary_database("delete");
        {
            let mut store = CapsuleStore::open_at(&path).unwrap();
            store.save("demo", &snapshot("one"), false).unwrap();
            set_at(&path, "demo", "temporary note").unwrap();
            store.delete("demo").unwrap();

            assert!(matches!(
                get_at(&path, "demo"),
                Err(ContinuationNoteError::NotFound(_))
            ));
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn empty_and_oversized_notes_are_rejected() {
        assert!(validate_message("   ").is_err());
        assert!(validate_message(&"x".repeat(MAX_NOTE_CHARS + 1)).is_err());
        assert!(validate_message("do the next useful thing").is_ok());
    }
}
