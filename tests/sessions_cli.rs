use std::process::Command;

fn lato(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lato"));
    command.env("LATO_HOME", home);
    command
}

fn create_session(home: &std::path::Path) -> String {
    let output = lato(home)
        .args(["-p", "Implement session metadata"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = lato(home).args(["sessions", "--json"]).output().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    body["sessions"][0]["sessionId"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn rename_and_confirmed_delete_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let session_id = create_session(home.path());

    let renamed = lato(home.path())
        .args(["sessions", "rename", &session_id, "Manual", "title"])
        .output()
        .unwrap();
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let listed = lato(home.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(body["sessions"][0]["title"], "Manual title");
    assert_eq!(body["sessions"][0]["titleSource"], "manual");

    let deleted = lato(home.path())
        .args(["sessions", "delete", &session_id, "--yes"])
        .output()
        .unwrap();
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    let listed = lato(home.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(body["sessions"], serde_json::json!([]));
}

#[test]
fn non_interactive_delete_requires_yes() {
    let home = tempfile::tempdir().unwrap();
    let session_id = create_session(home.path());
    let output = lato(home.path())
        .args(["sessions", "delete", &session_id])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
    let listed = lato(home.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
}
