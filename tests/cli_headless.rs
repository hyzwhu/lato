use std::process::Command;

#[test]
fn a5_1_headless_prompt_fake_model() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["-p", "reply with hi only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
}

#[test]
fn a5_2_respects_lato_home() {
    let d = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["-p", "hi"])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn a0_4_headless_ask_requires_tty() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["-p", "--ask", "ping"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("requires a tty"));
}

#[test]
fn a1_7_cli_uses_acp_not_actor_prompt_symbol() {
    let src = std::fs::read_to_string("src/cli.rs").unwrap();
    assert!(!src.contains("SessionActor"));
    assert!(!src.contains(".prompt("));
    assert!(
        std::fs::read_to_string("src/client.rs")
            .unwrap()
            .contains("AcpHost")
    );
}

#[test]
fn a4_5_login_openai_api_key_writes_store() {
    let d = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["login", "openai", "--api-key", "sk-test"])
        .status()
        .unwrap();
    assert!(status.success());
    let text = std::fs::read_to_string(d.path().join("auth.json")).unwrap();
    assert!(text.contains("openai"));
    assert!(text.contains("api_key"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.path().join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn a4_4_cli_rejects_xai_oauth() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["login", "xai", "--oauth"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!d.path().join("auth.json").exists());
}

#[test]
fn a4_6_auth_status_does_not_leak_key_surface() {
    // Phase 0 exposes status through ACP host; CLI has no status command yet, so ensure login output does not echo secret.
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["login", "openai", "--api-key", "sk-secret"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("sk-secret"));
}
