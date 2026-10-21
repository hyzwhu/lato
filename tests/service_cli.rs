use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
    time::timeout,
};

const TOKEN: &str = "service-cli-test-token-at-least-32-characters";

#[test]
fn service_requires_an_explicit_model_loopback_and_a_valid_credential() {
    for args in [
        vec!["serve"],
        vec!["serve", "--model", "local/test", "--listen", "0.0.0.0:8765"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_lato"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    for token in [None, Some("too-short")] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_lato"));
        command.args(["serve", "--model", "not-a-real-provider/model"]);
        if let Some(token) = token {
            command.env("LATO_SERVICE_TOKEN", token);
        } else {
            command.env_remove("LATO_SERVICE_TOKEN");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("service.invalid_token"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("too-short"));
    }
}

#[tokio::test]
async fn real_cli_service_uses_the_selected_http_provider_and_shuts_down() {
    let home = tempfile::tempdir().unwrap();
    let model_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_address = model_listener.local_addr().unwrap();
    std::fs::write(
        home.path().join("models.json"),
        serde_json::to_vec(&json!({"models":[{
            "provider":"service-test","id":"business","api":"openai-completions",
            "base_url":format!("http://{model_address}/v1"),"env":"LATO_TEST_MODEL_KEY"
        }]}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.path().join("business-agent.json"),
        r#"{"mode":"general","instructions":"CLI_SERVICE_BUSINESS_INSTRUCTIONS","tools":[]}"#,
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    let provider = tokio::spawn(async move {
        let (mut socket, _) = model_listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(bytes.len() < 64 * 1024);
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        assert!(
            headers.starts_with("POST /v1/chat/completions "),
            "{headers}"
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-model-key")
        );
        let length = headers
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            })
            .unwrap();
        while bytes.len() < header_end + length {
            let n = socket.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&chunk[..n]);
        }
        seen.lock()
            .unwrap()
            .push(serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap());
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Real provider transport reached.\"}}]}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_lato"))
        .args([
            "serve",
            "--model",
            "service-test/business",
            "--listen",
            "127.0.0.1:0",
        ])
        .arg("--agent-config")
        .arg(home.path().join("business-agent.json"))
        .env("LATO_HOME", home.path())
        .env("LATO_SERVICE_TOKEN", TOKEN)
        .env("LATO_TEST_MODEL_KEY", "fixture-model-key")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    let base = timeout(Duration::from_secs(10), async {
        while let Some(line) = stderr.next_line().await.unwrap() {
            assert!(!line.contains(TOKEN));
            if let Some(address) = line
                .strip_prefix("Lato service listening on ")
                .and_then(|s| s.split(';').next())
            {
                return address.to_owned();
            }
        }
        panic!("service did not announce a listener");
    })
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let session: Value = client
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(TOKEN)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = session["sessionId"].as_str().unwrap();
    let response = client
        .post(format!("{base}/v1/sessions/{id}/operations"))
        .bearer_auth(TOKEN)
        .json(&json!({"text":"Answer this business request"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let operation = response.json::<Value>().await.unwrap()["operationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = timeout(Duration::from_secs(10), async {
        loop {
            let result: Value = client
                .get(format!("{base}/v1/operations/{operation}"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if result["status"] == "completed" || result["status"] == "failed" {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["result"]["text"], "Real provider transport reached.");
    timeout(Duration::from_secs(3), provider)
        .await
        .unwrap()
        .unwrap();
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("CLI_SERVICE_BUSINESS_INSTRUCTIONS")
        );
        assert!(
            requests[0]
                .get("tools")
                .is_none_or(|tools| tools == &json!([]))
        );
    }
    #[cfg(unix)]
    {
        let status = std::process::Command::new("/bin/kill")
            .args(["-TERM", &child.id().unwrap().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            timeout(Duration::from_secs(12), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    #[cfg(not(unix))]
    child.kill().await.unwrap();
}
