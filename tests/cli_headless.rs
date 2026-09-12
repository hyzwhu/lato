use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn public_beta_help_lists_session_commands_and_headless_compatibility() {
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for required in ["sessions", "resume", "doctor", "login", "acp", "-p"] {
        assert!(help.contains(required), "missing {required}: {help}");
    }
}

#[test]
fn public_beta_version_matches_cargo_package() {
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        concat!("lato ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn sessions_render_empty_human_and_versioned_json() {
    let home = tempfile::tempdir().unwrap();
    let human = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .arg("sessions")
        .output()
        .unwrap();
    assert!(human.status.success());
    assert_eq!(
        String::from_utf8_lossy(&human.stdout).trim(),
        "No saved sessions."
    );

    let json = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success());
    assert!(json.stderr.is_empty());
    let body: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(body["schema_version"], 2);
    assert_eq!(body["sessions"], serde_json::json!([]));
}

#[test]
fn sessions_are_listed_newest_first() {
    let home = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let prompt = Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", home.path())
            .args(["-p", "reply with hi only"])
            .output()
            .unwrap();
        assert!(prompt.status.success());
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions[0]["sessionId"].as_str().unwrap() > sessions[1]["sessionId"].as_str().unwrap()
    );
}

#[test]
fn resume_requires_a_tty() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["resume", "s1700000000000-1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("resume requires a tty"));
    assert!(!home.path().join("sessions").exists());
}

#[test]
fn a1_1_stdio_acp_cli_initializes_and_rejects_session_load() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lato"))
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/load","params":{{}}}}"#
    )
    .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["result"]["protocolVersion"], 1);
    assert_eq!(lines[1]["error"]["code"], -32601);
}

#[test]
fn discovered_provider_model_cache_runs_with_persisted_provider_credential() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0u8; 32 * 1024];
        let count = socket.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.contains("discovered-model"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer saved-secret")
        );
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"dynamic-ok\"}}]}\n\ndata: [DONE]\n\n";
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("model-cache.json"), format!(
        r#"{{"models":[{{"provider":"minimax-cn","id":"discovered-model","api":"openai-completions","base_url":"http://{address}/v1","env":"MINIMAX_API_KEY"}}]}}"#
    )).unwrap();
    let login = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["login", "minimax-cn", "--api-key", "saved-secret"])
        .output()
        .unwrap();
    assert!(login.status.success());
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["-p", "--model", "minimax-cn/discovered-model", "hello"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "dynamic-ok");
}

#[tokio::test]
async fn discovered_codex_model_runs_with_saved_oauth_and_codex_transport() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for method in ["GET", "POST"] {
            let (socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut socket = BufReader::new(socket);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(headers.starts_with(&format!("{method} /backend-api/codex/responses ")));
            let lower = headers.to_ascii_lowercase();
            assert!(lower.contains("authorization: bearer saved-codex-token\r\n"));
            assert!(lower.contains("chatgpt-account-id: saved-account\r\n"));
            if method == "GET" {
                socket.get_mut().write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            } else {
                let length: usize = lower
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                socket.read_exact(&mut vec![0; length]).await.unwrap();
                let body = concat!(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"codex-dynamic-ok\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
                );
                socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        }
    });
    let home = tempfile::tempdir().unwrap();
    let mut credentials = lato_ai::CredentialStore::open(home.path()).unwrap();
    lato_ai::store_oauth(
        &mut credentials,
        "openai-codex",
        "saved-codex-token",
        "saved-refresh",
        i64::MAX,
        Some("saved-account"),
    )
    .unwrap();
    // Discovery must also work when another provider already populated the cache.
    lato_ai::ProviderModelsStore::open(home.path())
        .write("other-provider", lato_ai::ProviderModelsEntry::default())
        .unwrap();
    lato_ai::ProviderModelsStore::open(home.path())
        .write(
            "openai-codex",
            lato_ai::ProviderModelsEntry {
                models: vec![lato_ai::CustomModel {
                    provider: "openai-codex".into(),
                    id: "discovered-codex".into(),
                    api: lato_ai::ModelApi::OpenaiCodexResponses,
                    base_url: format!("http://{address}/backend-api"),
                    env: "LATO_API_KEY".into(),
                    context_window: None,
                    model_family: Some("openai".into()),
                }],
                ..Default::default()
            },
        )
        .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", home.path())
            .args(["-p", "--model", "openai-codex/discovered-codex", "hello"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "codex-dynamic-ok"
    );
    server.await.unwrap();
}

