use super::*;
use serde_json::{Value, json};

fn call() -> Value {
    json!({"type":"function_call","id":"item-1","call_id":"call-1","name":"write_file","arguments":"{\"path\":\"a.txt\",\"contents\":\"你好\"}"})
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

#[test]
fn responses_completed_items_do_not_need_added_events() {
    let events = [json!({"type":"response.output_item.done","item":call()})];
    let pieces = parse_stream_body(&sse(&events)).unwrap();
    assert!(
        matches!(pieces.as_slice(), [StreamPiece::ToolCall {id, arguments, ..}] if id == "call-1" && arguments["contents"] == "你好")
    );
    let mut mapper = crate::codex::events::CodexEventMapper::default();
    assert_eq!(mapper.accept(events[0].clone()).unwrap().len(), 1);
}

#[test]
fn responses_terminal_output_is_reconciled_without_duplicate_calls() {
    let events = [
        json!({"type":"response.output_item.done","item":call()}),
        json!({"type":"response.completed","response":{"output":[call()]}}),
    ];
    assert_eq!(parse_stream_body(&sse(&events)).unwrap().len(), 1);
    assert_eq!(parse_stream_body(&sse(&events[1..])).unwrap().len(), 1);
}

#[test]
fn pretty_printed_json_responses_include_text_and_tools() {
    let body = json!({"object":"response","status":"completed","error":null,"output":[
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"checking"}]}, call()
    ]});
    let pieces = parse_stream_body(&serde_json::to_string_pretty(&body).unwrap()).unwrap();
    assert!(
        matches!(pieces.as_slice(), [StreamPiece::Text(text), StreamPiece::ToolCall {..}] if text == "checking")
    );
}

#[test]
fn pretty_printed_anthropic_json_includes_text_and_tools() {
    let body = json!({"type":"message","role":"assistant","content":[
        {"type":"text","text":"checking"},
        {"type":"tool_use","id":"a1","name":"read_file","input":{"path":"a.txt"}}
    ]});
    let pieces = parse_stream_body(&serde_json::to_string_pretty(&body).unwrap()).unwrap();
    assert!(
        matches!(pieces.as_slice(), [StreamPiece::Text(text), StreamPiece::ToolCall {id, ..}] if text == "checking" && id == "a1")
    );
}

#[test]
fn provider_failure_is_not_an_empty_success() {
    for event in [
        json!({"type":"response.failed","response":{"error":{"message":"provider overloaded"}}}),
        json!({"type":"error","error":{"message":"provider overloaded"}}),
        json!({"error":{"message":"provider overloaded"}}),
    ] {
        assert!(
            parse_stream_body(&sse(&[event]))
                .unwrap_err()
                .contains("provider overloaded")
        );
    }
}

#[test]
fn chat_tool_order_is_numeric() {
    let calls: Vec<_> = (0..12).map(|index| json!({"index":index,"id":format!("c{index}"),"function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}})).collect();
    let pieces = parse_stream_body(&sse(&[
        json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}]}),
    ]))
    .unwrap();
    let ids: Vec<_> = pieces
        .iter()
        .map(|p| match p {
            StreamPiece::ToolCall { id, .. } => id.clone(),
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, (0..12).map(|i| format!("c{i}")).collect::<Vec<_>>());
}

#[tokio::test]
async fn http_terminal_event_finishes_without_waiting_for_eof() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, hold) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 8192];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        let body = sse(&[json!({"type":"response.completed","response":{"output":[call()]}})]);
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n", body.len()).as_bytes()).await.unwrap();
        let _ = hold.await;
    });
    let request = crate::HttpRequestSpec {
        method: "POST",
        url: format!("http://{address}"),
        headers: vec![],
        body: json!({}),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream_http_request(&http_client_for_url(&request.url), &request, tx),
    )
    .await;
    let _ = release.send(());
    server.await.unwrap();
    result
        .expect("terminal event must end the request while connection is open")
        .unwrap();
    assert!(matches!(rx.recv().await, Some(StreamPiece::ToolCall {id, ..}) if id == "call-1"));
}

#[test]
fn completed_arguments_override_partial_deltas_and_emit_once() {
    let mut added = call();
    added["arguments"] = json!("");
    let events = [
        json!({"type":"response.output_item.added","item":added}),
        json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"{"}),
        json!({"type":"response.output_item.done","item":call()}),
        json!({"type":"response.completed","response":{"output":[call()]}}),
    ];
    let pieces = parse_stream_body(&sse(&events)).unwrap();
    assert!(
        matches!(pieces.as_slice(), [StreamPiece::ToolCall {arguments, ..}] if arguments["path"] == "a.txt")
    );
}

#[test]
fn all_completion_events_do_not_duplicate_calls_or_text() {
    let mut added = call();
    added["arguments"] = json!("");
    let message =
        json!({"id":"msg-1","type":"message","content":[{"type":"output_text","text":"checking"}]});
    let events = [
        json!({"type":"response.output_text.delta","item_id":"msg-1","delta":"checking"}),
        json!({"type":"response.output_item.done","item":message}),
        json!({"type":"response.output_item.added","item":added}),
        json!({"type":"response.function_call_arguments.done","item_id":"item-1","arguments":call()["arguments"]}),
        json!({"type":"response.output_item.done","item":call()}),
        json!({"type":"response.completed","response":{"output":[message,call()]}}),
    ];
    let pieces = parse_stream_body(&sse(&events)).unwrap();
    assert!(
        matches!(pieces.as_slice(), [StreamPiece::Text(text), StreamPiece::ToolCall {..}] if text == "checking")
    );
}

#[test]
fn incomplete_responses_and_unfinished_calls_return_errors() {
    assert!(parse_stream_body(&sse(&[json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}})])).unwrap_err().contains("max_output_tokens"));
    let mut added = call();
    added["arguments"] = json!("");
    assert!(
        parse_stream_body(&sse(&[
            json!({"type":"response.output_item.added","item":added}),
            json!({"type":"response.completed","response":{"output":[]}}),
        ]))
        .unwrap_err()
        .contains("unfinished tool calls")
    );
}

#[test]
fn multiline_sse_data_and_malformed_json_are_handled() {
    let event = json!({"type":"response.output_item.done","item":call()});
    let body = serde_json::to_string_pretty(&event)
        .unwrap()
        .lines()
        .map(|line| format!("data: {line}\r\n"))
        .collect::<String>()
        + "\r\n";
    assert_eq!(parse_stream_body(&body).unwrap().len(), 1);
    assert!(parse_stream_body("data: {not json}\n\n").is_err());
    assert!(parse_stream_body("{not json}").is_err());
}

#[test]
fn provider_history_preserves_text_before_tool_calls() {
    let history = json!([
        {"role":"assistant","content":"checking","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]},
        {"role":"tool","tool_call_id":"c1","content":"contents"}
    ]);
    let responses = crate::api::responses_input(history.clone());
    assert_eq!(responses[0]["content"], "checking");
    assert_eq!(responses[1]["call_id"], "c1");
    assert_eq!(responses[2]["call_id"], "c1");
    let (_, anthropic) = crate::api::anthropic_messages(history);
    assert_eq!(anthropic[0]["content"][0]["text"], "checking");
    assert_eq!(anthropic[0]["content"][1]["id"], "c1");
    assert_eq!(anthropic[1]["content"][0]["tool_use_id"], "c1");
}
