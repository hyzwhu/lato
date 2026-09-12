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
        .env_remove("LATO_MODEL")
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
        .env_remove("LATO_MODEL")
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
    let payload: serde_json::Value = serde_json::from_str(body.trim())
        .unwrap_or_else(|error| panic!("run stdout is not JSON ({error}): {body}"));
    assert_eq!(payload["runId"], "wf-1");
    assert_eq!(payload["status"], "completed");
    assert!(
        payload.get("output").is_some() && payload["output"].get("workflow").is_none(),
        "expected host ScriptOutcome output, got: {body}"
    );
}

#[test]
fn workflow_validate_only_does_not_need_a_model() {
    let fixture = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_workflow_plugin(fixture.path(), "demo");
    let plugin = fixture.path().join("demo");
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(fixture.path())
        .env("LATO_HOME", home.path())
        .env_remove("LATO_MODEL")
        .args([
            "--plugin-dir",
            plugin.to_str().unwrap(),
            "workflow",
            "run",
            "demo/review-changes",
            "--validate-only",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = String::from_utf8_lossy(&output.stdout);
    let payload: serde_json::Value = serde_json::from_str(body.trim())
        .unwrap_or_else(|error| panic!("validate-only stdout is not JSON ({error}): {body}"));
    assert_eq!(payload["status"], "validated");
    assert_eq!(payload["name"], "review-changes");
    assert!(
        payload["outcome"]
            .as_str()
            .is_some_and(|outcome| outcome.contains("completed")),
        "validate-only stdout: {body}"
    );
}

#[test]
fn workflow_run_rejects_invalid_agent_budget() {
    let fixture = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_workflow_plugin(fixture.path(), "demo");
    let plugin = fixture.path().join("demo");
    for budget in ["0", "1025"] {
        let output = Command::new(env!("CARGO_BIN_EXE_lato"))
            .current_dir(fixture.path())
            .env("LATO_HOME", home.path())
            .env_remove("LATO_MODEL")
            .args([
                "--plugin-dir",
                plugin.to_str().unwrap(),
                "workflow",
                "run",
                "demo/review-changes",
                "--agent-budget",
                budget,
            ])
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "budget {budget}: {err}");
        assert!(
            err.contains("workflow.invalid_configuration"),
            "budget {budget}: {err}"
        );
    }
}

#[test]
fn workflow_run_unknown_id_fails() {
    let fixture = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(fixture.path())
        .env("LATO_HOME", fixture.path().join("home"))
        .env_remove("LATO_MODEL")
        .args(["workflow", "run", "missing/none"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("workflow.not_found"), "{err}");
}
