use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

/// One `lato acp` OS process driven over stdio. Dropping it closes stdin,
/// which ends the process: the owner is really gone afterwards.
struct AcpProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl AcpProcess {
    fn spawn(home: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", home)
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

fn initialize_params() -> serde_json::Value {
    serde_json::json!({})
}

/// One `session/new -> process exit -> session/resume -> next turn` cycle
/// across two real `lato acp` OS processes.
fn cross_process_resume_cycle() {
    let home = tempfile::tempdir().unwrap();

    // Process A: create the session and run one turn, then exit.
    let sid = {
        let mut process = AcpProcess::spawn(home.path());
        process.call("initialize", initialize_params());
        let new = process.call("session/new", serde_json::json!({}));
        assert!(new.get("error").is_none(), "session/new failed: {new}");
        let sid = new["result"]["sessionId"].as_str().unwrap().to_string();
        let prompt = process.call(
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": "one"}),
        );
        assert!(
            prompt.get("error").is_none(),
            "process A prompt failed: {prompt}"
        );
        process.exit();
        sid
    };

    // Process B: a real OS-process restart; resume and continue.
    let mut process = AcpProcess::spawn(home.path());
    process.call("initialize", initialize_params());
    let resume = process.call("session/resume", serde_json::json!({"sessionId": sid}));
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    let prompt = process.call(
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "two"}),
    );
    assert!(
        prompt.get("error").is_none(),
        "process B prompt failed: {prompt}"
    );
    assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
}

#[test]
fn cross_process_resume_survives_ten_consecutive_restarts() {
    for _ in 0..10 {
        cross_process_resume_cycle();
    }
}

#[test]
fn cross_process_resume_works_for_a_never_prompted_session() {
    let home = tempfile::tempdir().unwrap();

    let sid = {
        let mut process = AcpProcess::spawn(home.path());
        process.call("initialize", initialize_params());
        let new = process.call("session/new", serde_json::json!({}));
        assert!(new.get("error").is_none(), "session/new failed: {new}");
        new["result"]["sessionId"].as_str().unwrap().to_string()
        // process exits without any prompt: the journal holds only the
        // startup records
    };

    let mut process = AcpProcess::spawn(home.path());
    process.call("initialize", initialize_params());
    let resume = process.call("session/resume", serde_json::json!({"sessionId": sid}));
    assert!(resume.get("error").is_none(), "resume failed: {resume}");
    let prompt = process.call(
        "session/prompt",
        serde_json::json!({"sessionId": sid, "text": "first"}),
    );
    assert!(prompt.get("error").is_none(), "prompt failed: {prompt}");
    assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
}

#[test]
fn cross_process_resume_supports_repeated_resumes() {
    let home = tempfile::tempdir().unwrap();

    let sid = {
        let mut process = AcpProcess::spawn(home.path());
        process.call("initialize", initialize_params());
        let new = process.call("session/new", serde_json::json!({}));
        assert!(new.get("error").is_none(), "session/new failed: {new}");
        new["result"]["sessionId"].as_str().unwrap().to_string()
    };

    for turn in 0..2 {
        let mut process = AcpProcess::spawn(home.path());
        process.call("initialize", initialize_params());
        let resume = process.call("session/resume", serde_json::json!({"sessionId": sid}));
        assert!(
            resume.get("error").is_none(),
            "resume {turn} failed: {resume}"
        );
        let prompt = process.call(
            "session/prompt",
            serde_json::json!({"sessionId": sid, "text": format!("turn-{turn}")}),
        );
        assert!(
            prompt.get("error").is_none(),
            "prompt {turn} failed: {prompt}"
        );
        assert_eq!(prompt["result"]["status"], "complete", "{prompt}");
    }
}
