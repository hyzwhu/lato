use std::{fs, path::Path, process::Command};

fn write_workflow_plugin(parent: &Path, name: &str) {
    let root = parent.join(name);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("plugin.json"),
        serde_json::json!({
            "name": name,
            "workflows": {
                "review-changes": {
                    "description": "Review a diff",
                    "prompt": "Inspect the patch",
                    "profile": "explorer"
                }
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn workflow_list_and_run_use_plugin_dir() {
    let fixture = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_workflow_plugin(fixture.path(), "demo");
    let plugin = fixture.path().join("demo");
    let list = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(fixture.path())
        .env("LATO_HOME", home.path())
        .args(["--plugin-dir", plugin.to_str().unwrap(), "workflow", "list"])
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(
        listed.contains("demo/review-changes"),
        "list stdout: {listed}"
    );

    let run = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(fixture.path())
        .env("LATO_HOME", home.path())
        .args([
            "--plugin-dir",
            plugin.to_str().unwrap(),
            "workflow",
            "run",
            "demo/review-changes",
            "--input",
            "{\"path\":\"src/lib.rs\"}",
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let body = String::from_utf8_lossy(&run.stdout);
    assert!(body.contains("demo/review-changes"), "run stdout: {body}");
    assert!(body.contains("Completed"), "run stdout: {body}");
}

#[test]
fn workflow_run_unknown_id_fails() {
    let fixture = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(fixture.path())
        .env("LATO_HOME", fixture.path().join("home"))
        .args(["workflow", "run", "missing/none"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("not found") || err.contains("error"), "{err}");
}
