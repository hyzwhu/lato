use lato_ai::{CustomModel, ModelApi, ProviderModelsEntry, ProviderModelsStore};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn tool_calls(round: usize) -> Vec<(&'static str, &'static str, Value)> {
    if round == 0 {
        vec![
            (
                "write-1",
                "write_file",
                json!({"path":"result.txt","contents":"before你好"}),
            ),
            (
                "edit-1",
                "search_replace",
                json!({"path":"result.txt","old":"before","new":"after"}),
            ),
        ]
    } else {
        vec![("read-1", "read_file", json!({"path":"result.txt"}))]
    }
}

fn response(api: ModelApi, round: usize) -> (&'static str, String) {
    let text = if round == 2 { "verified" } else { "checking" };
    let calls = if round == 2 {
        vec![]
    } else {
        tool_calls(round)
    };
    match api {
        ModelApi::OpenaiCompletions => {
            let mut body = event(json!({"choices":[{"delta":{"content":text}}]}));
            for (index, (id, name, arguments)) in calls.iter().enumerate() {
                let raw = arguments.to_string();
                body += &event(
                    json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":id,"function":{"name":name,"arguments":""}}]}}]}),
                );
                body += &event(
                    json!({"choices":[{"delta":{"tool_calls":[{"index":index,"function":{"arguments":raw}}]}}]}),
                );
            }
            body += "data: [DONE]\n\n";
            ("text/event-stream", body)
        }
        ModelApi::AnthropicMessages => {
            let mut content = vec![json!({"type":"text","text":text})];
            content.extend(calls.into_iter().map(
                |(id, name, args)| json!({"type":"tool_use","id":id,"name":name,"input":args}),
            ));
            ("application/json", serde_json::to_string_pretty(&json!({"type":"message","role":"assistant","content":content,"stop_reason":if round == 2 {"end_turn"} else {"tool_use"}})).unwrap())
        }
        _ => {
            let mut output = vec![
                json!({"type":"message","id":format!("msg-{round}"),"role":"assistant","content":[{"type":"output_text","text":text}]}),
            ];
            output.extend(calls.into_iter().map(|(id, name, args)| json!({"type":"function_call","id":format!("item-{id}"),"call_id":id,"name":name,"arguments":args.to_string()})));
            let mut body = String::new();
            // A done-only first round and terminal-output-only follow-up exercise
            // both server event shapes, including duplicate terminal snapshots.
            if round == 0 {
                for item in &output {
                    body += &event(json!({"type":"response.output_item.done","item":item}));
                }
            }
            body += &event(json!({"type":"response.completed","response":{"output":output}}));
            ("text/event-stream", body)
        }
    }
}

fn verify_history(api: ModelApi, round: usize, request: &Value) {
    let text = request.to_string();
    assert!(text.contains("checking"), "assistant text lost: {request}");
    let expected = if round == 1 {
        vec!["write-1", "edit-1"]
    } else {
        vec!["write-1", "edit-1", "read-1"]
    };
    let results: Vec<(String, String)> = match api {
        ModelApi::OpenaiCompletions => request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| {
                (
                    m["tool_call_id"].as_str().unwrap().into(),
                    m["content"].as_str().unwrap().into(),
                )
            })
            .collect(),
        ModelApi::AnthropicMessages => request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|m| m["type"] == "tool_result")
            .map(|m| {
                (
                    m["tool_use_id"].as_str().unwrap().into(),
                    m["content"].as_str().unwrap().into(),
                )
            })
            .collect(),
        _ => request["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["type"] == "function_call_output")
            .map(|m| {
                (
                    m["call_id"].as_str().unwrap().into(),
                    m["output"].as_str().unwrap().into(),
                )
            })
            .collect(),
    };
    assert_eq!(
        results
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        results.iter().all(|(_, output)| !output.contains("ERROR")),
        "tool failed or executed twice: {results:?}"
    );
    if round == 2 {
        assert_eq!(results.last().unwrap().1, "after你好");
    }
}

async fn cli_tool_chain(api: ModelApi) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut held_connections = Vec::new();
        for round in 0..3 {
            loop {
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
                if headers.starts_with("GET ") {
                    socket.get_mut().write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    continue;
                }
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).await.unwrap();
                if headers
                    .to_ascii_lowercase()
                    .contains("content-encoding: zstd")
                {
                    bytes = zstd::stream::decode_all(std::io::Cursor::new(bytes)).unwrap();
                }
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                if round > 0 {
                    verify_history(api, round, &request);
                }
                let (content_type, body) = response(api, round);
                if content_type == "text/event-stream" {
                    // Intentionally leave the chunked response open after completion.
                    socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n", body.len()).as_bytes()).await.unwrap();
                    held_connections.push(socket);
                } else {
                    socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                break;
            }
        }
    });
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let codex = api == ModelApi::OpenaiCodexResponses;
    let provider = if codex {
        "openai-codex"
    } else {
        "repair-fixture"
    };
    if codex {
        lato_ai::store_oauth(
            &mut lato_ai::CredentialStore::open(home.path()).unwrap(),
            provider,
            "fixture-token",
            "fixture-refresh",
            i64::MAX,
            Some("fixture-account"),
        )
        .unwrap();
    }
    ProviderModelsStore::open(home.path())
        .write(
            provider,
            ProviderModelsEntry {
                models: vec![CustomModel {
                    provider: provider.into(),
                    id: "repair".into(),
                    api,
                    base_url: format!("http://{address}/v1"),
                    env: "REPAIR_FIXTURE_KEY".into(),
                    context_window: None,
                    model_family: None,
                }],
                ..Default::default()
            },
        )
        .unwrap();
    if !codex {
        let models = ProviderModelsStore::open(home.path())
            .read(provider)
            .unwrap()
            .unwrap()
            .models;
        std::fs::write(
            home.path().join("models.json"),
            serde_json::to_vec(&models).unwrap(),
        )
        .unwrap();
    }
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(
            std::env::var("LATO_TEST_BINARY").unwrap_or_else(|_| env!("CARGO_BIN_EXE_lato").into()),
        )
        .current_dir(workspace.path())
        .env("LATO_HOME", home.path())
        .env("REPAIR_FIXTURE_KEY", "fixture-key")
        .args([
            "-p",
            "--model",
            &format!("{provider}/repair"),
            "请在当前目录创建 result.txt，修改内容，然后读取文件确认。",
        ])
        .kill_on_drop(true)
        .output(),
    )
    .await
    .expect("tool loop timed out")
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.await.unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "after你好"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).ends_with("verified\n"),
        "{:?}",
        output
    );
}

#[tokio::test]
async fn chat_completions_tool_chain() {
    cli_tool_chain(ModelApi::OpenaiCompletions).await;
}
#[tokio::test]
async fn responses_tool_chain() {
    cli_tool_chain(ModelApi::OpenaiResponses).await;
}
#[tokio::test]
async fn anthropic_json_tool_chain() {
    cli_tool_chain(ModelApi::AnthropicMessages).await;
}
#[tokio::test]
async fn codex_sse_tool_chain() {
    cli_tool_chain(ModelApi::OpenaiCodexResponses).await;
}
