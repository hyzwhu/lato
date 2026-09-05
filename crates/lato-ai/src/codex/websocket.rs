use super::{CodexRequest, CodexTransportError, TransportOutcome, events::CodexEventMapper};
use crate::StreamPiece;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

const MAX_CONNECTIONS: usize = 32;
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct PooledConnection {
    socket: Socket,
    last_used: Instant,
}

fn pool() -> &'static Mutex<HashMap<String, PooledConnection>> {
    static POOL: OnceLock<Mutex<HashMap<String, PooledConnection>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn stream_websocket(
    request: &CodexRequest,
    tx: mpsc::Sender<StreamPiece>,
) -> Result<TransportOutcome, CodexTransportError> {
    let key = connection_key(request);
    let mut connection = acquire(request, key.as_deref()).await?;
    let mut mapper = CodexEventMapper::default();
    let mut frame = request.body.clone();
    frame["type"] = serde_json::json!("response.create");
    if let Err(error) = connection
        .socket
        .send(Message::Text(frame.to_string().into()))
        .await
    {
        return Err(CodexTransportError::before_stream(error.to_string()));
    }

    let result = 'stream: loop {
        let Some(message) = connection.socket.next().await else {
            break Err(CodexTransportError::with_mapper(
                "Codex WebSocket closed before a terminal response event",
                &mapper,
            ));
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => break Err(CodexTransportError::with_mapper(error.to_string(), &mapper)),
        };
        let payload = match message {
            Message::Text(text) => text.as_bytes().to_vec(),
            Message::Binary(bytes) => bytes.to_vec(),
            Message::Ping(payload) => {
                if let Err(error) = connection.socket.send(Message::Pong(payload)).await {
                    break Err(CodexTransportError::with_mapper(error.to_string(), &mapper));
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(frame) => {
                break Err(CodexTransportError::with_mapper(
                    frame
                        .map(|frame| format!("Codex WebSocket closed: {}", frame.reason))
                        .unwrap_or_else(|| "Codex WebSocket closed".into()),
                    &mapper,
                ));
            }
            Message::Frame(_) => continue,
        };
        let value = match serde_json::from_slice(&payload) {
            Ok(value) => value,
            Err(error) => {
                break Err(CodexTransportError::with_mapper(
                    format!("invalid Codex WebSocket event: {error}"),
                    &mapper,
                ));
            }
        };
        let pieces = match mapper.accept(value) {
            Ok(pieces) => pieces,
            Err(error) => break Err(CodexTransportError::with_mapper(error, &mapper)),
        };
        for piece in pieces {
            if tx.send(piece).await.is_err() {
                break 'stream Err(CodexTransportError::with_mapper(
                    "stream receiver closed",
                    &mapper,
                ));
            }
        }
        if mapper.terminal() {
            break Ok(TransportOutcome {
                events_started: mapper.started(),
                terminal: true,
                usage: mapper.usage().cloned(),
            });
        }
    };

    if result.is_ok() {
        if let Some(key) = key {
            release(key, connection).await;
        } else {
            let _ = connection.socket.close(None).await;
        }
    } else {
        let _ = connection.socket.close(None).await;
    }
    result
}

fn connection_key(request: &CodexRequest) -> Option<String> {
    Some(format!(
        "{}:{}",
        request.header("chatgpt-account-id")?,
        request.session_key.as_deref()?
    ))
}

async fn acquire(
    request: &CodexRequest,
    key: Option<&str>,
) -> Result<PooledConnection, CodexTransportError> {
    if let Some(key) = key {
        let mut pool = pool().lock().await;
        let now = Instant::now();
        pool.retain(|_, connection| now.duration_since(connection.last_used) < IDLE_TIMEOUT);
        if let Some(connection) = pool.remove(key) {
            return Ok(connection);
        }
    }
    connect(request).await
}