#[test]
fn e4_1_cli_custom_model_http_sse_end_to_end() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(value) => break value,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        };
        let mut request = [0u8; 32 * 1024];
        let count = socket.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.starts_with("POST /v1/chat/completions"));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer local-secret")
        );
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"custom-ok\"}}]}\n\ndata: [DONE]\n\n";
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("models.json"), format!(
        r#"{{"models":[{{"provider":"local","id":"qwen","api":"openai-completions","base_url":"http://{address}/v1","env":"LOCAL_KEY"}}]}}"#
    )).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .env("LOCAL_KEY", "local-secret")
        .args(["-p", "--model", "local/qwen", "say custom-ok"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "custom-ok");
}

#[test]
fn b1_6_headless_http_model_tool_loop_edits_workspace_offline_fixture() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 64 * 1024];
            let count = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.contains("search_replace"));
            let body = if turn == 0 {
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"edit-1\",\"function\":{\"name\":\"search_replace\",\"arguments\":\"{\\\"path\\\":\\\"code.txt\\\",\\\"old\\\":\\\"broken\\\",\\\"new\\\":\\\"fixed\\\"}\"}}]}}]}\n\ndata: [DONE]\n\n"
            } else {
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n"
            };
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    let d = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("code.txt"), "broken").unwrap();
    std::fs::write(home.path().join("models.json"), format!(
        r#"{{"models":[{{"provider":"fixture","id":"coder","api":"openai-completions","base_url":"http://{address}/v1","env":"FIXTURE_KEY"}}]}}"#
    )).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(d.path())
        .env("LATO_HOME", home.path())
        .env("FIXTURE_KEY", "key")
        .args(["-p", "--model", "fixture/coder", "fix code.txt"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(d.path().join("code.txt")).unwrap(),
        "fixed"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");
}

fn write_http_json(socket: &mut std::net::TcpStream, body: &str) {
    use std::io::Write;
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

#[test]
fn headless_sensenova_think_only_write_task_exits_nonzero_without_artifact() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 64 * 1024];
            let _ = socket.read(&mut request).unwrap();
            write_http_json(
                &mut socket,
                r#"{"choices":[{"message":{"reasoning_content":"I will create hello.txt","content":"</think>"},"finish_reason":"stop"}]}"#,
            );
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("models.json"),
        format!(
            r#"{{"models":[{{"provider":"sensenova","id":"glm-5.2","api":"openai-completions","base_url":"http://{address}/v1","env":"SENSENOVA_API_KEY"}}]}}"#
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(workspace.path())
        .env("LATO_HOME", home.path())
        .env("SENSENOVA_API_KEY", "key")
        .args([
            "-p",
            "--model",
            "sensenova/glm-5.2",
            "请在当前目录创建 hello.txt，内容是 Hello, world!",
        ])
        .output()
        .unwrap();
    let _ = server.join();
    assert_ne!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("workspace tool was required"),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!workspace.path().join("hello.txt").exists());
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("think"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn headless_sensenova_reasoning_xml_tool_call_writes_artifact() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 64 * 1024];
            let _ = socket.read(&mut request).unwrap();
            let body = if turn == 0 {
                r#"{"choices":[{"message":{"reasoning_content":"<tool_call>write_file<arg_key>path</arg_key><arg_value>hello.txt</arg_value><arg_key>contents</arg_key><arg_value>Hello, world!</arg_value></tool_call>","content":"</think>"},"finish_reason":"stop"}]}"#
            } else {
                r#"{"choices":[{"message":{"content":"已创建 hello.txt"},"finish_reason":"stop"}]}"#
            };
            write_http_json(&mut socket, body);
        }
    });
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("models.json"),
        format!(
            r#"{{"models":[{{"provider":"sensenova","id":"glm-5.2","api":"openai-completions","base_url":"http://{address}/v1","env":"SENSENOVA_API_KEY"}}]}}"#
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(workspace.path())
        .env("LATO_HOME", home.path())
        .env("SENSENOVA_API_KEY", "key")
        .args([
            "-p",
            "--model",
            "sensenova/glm-5.2",
            "请在当前目录创建 hello.txt，内容是 Hello, world!",
        ])
        .output()
        .unwrap();
    let _ = server.join();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("hello.txt")).unwrap(),
        "Hello, world!"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("已创建 hello.txt"), "stdout={stdout}");
    assert!(!stdout.contains("think"), "stdout={stdout}");
}

