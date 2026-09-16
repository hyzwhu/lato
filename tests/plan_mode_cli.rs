//! T4/T7 — Plan-mode CLI paths: headless `--plan` exit code 3 with no
//! auto-approval, argument conflicts, and fail-closed `resume --plan` draft
//! validation (spec §2).

use std::process::Command;

fn lato(home: &tempfile::TempDir, workspace: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lato"));
    command
        .env("LATO_HOME", home.path())
        .current_dir(workspace.path());
    command
}

#[test]
fn headless_plan_exits_3_when_a_plan_was_produced_but_not_approved() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = lato(&home, &workspace)
        .args(["-p", "--plan", "draft a plan for the workspace"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "headless --plan must exit 3 when the plan was not approved; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("plan file:"),
        "must print the plan path: {stdout}"
    );
    // No auto-approval: the session ended with the plan unapproved, and no
    // approval record could have been created headlessly.
    assert!(!stdout.contains("approved"));
}

#[test]
fn headless_without_plan_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = lato(&home, &workspace)
        .args(["-p", "say hello"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plan_flag_is_rejected_outside_headless_and_resume() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let output = lato(&home, &workspace)
        .args(["doctor", "--plan"])
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert_eq!(
        output.status.code(),
        Some(2),
        "--plan outside -p/resume is a usage error"
    );
}

#[test]
fn resume_plan_fails_closed_on_unreadable_or_oversized_draft() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    // Oversized draft (131,073 bytes): resume --plan must refuse before any
    // session work, with exit code 1.
    std::fs::write(workspace.path().join("plan.md"), vec![b'a'; 131_073]).unwrap();
    let output = lato(&home, &workspace)
        .args(["resume", "whatever", "--plan"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--plan draft"), "{stderr}");

    // A directory in place of the draft is equally refused.
    std::fs::remove_file(workspace.path().join("plan.md")).unwrap();
    std::fs::create_dir(workspace.path().join("plan.md")).unwrap();
    let output = lato(&home, &workspace)
        .args(["resume", "whatever", "--plan"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));

    // A missing draft is fine: entry proceeds far enough to resolve the
    // (missing) session instead of failing on the draft.
    std::fs::remove_dir(workspace.path().join("plan.md")).unwrap();
    let output = lato(&home, &workspace)
        .args(["resume", "definitely-missing-session", "--plan"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no session has the supplied ID or title"),
        "missing draft must not block resume: {stderr}"
    );
}
