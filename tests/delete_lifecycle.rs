use context_capsule::persistence::{
    CAPSULE_SCHEMA_VERSION, CapsuleStore, PersistenceError, StoredCapsuleSnapshot,
};
use serde_json::json;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn temporary_database(name: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir().join(format!(
        "context-capsule-delete-{name}-{}-{now}.db",
        std::process::id()
    ))
}

fn snapshot(marker: &str) -> StoredCapsuleSnapshot {
    StoredCapsuleSnapshot {
        schema_version: CAPSULE_SCHEMA_VERSION,
        captured_at_unix_ms: 123,
        snapshot: json!({ "marker": marker }),
    }
}

fn cleanup_database(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(PathBuf::from(format!("{}-wal", path.display())));
    let _ = fs::remove_file(PathBuf::from(format!("{}-shm", path.display())));
}

#[test]
fn repeated_create_delete_leaves_no_stale_capsule() {
    let path = temporary_database("repeat");
    {
        let mut store = CapsuleStore::open_at(&path).expect("open store");
        for round in 0..6 {
            store
                .save("Delete Target", &snapshot(&format!("round-{round}")), false)
                .expect("create capsule");
            assert_eq!(store.list().expect("list after create").len(), 1);

            store.delete("DELETE TARGET").expect("delete capsule");
            assert!(matches!(
                store.load("Delete Target"),
                Err(PersistenceError::NotFound(_))
            ));
            assert!(store.list().expect("list after delete").is_empty());
        }
    }
    cleanup_database(&path);
}

#[test]
fn deleting_one_capsule_does_not_touch_another_capsule() {
    let path = temporary_database("selective");
    {
        let mut store = CapsuleStore::open_at(&path).expect("open store");
        store
            .save("First", &snapshot("first"), false)
            .expect("save first capsule");
        store
            .save("Second", &snapshot("second"), false)
            .expect("save second capsule");

        store.delete("First").expect("delete first capsule");

        assert!(matches!(
            store.load("First"),
            Err(PersistenceError::NotFound(_))
        ));
        assert_eq!(
            store.load("Second").expect("second remains").snapshot["marker"],
            "second"
        );
        let names = store
            .list()
            .expect("list remaining capsules")
            .into_iter()
            .map(|capsule| capsule.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Second"]);
    }
    cleanup_database(&path);
}