async fn connect(request: &CodexRequest) -> Result<PooledConnection, CodexTransportError> {
    let mut url = url::Url::parse(&request.url)
        .map_err(|error| CodexTransportError::before_stream(error.to_string()))?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|_| CodexTransportError::before_stream("invalid Codex WebSocket URL"))?,
        "http" => url
            .set_scheme("ws")
            .map_err(|_| CodexTransportError::before_stream("invalid Codex WebSocket URL"))?,
        _ => {
            return Err(CodexTransportError::before_stream(
                "invalid Codex WebSocket scheme",
            ));
        }
    }
    let mut handshake = url
        .as_str()
        .into_client_request()
        .map_err(|error| CodexTransportError::before_stream(error.to_string()))?;
    for (key, value) in &request.headers {
        if matches_ignore_ascii_case(
            key,
            &[
                "accept",
                "content-type",
                "openai-beta",
                "x-client-request-id",
                "session-id",
            ],
        ) {
            continue;
        }
        let name = key
            .parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
            .map_err(|error| CodexTransportError::before_stream(error.to_string()))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| CodexTransportError::before_stream(error.to_string()))?;
        handshake.headers_mut().insert(name, value);
    }
    for (key, value) in [
        ("openai-beta", "responses_websockets=2026-02-06"),
        (
            "x-client-request-id",
            request.header("x-client-request-id").unwrap_or("lato"),
        ),
        ("session-id", request.header("session-id").unwrap_or("lato")),
    ] {
        handshake.headers_mut().insert(
            key.parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
                .unwrap(),
            HeaderValue::from_str(value)
                .map_err(|error| CodexTransportError::before_stream(error.to_string()))?,
        );
    }
    let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(handshake))
        .await
        .map_err(|_| CodexTransportError::before_stream("Codex WebSocket connection timed out"))?
        .map_err(|error| CodexTransportError::before_stream(error.to_string()))?;
    Ok(PooledConnection {
        socket,
        last_used: Instant::now(),
    })
}

async fn release(key: String, mut connection: PooledConnection) {
    connection.last_used = Instant::now();
    let mut pool = pool().lock().await;
    if pool.len() >= MAX_CONNECTIONS
        && let Some(oldest) = pool
            .iter()
            .min_by_key(|(_, connection)| connection.last_used)
            .map(|(key, _)| key.clone())
    {
        pool.remove(&oldest);
    }
    pool.insert(key, connection);
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Auth, Model, ModelApi, codex::build_codex_request};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{
        accept_hdr_async,
        tungstenite::handshake::server::{Request, Response},
    };

    #[tokio::test]
    async fn secure_websocket_starts_tls_and_returns_handshake_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut record_header = [0_u8; 5];
            tokio::time::timeout(
                Duration::from_secs(5),
                socket.read_exact(&mut record_header),
            )
            .await
            .expect("client must start a TLS handshake")
            .unwrap();
            assert_eq!(record_header[0], 22, "expected a TLS handshake record");
            assert_eq!(record_header[1], 3, "expected a TLS protocol version");
            // Closing during the handshake must return a transport error, not panic.
        });
        let request = CodexRequest {
            url: format!("https://{address}/codex/responses"),
            headers: Vec::new(),
            body: serde_json::json!({}),
            session_key: None,
        };
        let (tx, _rx) = mpsc::channel(1);
        let error = stream_websocket(&request, tx).await.unwrap_err();
        assert!(!error.events_started);
        assert!(!error.message.contains("timed out"));
        server.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn sends_codex_beta_and_response_create_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket =
                accept_hdr_async(socket, |request: &Request, response: Response| {
                    assert_eq!(
                        request.headers()["openai-beta"],
                        "responses_websockets=2026-02-06"
                    );
                    assert_eq!(request.headers()["chatgpt-account-id"], "acct");
                    Ok(response)
                })
                .await
                .unwrap();
            let frame = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(frame["type"], "response.create");
            websocket
                .send(Message::Text(
                    r#"{"type":"response.output_text.delta","delta":"ok"}"#.into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"r1"}}"#.into(),
                ))
                .await
                .unwrap();
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
            &serde_json::json!({"messages":[]}),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(2);
        let outcome = stream_websocket(&request, tx).await.unwrap();
        assert!(outcome.events_started && outcome.terminal);
        assert!(matches!(rx.recv().await, Some(StreamPiece::Text(text)) if text == "ok"));
        server.await.unwrap();
    }
}
