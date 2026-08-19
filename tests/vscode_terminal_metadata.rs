use context_capsule::vscode::IntegratedTerminalSnapshot;

#[test]
fn active_terminal_metadata_round_trips() {
    let raw = serde_json::json!({
        "name": "PowerShell",
        "kind": "process",
        "restorable": true,
        "active": true,
        "shellPath": "pwsh.exe",
        "cwd": "file:///C:/work/tri-up",
        "cwdIsUri": true
    });

    let terminal: IntegratedTerminalSnapshot = serde_json::from_value(raw).unwrap();
    assert_eq!(terminal.active, Some(true));

    let serialized = serde_json::to_value(terminal).unwrap();
    assert_eq!(
        serialized.get("active").and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[test]
fn old_terminal_snapshot_without_active_field_stays_valid() {
    let raw = serde_json::json!({
        "name": "PowerShell",
        "kind": "process",
        "restorable": true,
        "cwd": "C:/work/tri-up",
        "cwdIsUri": false
    });

    let terminal: IntegratedTerminalSnapshot = serde_json::from_value(raw).unwrap();
    assert_eq!(terminal.active, None);

    let serialized = serde_json::to_value(terminal).unwrap();
    assert!(serialized.get("active").is_none());
}
