use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const SPAWN: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"spawn-1\",",
    "\"function\":{\"name\":\"spawn\",\"arguments\":",
    "\"{\\\"task_id\\\":\\\"phase5-child\\\",\\\"profile\\\":\\\"worker\\\",",
    "\\\"task\\\":\\\"inspect the fixture\\\",\\\"background\\\":false}\"}}]}}]}\n\n",
    "data: [DONE]\n\n",
);

const CHILD_RESULT: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":",
    "\"{\\\"summary\\\":\\\"fixture inspected\\\",\\\"changed_files\\\":[],",
    "\\\"tests\\\":[],\\\"artifacts\\\":[]}\"}}]}\n\n",
    "data: [DONE]\n\n",
);

const INSPECT: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"inspect-1\",",
    "\"function\":{\"name\":\"inspect\",",
    "\"arguments\":\"{\\\"task_id\\\":\\\"phase5-child\\\"}\"}}]}}]}\n\n",
    "data: [DONE]\n\n",
);

const WAIT: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"wait-1\",",
    "\"function\":{\"name\":\"wait\",",
    "\"arguments\":\"{\\\"task_id\\\":\\\"phase5-child\\\",",
    "\\\"timeout_ms\\\":3000}\"}}]}}]}\n\n",
    "data: [DONE]\n\n",
);

const COMPLETE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"phase5-smoke-ok\"}}]}\n\n",
    "data: [DONE]\n\n",
);

struct SmokeOutcome {
    output: Output,
    session_journal: PathBuf,
    worktree_root_is_empty: bool,
    _repo: tempfile::TempDir,
    _home: tempfile::TempDir,
}

#[test]
fn phase5_installed_command_smoke_exercises_real_task_chain() {
    let binary = std::env::var_os("LATO_SMOKE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_lato")));
    let outcome = run_scripted_task_smoke(&binary);
    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&outcome.output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&outcome.output.stdout).trim(),
        "phase5-smoke-ok"
    );
    assert!(outcome.session_journal.is_file());
    assert!(outcome.worktree_root_is_empty);
}

fn run_scripted_task_smoke(binary: &Path) -> SmokeOutcome {
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    initialize_repo(repo.path());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    fs::write(
        home.path().join("models.json"),
        format!(
            r#"{{"models":[{{"provider":"phase5-fixture","id":"task-smoke","api":"openai-completions","base_url":"http://{address}/v1","env":"PHASE5_FIXTURE_KEY"}}]}}"#
        ),
    )
    .unwrap();

    let server = thread::spawn(move || {
        let responses = [SPAWN, CHILD_RESULT, INSPECT, WAIT, COMPLETE];
        for (index, response) in responses.into_iter().enumerate() {
            let mut socket = accept_before(&listener, Instant::now() + Duration::from_secs(30));
            let request = read_request(&mut socket);
            assert_request(index, &request);
            write_response(&mut socket, response);
        }
        assert!(
            listener.accept().is_err(),
            "model received an unexpected request after terminal completion"
        );
    });

    let mut child = Command::new(binary)
        .current_dir(repo.path())
        .env("LATO_HOME", home.path())
        .env("PHASE5_FIXTURE_KEY", "offline-fixture-key")
        .args([
            "-p",
            "--model",
            "phase5-fixture/task-smoke",
            "run the phase 5 task smoke",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("lato command did not finish within 30 seconds");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    eprintln!(
        "phase5 smoke lato stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();

    let session_journal = find_session_journal(home.path());
    let worktrees = repo.path().join(".lato/worktrees");
    let worktree_root_is_empty = !worktrees.exists()
        || fs::read_dir(&worktrees)
            .unwrap()
            .next()
            .transpose()
            .unwrap()
            .is_none();
    SmokeOutcome {
        output,
        session_journal,
        worktree_root_is_empty,
        _repo: repo,
        _home: home,
    }
}

fn assert_request(index: usize, body: &Value) {
    let tools = body["tools"].as_array().expect("request tools");
    let names = tools
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(!names.contains(&"spawn_subagent"));
    if index != 1 {
        for required in ["spawn", "send", "wait", "cancel", "inspect"] {
            assert!(
                names.contains(&required),
                "parent request {index} omitted {required}: {names:?}"
            );
        }
    }

    let serialized = body.to_string();
    let context = |message: String| format!("{message}; request body: {serialized}");
    match index {
        0 => assert!(
            !serialized.contains("phase5-child"),
            "{}",
            context("request 0 leaked the child task id".into())
        ),
        1 => {
            assert!(
                serialized.contains("inspect the fixture"),
                "{}",
                context("request 1 lost the delegated task".into())
            );
            assert!(
                serialized.contains("changed_files"),
                "{}",
                context("request 1 lost the worker result contract".into())
            );
        }
        2 => assert!(
            serialized.contains("phase5-child"),
            "{}",
            context("request 2 lost the child task id".into())
        ),
        3 => assert!(
            serialized.contains("inspect-1"),
            "{}",
            context("request 3 lost the inspect tool call id".into())
        ),
        4 => assert!(
            serialized.contains("wait-1"),
            "{}",
            context("request 4 lost the wait tool call id".into())
        ),
        _ => unreachable!(),
    }
}

fn accept_before(listener: &TcpListener, deadline: Instant) -> TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Accepted sockets inherit the listener's non-blocking mode
                // on Windows; restore blocking mode so reads wait for data.
                stream
                    .set_nonblocking(false)
                    .expect("restore blocking socket");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0, "connection closed before HTTP headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        assert!(
            bytes.len() <= 128 * 1024,
            "HTTP headers exceeded fixture limit"
        );
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    assert!(headers.starts_with("post /v1/chat/completions "));
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .expect("content-length header")
        .trim()
        .parse::<usize>()
        .unwrap();
    assert!(content_length <= 2 * 1024 * 1024);
    while bytes.len() - header_end < content_length {
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0, "connection closed before HTTP body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap()
}

fn write_response(socket: &mut TcpStream, body: &str) {
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
    socket.flush().unwrap();
}

fn initialize_repo(root: &Path) {
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "user.email", "lato@example.invalid"]);
    run_git(root, &["config", "user.name", "Lato Test"]);
    fs::write(root.join("README.md"), "phase 5 smoke\n").unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-qm", "initial"]);
}

fn run_git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn find_session_journal(home: &Path) -> PathBuf {
    let sessions = home.join("sessions");
    let entries = fs::read_dir(&sessions).expect("session directory");
    let journals = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("events.jsonl"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(
        journals.len(),
        1,
        "expected exactly one persisted parent session"
    );
    for line in fs::read_to_string(&journals[0]).unwrap().lines() {
        serde_json::from_str::<Value>(line).expect("journal line must be JSON");
    }
    journals[0].clone()
}
