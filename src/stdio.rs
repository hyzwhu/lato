use lato_agent::{AcpHost, default_fake_stream};
use lato_protocol::JsonRpcReq;
use lato_workspace::SessionTrust;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub async fn run() -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let trust = SessionTrust::for_interactive(&cwd, true);
    let (updates_tx, mut updates_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut host = AcpHost::new_with_home(
        cwd,
        trust,
        updates_tx,
        default_fake_stream(),
        crate::cli::lato_home(),
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => {
                eprintln!("error reading ACP stdin: {e}");
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: JsonRpcReq = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let response = serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}});
                write_json(&mut stdout, &response).await;
                continue;
            }
        };
        if let Some(response) = host.handle(request).await {
            write_json(&mut stdout, &response).await;
        }
        while let Ok(notification) = updates_rx.try_recv() {
            write_json(&mut stdout, &notification).await;
        }
    }
    0
}

async fn write_json(stdout: &mut tokio::io::Stdout, value: &serde_json::Value) {
    if let Ok(mut bytes) = serde_json::to_vec(value) {
        bytes.push(b'\n');
        let _ = stdout.write_all(&bytes).await;
        let _ = stdout.flush().await;
    }
}
