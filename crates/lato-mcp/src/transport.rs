use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn call_stdio(
    program: &Path,
    args: &[String],
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let request = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    let mut stdin = child.stdin.take().ok_or("MCP stdin unavailable")?;
    let mut bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await.map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("MCP stdout unavailable")?;
    let mut lines = BufReader::new(stdout).lines();
    let response = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .map_err(|_| "MCP stdio timeout".to_string())?
        .map_err(|e| e.to_string())?
        .ok_or("MCP server closed without response")?;
    let value: Value = serde_json::from_str(&response).map_err(|e| e.to_string())?;
    if let Some(error) = value.get("error") {
        Err(format!("MCP error: {error}"))
    } else {
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

pub async fn call_streamable_http(
    endpoint: &str,
    method: &str,
    params: Value,
    headers: &[(String, String)],
) -> Result<Value, String> {
    let url = url::Url::parse(endpoint).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("MCP HTTP endpoint must be http(s)".into());
    }
    let client = reqwest::Client::new();
    let mut request = client
        .post(url)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}));
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let value: Value = response.json().await.map_err(|e| e.to_string())?;
    if let Some(error) = value.get("error") {
        Err(format!("MCP error: {error}"))
    } else {
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[tokio::test]
    async fn e5_1_mcp_stdio_transport_calls_json_rpc_server_fixture() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("server.sh");
        std::fs::write(&script, "#!/bin/sh\nread line\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = call_stdio(&script, &[], "tools/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
    }
}
