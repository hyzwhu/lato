use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn write_plugin(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(root.join("skills")).unwrap();
    std::fs::write(
        root.join("plugin.json"),
        serde_json::to_vec(&serde_json::json!({
            "name": name,
            "version": "1.0.0",
            "skills": "skills"
        }))
        .unwrap(),
    )
    .unwrap();
    root
}

#[test]
fn repeated_plugin_dirs_reach_the_session_snapshot() {
    let fixture = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let first = write_plugin(fixture.path(), "first");
    let second = write_plugin(fixture.path(), "second");
    let mut child = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(fixture.path())
        .env("LATO_HOME", home.path())
        .arg("--plugin-dir")
        .arg(&first)
        .arg("--plugin-dir")
        .arg(&second)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"lato/plugins/reload","params":{"force":true}}),
    ] {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let reload = responses
        .iter()
        .find(|response| response["id"] == 3)
        .expect("reload response");
    assert_eq!(reload["result"]["active"], 2);
    assert!(reload["result"]["generation"].as_u64().unwrap() >= 2);
}

#[test]
fn missing_cli_plugin_root_fails_before_session_start() {
    let fixture = tempfile::tempdir().unwrap();
    let missing = fixture.path().join("missing-plugin");
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(fixture.path())
        .env("LATO_HOME", fixture.path().join("home"))
        .arg("--plugin-dir")
        .arg(&missing)
        .args(["-p", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("plugin directory not found"));
    assert!(!fixture.path().join("home/sessions").exists());
}
