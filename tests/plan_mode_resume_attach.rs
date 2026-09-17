//! T7 — real `session/resume --plan` attach + draft load (A+ Stage 2, D2).
//!
//! The interactive `lato resume --plan` path speaks the ACP wire protocol:
//! `session/resume` (attach) → `lato/plan/enter` → `lato/plan/status`. These
//! tests drive a REAL resumable session across two real `lato acp` OS
//! processes and assert the attach → Plan Enter → existing-draft-hash chain,
//! not merely that the CLI pre-validation did not reject early.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

/// One `lato acp` OS process driven over stdio, rooted in `workspace`.
struct AcpProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl AcpProcess {
    fn spawn(home: &std::path::Path, workspace: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", home)
            .current_dir(workspace)
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).unwrap();
            assert!(
                read > 0,
                "acp process closed stdout before replying to {method}"
            );
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                && value["id"].as_i64() == Some(id)
            {
                return value;
            }
            // Notifications (session/update etc.) are skipped; only the
            // matching response resolves the call.
        }
    }

    fn exit(mut self) {
        drop(self.stdin); // closing stdin ends the acp process
        let status = self.child.wait().unwrap();
        assert!(status.success(), "acp process failed: {status}");
    }
}

/// Creates a real, resumable session (one turn of history) in its own acp
/// process rooted at `workspace`, then lets the process exit.
fn create_resumable_session(home: &std::path::Path, workspace: &std::path::Path) -> String {
    let mut process = AcpProcess::spawn(home, workspace);
    process.call("initialize", serde_json::json!({}));
    let new = process.call("session/new", serde_json::json!({}));
    assert!(new.get("error").is_none(), "session/new failed: {new}");
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
    let prompt = process.call(
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "one"}),
    );
    assert!(prompt.get("error").is_none(), "prompt failed: {prompt}");
    process.exit();
    sid
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

const DRAFT: &str = "# carried-over plan\n\nstep one: real content\n";

/// T7 branch 1 — a real resumed session WITHOUT `--plan`: the attach
/// succeeds, the session is usable, and Plan mode was never entered (the
/// draft on disk is not loaded into a plan activation).
#[test]
fn resumed_session_without_plan_stays_out_of_plan_mode() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("plan.md"), DRAFT).unwrap();
    let sid = create_resumable_session(home.path(), workspace.path());

    let mut process = AcpProcess::spawn(home.path(), workspace.path());
    process.call("initialize", serde_json::json!({}));
    // Real session/resume attach.
    let resume = process.call("session/resume", serde_json::json!({"sessionId": sid}));
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    // No plan flag: the status must be inactive and carry NO draft hash —
    // the draft on disk was not read into a plan activation.
    let status = process.call("lato/plan/status", serde_json::json!({"sessionId": sid}));
    let result = &status["result"];
    assert_eq!(result["phase"], "inactive", "{status}");
    assert!(
        result["lastDraftHash"].is_null(),
        "draft must not be loaded without --plan: {status}"
    );
    assert_eq!(result["draftPublished"], false, "{status}");
    // The session remains fully usable after the attach.
    let prompt = process.call(
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "two"}),
    );
    assert!(prompt.get("error").is_none(), "prompt failed: {prompt}");
    assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
    process.exit();
}

/// T7 branch 2 — a real resumed session WITH `--plan`: the exact chain the
/// TUI drives (`session/resume` → `lato/plan/enter` → `lato/plan/status`)
/// attaches the session, enters Drafting, and loads the existing draft hash
/// matching the on-disk bytes.
#[test]
fn resumed_session_with_plan_enters_drafting_and_loads_draft_hash() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("plan.md"), DRAFT).unwrap();
    let sid = create_resumable_session(home.path(), workspace.path());

    let mut process = AcpProcess::spawn(home.path(), workspace.path());
    process.call("initialize", serde_json::json!({}));
    let resume = process.call("session/resume", serde_json::json!({"sessionId": sid}));
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    // `resume --plan` re-enters Plan mode (BackendCommand::Plan(Enter)).
    let enter = process.call("lato/plan/enter", serde_json::json!({"sessionId": sid}));
    assert!(enter.get("error").is_none(), "plan enter failed: {enter}");
    let status = process.call("lato/plan/status", serde_json::json!({"sessionId": sid}));
    let result = &status["result"];
    assert_eq!(result["phase"], "drafting", "{status}");
    assert_eq!(
        result["lastDraftHash"],
        serde_json::json!(sha256_hex(DRAFT.as_bytes())),
        "the existing draft must be loaded as the starting hash: {status}"
    );
    // The draft file itself is untouched by the load.
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("plan.md")).unwrap(),
        DRAFT
    );
    process.exit();
}

/// T7 branch 2, fail-closed — `resume --plan` with an UNREADABLE (oversized)
/// draft refuses with exit 1 BEFORE any session work, and the original
/// session data stays intact and resumable afterwards.
#[test]
fn resume_plan_fail_closed_on_unreadable_draft_leaves_session_intact() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let sid = create_resumable_session(home.path(), workspace.path());

    // Oversized draft: 131,073 bytes.
    std::fs::write(workspace.path().join("plan.md"), vec![b'a'; 131_073]).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .current_dir(workspace.path())
        .args(["resume", &sid, "--plan"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "unreadable draft must fail closed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--plan draft"),
        "{:}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The original session was not touched by the refused resume: a fresh
    // process attaches and completes a turn.
    let mut process = AcpProcess::spawn(home.path(), workspace.path());
    process.call("initialize", serde_json::json!({}));
    let resume = process.call("session/resume", serde_json::json!({"sessionId": sid}));
    assert!(
        resume.get("error").is_none(),
        "session data must be intact after the refused resume: {resume}"
    );
    let prompt = process.call(
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "after"}),
    );
    assert!(prompt.get("error").is_none(), "prompt failed: {prompt}");
    assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
    process.exit();
}
