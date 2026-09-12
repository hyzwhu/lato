use std::process::Command;

#[test]
fn help_advertises_the_bilingual_interactive_interface() {
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--lang"), "{help}");
    assert!(help.contains("zh-CN"), "{help}");
    assert!(help.contains("--sandbox"), "{help}");
    assert!(help.contains("Interactive: lato [--sandbox"), "{help}");
    assert!(help.contains("lato resume ID|TITLE"), "{help}");
}

#[test]
fn language_is_rejected_outside_interactive_modes() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["--lang", "en", "sessions"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive mode"));
}

#[test]
fn interactive_mode_still_fails_cleanly_without_a_tty() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["--lang", "zh-CN"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a tty"));
}

#[test]
fn sandbox_option_reaches_interactive_startup_for_new_and_resumed_sessions() {
    let home = tempfile::tempdir().unwrap();
    let created = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["-p", "reply with hi only"])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let listed = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let session_id = body["sessions"][0]["sessionId"].as_str().unwrap();
    for args in [
        vec!["--sandbox".into(), "off".into()],
        vec![
            "resume".into(),
            session_id.to_string(),
            "--sandbox".into(),
            "read-only".into(),
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", home.path())
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires a tty"));
    }
}
