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
fn headless_plan_without_a_produced_plan_file_is_an_explicit_failure() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // The fake model never calls plan_draft, so no plan.md is produced: the
    // run must NOT report the pending-approval code 3, and the nonexistent
    // plan path must not be presented as a deliverable.
    let output = lato(&home, &workspace)
        .args(["-p", "--plan", "draft a plan for the workspace"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "no plan file produced must be an explicit failure; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no readable plan file was produced"),
        "{stderr}"
    );
    assert!(!workspace.path().join("plan.md").exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("plan file:"));
}

#[test]
fn headless_plan_exits_3_only_when_a_readable_plan_file_actually_exists() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // A real, readable, non-empty plan.md with the session still unapproved
    // (headless never auto-approves) is exactly the "produced but not
    // approved" state: exit code 3.
    std::fs::write(
        workspace.path().join("plan.md"),
        "# implementation plan\n\nstep one: real content\n",
    )
    .unwrap();
    let output = lato(&home, &workspace)
        .args(["-p", "--plan", "continue planning"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("plan file:"), "{stdout}");
    assert!(!stdout.contains("approved"));

    // An empty plan.md is not a produced plan: explicit failure.
    let workspace2 = tempfile::tempdir().unwrap();
    std::fs::write(workspace2.path().join("plan.md"), b"").unwrap();
    let output = lato(&home, &workspace2)
        .args(["-p", "--plan", "continue planning"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
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
