use serde_json::Value;
use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PROCESS_DEADLINE: Duration = Duration::from_secs(30);
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
const SOCKET_RETRY: Duration = Duration::from_millis(10);
const SOCKET_TIMEOUT: Duration = Duration::from_millis(100);
const FINAL_QUIET_PERIOD: Duration = Duration::from_millis(100);

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

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn wait_with_output(mut self, deadline: Instant) -> Result<Output, String> {
        loop {
            let child = self.child.as_mut().expect("child guard is armed");
            match child.try_wait() {
                Ok(Some(_)) => {
                    return self
                        .child
                        .take()
                        .expect("child guard is armed")
                        .wait_with_output()
                        .map_err(|error| format!("failed to collect lato output: {error}"));
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => return Err("lato command did not finish within 30 seconds".into()),
                Err(error) => return Err(format!("failed to poll lato command: {error}")),
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

struct ServerGuard {
    cancellation: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<(), String>>>,
}

impl ServerGuard {
    fn spawn(listener: TcpListener) -> Self {
        let cancellation = Arc::new(AtomicBool::new(false));
        let server_cancellation = Arc::clone(&cancellation);
        let handle = thread::spawn(move || run_server(listener, &server_cancellation));
        Self {
            cancellation,
            handle: Some(handle),
        }
    }

    fn join(mut self) -> Result<(), String> {
        self.handle
            .take()
            .expect("server guard is armed")
            .join()
            .map_err(|_| "model fixture server panicked".to_owned())?
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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

    let server = ServerGuard::spawn(listener);

    let child = Command::new(binary)
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
    let output = ChildGuard::new(child)
        .wait_with_output(Instant::now() + PROCESS_DEADLINE)
        .unwrap();
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

fn run_server(listener: TcpListener, cancellation: &AtomicBool) -> Result<(), String> {
    for (index, response) in [INVOKE_SKILL, COMPLETE].into_iter().enumerate() {
        let mut socket = accept_before(&listener, Instant::now() + PROCESS_DEADLINE, cancellation)?;
        let request = read_request(&mut socket, Instant::now() + REQUEST_DEADLINE, cancellation)?;
        assert_request(index, &request);
        write_response(
            &mut socket,
            response,
            Instant::now() + REQUEST_DEADLINE,
            cancellation,
        )?;
    }
    reject_unexpected_connection_before(
        &listener,
        Instant::now() + FINAL_QUIET_PERIOD,
        cancellation,
    )
}

fn accept_before(
    listener: &TcpListener,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<TcpStream, String> {
    let (stream, _) = retry_io(deadline, cancellation, "fixture accept", || {
        listener.accept()
    })?;
    // A stream accepted from a nonblocking listener may inherit nonblocking
    // mode on some platforms. Switch it before any request or response I/O.
    stream
        .set_nonblocking(false)
        .map_err(|error| format!("failed to make accepted fixture socket blocking: {error}"))?;
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("failed to set fixture read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|error| format!("failed to set fixture write timeout: {error}"))?;
    Ok(stream)
}

fn read_request(
    socket: &mut TcpStream,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<Value, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let count = retry_io(deadline, cancellation, "fixture request read", || {
            socket.read(&mut chunk)
        })?;
        if count == 0 {
            return Err("connection closed before HTTP headers".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if bytes.len() > 128 * 1024 {
            return Err("HTTP headers exceeded limit".into());
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    if !headers.starts_with("post /v1/chat/completions ") {
        return Err("fixture received an unexpected HTTP request target".into());
    }
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .ok_or_else(|| "fixture request omitted content-length".to_owned())?
        .trim()
        .parse::<usize>()
        .map_err(|error| format!("invalid fixture content-length: {error}"))?;
    if content_length > 2 * 1024 * 1024 {
        return Err("fixture request body exceeded limit".into());
    }
    while bytes.len() - header_end < content_length {
        let count = retry_io(deadline, cancellation, "fixture request read", || {
            socket.read(&mut chunk)
        })?;
        if count == 0 {
            return Err("connection closed before HTTP body".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .map_err(|error| format!("fixture request body was not valid JSON: {error}"))
}

fn retry_io<T>(
    deadline: Instant,
    cancellation: &AtomicBool,
    operation_name: &str,
    mut operation: impl FnMut() -> io::Result<T>,
) -> Result<T, String> {
    loop {
        if cancellation.load(Ordering::Acquire) {
            return Err("model fixture server cancelled".into());
        }
        if Instant::now() >= deadline {
            return Err(format!("{operation_name} deadline exceeded"));
        }
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(SOCKET_RETRY);
            }
            Err(error) => return Err(format!("{operation_name} failed: {error}")),
        }
    }
}

fn write_response(
    socket: &mut TcpStream,
    body: &str,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), String> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut written = 0;
    while written < response.len() {
        let count = retry_io(deadline, cancellation, "fixture response write", || {
            socket.write(&response.as_bytes()[written..])
        })?;
        if count == 0 {
            return Err("connection closed before HTTP response completed".into());
        }
        written += count;
    }
    retry_io(deadline, cancellation, "fixture response flush", || {
        socket.flush()
    })
}

fn reject_unexpected_connection_before(
    listener: &TcpListener,
    deadline: Instant,
    cancellation: &AtomicBool,
) -> Result<(), String> {
    loop {
        if cancellation.load(Ordering::Acquire) {
            return Err("model fixture server cancelled".into());
        }
        match listener.accept() {
            Ok(_) => return Err("model received an unexpected request after completion".into()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(SOCKET_RETRY);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(format!("fixture final accept failed: {error}")),
        }
    }
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