#[test]
fn headless_answers_current_workspace_without_model_or_tool_call() {
    let d = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(d.path())
        .env("LATO_HOME", home.path())
        .args(["-p", "给我一下当前文件夹路径"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("当前工作目录:"), "stdout={stdout}");
    assert!(
        stdout.contains(&d.path().display().to_string()),
        "stdout={stdout}"
    );
}

#[test]
fn headless_answers_parent_workspace_directory_without_model_or_tool_call() {
    let d = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(d.path())
        .env("LATO_HOME", home.path())
        .args(["-p", "上一层目录是什么"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("上一层目录:"), "stdout={stdout}");
    assert!(
        stdout.contains(&d.path().parent().unwrap().display().to_string()),
        "stdout={stdout}"
    );
}

#[test]
fn headless_answers_grandparent_workspace_directory_without_model_or_tool_call() {
    let d = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(d.path())
        .env("LATO_HOME", home.path())
        .args(["-p", "上上层目录是什么"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("上上层目录:"), "stdout={stdout}");
    assert!(
        stdout.contains(
            &d.path()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .display()
                .to_string()
        ),
        "stdout={stdout}"
    );
}

#[test]
fn headless_answers_pwd_without_model_or_tool_call() {
    let d = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .current_dir(d.path())
        .env("LATO_HOME", home.path())
        .args(["-p", "pwd"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("当前工作目录:"), "stdout={stdout}");
    assert!(
        stdout.contains(&d.path().display().to_string()),
        "stdout={stdout}"
    );
}

#[test]
fn headless_answers_current_model_without_validating_provider() {
    let d = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["-p", "--model", "fixture/coder", "你是什么模型"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "当前模型: fixture/coder"
    );
}

#[test]
fn general_model_question_is_not_stolen_by_local_fact_shortcut() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["-p", "which model architecture should I use?"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
}

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
fn interactive_mode_requires_a_tty_when_no_arguments_are_given() {
    let output = Command::new(env!("CARGO_BIN_EXE_lato")).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive mode requires a tty"));
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
fn china_providers_accept_api_key_login() {
    for provider in ["minimax-cn", "zai", "zai-coding-cn", "sensenova"] {
        let d = tempfile::tempdir().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", d.path())
            .args(["login", provider, "--api-key", "secret"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{provider}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let store = std::fs::read_to_string(d.path().join("auth.json")).unwrap();
        assert!(store.contains(provider));
    }
}

#[test]
fn api_key_login_can_replace_an_existing_credential() {
    let d = tempfile::tempdir().unwrap();
    for key in ["old-secret", "new-secret"] {
        let output = Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", d.path())
            .args(["login", "minimax-cn", "--api-key", key])
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    let store = std::fs::read_to_string(d.path().join("auth.json")).unwrap();
    assert!(store.contains("new-secret"));
    assert!(!store.contains("old-secret"));
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
fn b1_5_cli_rejects_bedrock_vertex_cloudflare_api_key() {
    for provider in ["amazon-bedrock", "google-vertex", "cloudflare-workers-ai"] {
        let d = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_lato"))
            .env("LATO_HOME", d.path())
            .args(["login", provider, "--api-key", "secret"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{provider} should be rejected");
        assert!(!d.path().join("auth.json").exists());
    }
}

#[test]
fn c1_1_cli_mock_kimi_oauth_writes_store() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .env("LATO_MOCK_OAUTH", "1")
        .args(["login", "kimi-coding", "--oauth"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(d.path().join("auth.json")).unwrap();
    assert!(text.contains("kimi-coding"));
    assert!(text.contains("oauth"));
}

#[test]
fn c1_2_cli_mock_openai_codex_oauth_writes_separate_id() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .env("LATO_MOCK_OAUTH", "1")
        .args(["login", "openai-codex", "--oauth"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(d.path().join("auth.json")).unwrap();
    assert!(text.contains("openai-codex"));
    assert!(text.contains("\"account_id\": \"mock-account\""));
    assert!(!text.contains("\"openai\""));
}

#[test]
fn cli_mock_openai_codex_device_auth_writes_account_identity() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .env("LATO_MOCK_OAUTH", "1")
        .args(["login", "openai-codex", "--oauth", "--device-auth"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(d.path().join("auth.json")).unwrap()).unwrap();
    assert_eq!(value["openai-codex"]["account_id"], "mock-account");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("mock-access"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("mock-refresh"));
}

#[test]
fn c1_3_cli_rejects_openai_codex_api_key() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", d.path())
        .args(["login", "openai-codex", "--api-key", "secret"])
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
