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
