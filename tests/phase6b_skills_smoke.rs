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

const INVOKE_SKILL: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"skill-1\",",
    "\"function\":{\"name\":\"skill\",\"arguments\":",
    "\"{\\\"skill\\\":\\\"smoke-plugin:inspect\\\",",
    "\\\"args\\\":\\\"alpha beta\\\"}\"}}]}}]}\n\n",
    "data: [DONE]\n\n",
);

const COMPLETE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":",
    "\"phase6b-skills-smoke-ok\"}}]}\n\n",
    "data: [DONE]\n\n",
);

struct SmokeOutcome {
    output: Output,
    session_journal: PathBuf,
    _root: tempfile::TempDir,
}

#[test]
fn phase6b_installed_command_smoke_exercises_skill_invocation() {
    let binary = std::env::var_os("LATO_SMOKE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_lato")));
    let outcome = run_scripted_skill_smoke(&binary);
    assert!(
        outcome.output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&outcome.output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&outcome.output.stdout).trim(),
        "phase6b-skills-smoke-ok"
    );
    assert!(outcome.session_journal.is_file());

    let audit_records = fs::read_to_string(&outcome.session_journal)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record.pointer("/record/type").and_then(Value::as_str) == Some("extension_audit")
        })
        .collect::<Vec<_>>();
    assert_eq!(audit_records.len(), 2);
    let audit_json = serde_json::to_string(&audit_records).unwrap();
    assert!(audit_json.contains("skill_catalog_materialized"));
    assert!(audit_json.contains("skill_invoked"));
    assert!(!audit_json.contains("alpha beta"));
    assert!(!audit_json.contains("First=alpha"));
}

fn run_scripted_skill_smoke(binary: &Path) -> SmokeOutcome {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let plugin = root.path().join("smoke-plugin");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(plugin.join("skills/inspect")).unwrap();
    initialize_repo(&workspace);
    fs::write(
        plugin.join("plugin.json"),
        r#"{"name":"smoke-plugin","skills":"skills"}"#,
    )
    .unwrap();
    fs::write(
        plugin.join("skills/inspect/SKILL.md"),
        concat!(
            "---\n",
            "name: inspect\n",
            "description: Inspect two bounded smoke arguments.\n",
            "allowed-tools:\n",
            "  - read_file\n",
            "---\n",
            "First=$0 Second=$ARGUMENTS[1] All=$ARGUMENTS.\n",
        ),
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    fs::write(
        home.join("models.json"),
        format!(
            r#"{{"models":[{{"provider":"phase6b-fixture","id":"skills-smoke","api":"openai-completions","base_url":"http://{address}/v1","env":"PHASE6B_FIXTURE_KEY"}}]}}"#
        ),
    )
    .unwrap();

    let server = thread::spawn(move || {
        for (index, response) in [INVOKE_SKILL, COMPLETE].into_iter().enumerate() {
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
        .current_dir(&workspace)
        .env("LATO_HOME", &home)
        .env("PHASE6B_FIXTURE_KEY", "offline-fixture-key")
        .args([
            "-p",
            "--model",
            "phase6b-fixture/skills-smoke",
            "--plugin-dir",
        ])
        .arg(&plugin)
        .arg("run the phase 6b skills smoke")
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
    server.join().unwrap();

    SmokeOutcome {
        output,
        session_journal: find_session_journal(&home),
        _root: root,
    }
}

fn assert_request(index: usize, body: &Value) {
    let tools = body["tools"].as_array().expect("request tools");
    let names = tools
        .iter()
        .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let serialized = body.to_string();
    match index {
        0 => {
            assert!(names.contains(&"skill"));
            assert!(serialized.contains("smoke-plugin:inspect"));
            assert!(serialized.contains("Inspect two bounded smoke arguments."));
            assert!(!serialized.contains("First=alpha"));
        }
        1 => {
            assert_eq!(names, vec!["read_file"]);
            assert!(serialized.contains("<skill name=\\\"smoke-plugin:inspect\\\""));
            assert!(serialized.contains("First=alpha Second=beta All=alpha beta."));
        }
        _ => unreachable!(),
    }
}

fn accept_before(listener: &TcpListener, deadline: Instant) -> TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
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
        assert!(bytes.len() <= 128 * 1024, "HTTP headers exceeded limit");
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
    fs::write(root.join("README.md"), "phase 6b skill smoke\n").unwrap();
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
    let journals = fs::read_dir(home.join("sessions"))
        .expect("session directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("events.jsonl"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(journals.len(), 1, "expected one persisted session");
    journals[0].clone()
}
