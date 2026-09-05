use super::{CodexRequest, CodexTransportError, TransportOutcome, events::CodexEventMapper};
use crate::StreamPiece;
use futures_util::StreamExt;
use std::io::Cursor;
use tokio::sync::mpsc;

pub struct EncodedBody {
    pub bytes: Vec<u8>,
    pub content_encoding: Option<&'static str>,
}

pub fn encode_sse_body(body: &serde_json::Value) -> Result<EncodedBody, String> {
    let json = serde_json::to_vec(body).map_err(|error| error.to_string())?;
    match zstd::stream::encode_all(Cursor::new(&json), 3) {
        Ok(bytes) => Ok(EncodedBody {
            bytes,
            content_encoding: Some("zstd"),
        }),
        Err(_) => Ok(EncodedBody {
            bytes: json,
            content_encoding: None,
        }),
    }
}

pub async fn stream_sse(
    client: &reqwest::Client,
    request: &CodexRequest,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<TransportOutcome, CodexTransportError> {
    let encoded = encode_sse_body(&request.body).map_err(CodexTransportError::before_stream)?;
    let mut builder = client.post(&request.url);
    for (key, value) in &request.headers {
        builder = builder.header(key, value);
    }
    if let Some(encoding) = encoded.content_encoding {
        builder = builder.header("content-encoding", encoding);
    }
    let response = builder
        .body(encoded.bytes)
        .send()
        .await
        .map_err(|error| CodexTransportError::before_stream(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(CodexTransportError::before_stream(format!(
            "http {status}: {body}"
        )));
    }

    let mut mapper = CodexEventMapper::default();
    let mut bytes = response.bytes_stream();
    let mut buffered = Vec::new();
    let mut event_data = Vec::<String>::new();
    'read: while let Some(chunk) = bytes.next().await {
        let chunk =
            chunk.map_err(|error| CodexTransportError::with_mapper(error.to_string(), &mapper))?;
        buffered.extend_from_slice(&chunk);
        while let Some(end) = buffered.iter().position(|byte| *byte == b'\n') {
            let mut line = buffered.drain(..=end).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            accept_sse_line(&line, &mut event_data, &mut mapper, &tx).await?;
            if mapper.terminal() {
                buffered.clear();
                break 'read;
            }
        }
    }
    if !buffered.is_empty() {
        accept_sse_line(&buffered, &mut event_data, &mut mapper, &tx).await?;
    }
    flush_event(&mut event_data, &mut mapper, &tx).await?;
    if !mapper.terminal() {
        return Err(CodexTransportError::with_mapper(
            "Codex SSE stream ended before a terminal response event",
            &mapper,
        ));
    }
    Ok(TransportOutcome {
        events_started: mapper.started(),
        terminal: mapper.terminal(),
        usage: mapper.usage().cloned(),
    })
}

async fn accept_sse_line(
    line: &[u8],
    event_data: &mut Vec<String>,
    mapper: &mut CodexEventMapper,
    tx: &mpsc::Sender<StreamPiece>,
) -> Result<(), CodexTransportError> {
    let line = std::str::from_utf8(line)
        .map_err(|_| CodexTransportError::with_mapper("Codex SSE was not valid UTF-8", mapper))?;
    if line.is_empty() {
        return flush_event(event_data, mapper, tx).await;
    }
    if line.starts_with(':') {
        return Ok(());
    }
    if let Some(data) = line.strip_prefix("data:") {
        event_data.push(data.strip_prefix(' ').unwrap_or(data).to_string());
    }
    Ok(())
}

async fn flush_event(
    event_data: &mut Vec<String>,
    mapper: &mut CodexEventMapper,
    tx: &mpsc::Sender<StreamPiece>,
) -> Result<(), CodexTransportError> {
    if event_data.is_empty() {
        return Ok(());
    }
    let data = event_data.join("\n");
    event_data.clear();
    if data == "[DONE]" {
        return Ok(());
    }
    let value = serde_json::from_str(&data).map_err(|error| {
        CodexTransportError::with_mapper(format!("invalid Codex SSE event: {error}"), mapper)
    })?;
    let pieces = mapper
        .accept(value)
        .map_err(|error| CodexTransportError::with_mapper(error, mapper))?;
    for piece in pieces {
        tx.send(piece)
            .await
            .map_err(|_| CodexTransportError::with_mapper("stream receiver closed", mapper))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::build_codex_request;
    use super::*;
    use crate::{Auth, Model, ModelApi, http_client_for_url};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn zstd_body_round_trips() {
        let body = serde_json::json!({"model":"gpt-5-codex","store":false});
        let encoded = encode_sse_body(&body).unwrap();
        assert_eq!(encoded.content_encoding, Some("zstd"));
        let decoded = zstd::stream::decode_all(Cursor::new(encoded.bytes)).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&decoded).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn streams_utf8_and_tool_calls_across_chunks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.contains("content-encoding: zstd"));
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
            let events = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你\"}\n\n",
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"grep\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"i1\",\"arguments\":\"{\\\"pattern\\\":\\\"TODO\\\"}\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            ).as_bytes();
            let split = events.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
            for part in [&events[..split], &events[split..]] {
                socket
                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                    .await
                    .unwrap();
                socket.write_all(part).await.unwrap();
                socket.write_all(b"\r\n").await.unwrap();
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
        });
        let base_url: &'static str = Box::leak(format!("http://{address}").into_boxed_str());
        let request = build_codex_request(
            &Model {
                provider: "openai-codex",
                id: "gpt-5-codex",
                api: ModelApi::OpenaiCodexResponses,
                base_url: Some(base_url),
                context_window: None,
                model_family: None,
            },
            &Auth {
                api_key: Some("token".into()),
                account_id: Some("acct".into()),
                ..Default::default()
            },
            &serde_json::json!({"messages":[],"session_id":"s","turn_id":"t"}),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let outcome = stream_sse(&http_client_for_url(&request.url), &request, tx)
            .await
            .unwrap();
        assert!(outcome.events_started && outcome.terminal);
        assert!(matches!(rx.recv().await, Some(StreamPiece::Text(text)) if text == "你"));
        assert!(
            matches!(rx.recv().await, Some(StreamPiece::ToolCall { id, name, arguments }) if id == "c1" && name == "grep" && arguments["pattern"] == "TODO")
        );
        server.await.unwrap();
    }
}
