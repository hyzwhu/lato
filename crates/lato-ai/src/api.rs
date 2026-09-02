use crate::{Auth, Model, ModelApi};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequestSpec {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

pub fn dialect_implemented(api: ModelApi) -> bool {
    matches!(
        api,
        ModelApi::OpenaiCompletions
            | ModelApi::OpenaiResponses
            | ModelApi::OpenaiCodexResponses
            | ModelApi::AzureOpenaiResponses
            | ModelApi::AnthropicMessages
            | ModelApi::GoogleGenerativeAi
            | ModelApi::GoogleVertex
            | ModelApi::BedrockConverseStream
            | ModelApi::MistralConversations
    )
}

pub fn build_request(
    model: &Model,
    auth: &Auth,
    messages: serde_json::Value,
) -> Result<HttpRequestSpec, String> {
    if !dialect_implemented(model.api) {
        return Err("dialect_unimplemented".into());
    }
    let context = messages;
    let messages = context
        .get("messages")
        .cloned()
        .unwrap_or_else(|| context.clone());
    let openai_tools = context
        .get("tools")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(vec![]));
    let tool_choice = context.get("tool_choice").cloned();
    let stream = context
        .get("stream")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let base = auth
        .base_url
        .as_deref()
        .or(model.base_url)
        .ok_or("missing base_url")?
        .trim_end_matches('/');
    match model.api {
        ModelApi::OpenaiCompletions => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/chat/completions"),
            headers: bearer_headers(auth),
            body: openai_chat_body(model.id, messages, openai_tools, tool_choice, stream),
        }),
        ModelApi::OpenaiResponses | ModelApi::OpenaiCodexResponses => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/responses"),
            headers: bearer_headers(auth),
            body: responses_request_body(
                Some(model.id),
                messages,
                openai_tools,
                tool_choice,
                stream,
            ),
        }),
        ModelApi::AzureOpenaiResponses => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/responses?api-version=2025-04-01-preview"),
            headers: api_key_headers(auth),
            body: responses_request_body(None, messages, openai_tools, tool_choice, stream),
        }),
        ModelApi::AnthropicMessages => {
            let mut headers = auth.headers.clone();
            if let Some(k) = &auth.api_key {
                headers.push(("x-api-key".into(), k.clone()));
            }
            headers.push(("anthropic-version".into(), "2023-06-01".into()));
            headers.push(("content-type".into(), "application/json".into()));
            Ok(HttpRequestSpec {
                method: "POST",
                url: format!("{base}/v1/messages"),
                headers,
                body: anthropic_request_body(model.id, messages, openai_tools, tool_choice, stream),
            })
        }
        ModelApi::GoogleGenerativeAi => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/v1beta/models/{}:streamGenerateContent", model.id),
            headers: google_api_key_headers(auth),
            body: serde_json::json!({"contents": messages}),
        }),
        ModelApi::GoogleVertex => Ok(HttpRequestSpec {
            method: "POST",
            url: format!(
                "{base}/publishers/google/models/{}:streamGenerateContent",
                model.id
            ),
            headers: bearer_headers(auth),
            body: serde_json::json!({"contents": messages}),
        }),
        ModelApi::BedrockConverseStream => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/model/{}/converse-stream", model.id),
            headers: auth.headers.clone(),
            body: serde_json::json!({"messages": messages}),
        }),
        ModelApi::MistralConversations => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/v1/conversations"),
            headers: bearer_headers(auth),
            body: serde_json::json!({"model": model.id, "inputs": messages, "tools":openai_tools, "stream": true}),
        }),
        ModelApi::PiMessages => Err("dialect_unimplemented".into()),
    }
}

pub(crate) fn responses_input(messages: serde_json::Value) -> serde_json::Value {
    let Some(items) = messages.as_array() else {
        return messages;
    };
    serde_json::Value::Array(
        items
            .iter()
            .flat_map(|message| {
                if message.get("role").and_then(|v| v.as_str()) == Some("tool") {
                    return vec![serde_json::json!({
                        "type":"function_call_output",
                        "call_id":message.get("tool_call_id").cloned().unwrap_or_default(),
                        "output":message.get("content").cloned().unwrap_or_default()
                    })];
                }
                if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                    return calls
                        .iter()
                        .filter_map(|call| {
                            let function = call.get("function")?;
                            Some(serde_json::json!({
                                "type":"function_call",
                                "call_id":call.get("id").cloned().unwrap_or_default(),
                                "name":function.get("name").cloned().unwrap_or_default(),
                                "arguments":function.get("arguments").cloned().unwrap_or_default()
                            }))
                        })
                        .collect();
                }
                vec![message.clone()]
            })
            .collect(),
    )
}

pub(crate) fn anthropic_messages(messages: serde_json::Value) -> (String, serde_json::Value) {
    let Some(items) = messages.as_array() else {
        return (String::new(), messages);
    };
    let mut system = Vec::new();
    let mut out = Vec::new();
    for message in items {
        match message.get("role").and_then(|v| v.as_str()) {
            Some("system") => {
                if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
                    system.push(content.to_string());
                }
            }
            Some("tool") => out.push(serde_json::json!({
                "role":"user",
                "content":[{"type":"tool_result","tool_use_id":message.get("tool_call_id").cloned().unwrap_or_default(),"content":message.get("content").cloned().unwrap_or_default()}]
            })),
            Some("assistant") if message.get("tool_calls").is_some() => {
                let content = message
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|call| {
                        let function = call.get("function")?;
                        let input = function
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .unwrap_or_else(|| serde_json::json!({}));
                        Some(serde_json::json!({
                            "type":"tool_use",
                            "id":call.get("id").cloned().unwrap_or_default(),
                            "name":function.get("name").cloned().unwrap_or_default(),
                            "input":input
                        }))
                    })
                    .collect::<Vec<_>>();
                out.push(serde_json::json!({"role":"assistant","content":content}));
            }
            _ => out.push(message.clone()),
        }
    }
    (system.join("\n\n"), serde_json::Value::Array(out))
}

pub(crate) fn responses_request_body(
    model_id: Option<&str>,
    messages: serde_json::Value,
    openai_tools: serde_json::Value,
    tool_choice: Option<serde_json::Value>,
    stream: bool,
) -> serde_json::Value {
    let tools = responses_tools(openai_tools);
    let has_tools = tools.as_array().is_some_and(|items| !items.is_empty());
    let mut body = serde_json::json!({
        "input": responses_input(messages),
        "tools": tools,
        "stream": stream,
    });
    if let Some(model_id) = model_id {
        body["model"] = serde_json::json!(model_id);
    }
    if has_tools && let Some(tool_choice) = tool_choice {
        body["tool_choice"] = tool_choice;
    }
    body
}

pub(crate) fn anthropic_request_body(
    model_id: &str,
    messages: serde_json::Value,
    openai_tools: serde_json::Value,
    tool_choice: Option<serde_json::Value>,
    stream: bool,
) -> serde_json::Value {
    let (system, messages) = anthropic_messages(messages);
    let tools = anthropic_tools(openai_tools);
    let has_tools = tools.as_array().is_some_and(|items| !items.is_empty());
    let mut body = serde_json::json!({
        "model": model_id,
        "system": system,
        "messages": messages,
        "tools": tools,
        "max_tokens": 4096,
        "stream": stream,
    });
    if has_tools && let Some(tool_choice) = anthropic_tool_choice(tool_choice.as_ref()) {
        body["tool_choice"] = tool_choice;
    }
    body
}

fn anthropic_tool_choice(choice: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    match choice.and_then(|value| value.as_str()) {
        Some("required") => Some(serde_json::json!({"type":"any"})),
        Some("auto") => Some(serde_json::json!({"type":"auto"})),
        Some("none") => Some(serde_json::json!({"type":"none"})),
        _ => None,
    }
}

pub(crate) fn responses_tools(openai_tools: serde_json::Value) -> serde_json::Value {
    serde_json::Value::Array(openai_tools.as_array().into_iter().flatten().filter_map(|tool| {
        let function = tool.get("function")?;
        Some(serde_json::json!({
            "type":"function",
            "name":function.get("name")?,
            "description":function.get("description").cloned().unwrap_or_default(),
            "parameters":function.get("parameters").cloned().unwrap_or_else(|| serde_json::json!({"type":"object"}))
        }))
    }).collect())
}

fn anthropic_tools(openai_tools: serde_json::Value) -> serde_json::Value {
    serde_json::Value::Array(openai_tools.as_array().into_iter().flatten().filter_map(|tool| {
        let function = tool.get("function")?;
        Some(serde_json::json!({
            "name":function.get("name")?,
            "description":function.get("description").cloned().unwrap_or_default(),
            "input_schema":function.get("parameters").cloned().unwrap_or_else(|| serde_json::json!({"type":"object"}))
        }))
    }).collect())
}

fn google_api_key_headers(auth: &Auth) -> Vec<(String, String)> {
    let mut headers = auth.headers.clone();
    if let Some(k) = &auth.api_key {
        headers.push(("x-goog-api-key".into(), k.clone()));
    }
    headers.push(("content-type".into(), "application/json".into()));
    headers
}

fn api_key_headers(auth: &Auth) -> Vec<(String, String)> {
    let mut headers = auth.headers.clone();
    if let Some(k) = &auth.api_key {
        headers.push(("api-key".into(), k.clone()));
    }
    headers.push(("content-type".into(), "application/json".into()));
    headers
}

pub(crate) fn openai_chat_body(
    model_id: &str,
    messages: serde_json::Value,
    tools: serde_json::Value,
    tool_choice: Option<serde_json::Value>,
    stream: bool,
) -> serde_json::Value {
    let has_tools = tools.as_array().is_some_and(|items| !items.is_empty());
    let mut body = serde_json::json!({
        "model": model_id,
        "messages": messages,
        "stream": stream,
    });
    if has_tools {
        body["tools"] = tools;
        // SenseNova/GLM-family gateways often skip function calling unless tool_choice is set.
        // Callers can raise this to "required" when a workspace mutation was requested but
        // the model only produced assistant text.
        body["tool_choice"] = tool_choice.unwrap_or_else(|| serde_json::json!("auto"));
    }
    body
}

fn bearer_headers(auth: &Auth) -> Vec<(String, String)> {
    let mut headers = auth.headers.clone();
    if let Some(k) = &auth.api_key {
        headers.push(("authorization".into(), format!("Bearer {k}")));
    }
    headers.push(("content-type".into(), "application/json".into()));
    headers
}

pub fn http_client_for_url(url: &str) -> reqwest::Client {
    let loopback = url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    let builder = reqwest::Client::builder();
    if loopback {
        builder
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    } else {
        builder.build().unwrap_or_else(|_| reqwest::Client::new())
    }
}

pub async fn send_request_response(
    client: &reqwest::Client,
    spec: &HttpRequestSpec,
) -> Result<reqwest::Response, String> {
    let method = reqwest::Method::from_bytes(spec.method.as_bytes()).map_err(|e| e.to_string())?;
    let mut last_error = String::new();
    for attempt in 0..3 {
        let mut req = client.request(method.clone(), &spec.url);
        for (k, v) in &spec.headers {
            req = req.header(k, v);
        }
        match req.json(&spec.body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                let text = resp.text().await.map_err(|e| e.to_string())?;
                last_error = format!("http {status}: {text}");
                if !retryable_status(status.as_u16()) {
                    return Err(last_error);
                }
            }
            Err(error) => {
                let retryable = error.is_timeout() || error.is_connect();
                last_error = error.to_string();
                if !retryable {
                    return Err(last_error);
                }
            }
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(100 * (1 << attempt))).await;
        }
    }
    Err(format!(
        "sampling failed after 3 transient attempts: {last_error}"
    ))
}

pub async fn send_request(
    client: &reqwest::Client,
    spec: &HttpRequestSpec,
) -> Result<String, String> {
    send_request_response(client, spec)
        .await?
        .text()
        .await
        .map_err(|error| error.to_string())
}

fn retryable_status(status: u16) -> bool {
    status == 408 || status == 409 || status == 429 || status >= 500
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Auth, Model, lookup_model};

    fn auth() -> Auth {
        Auth {
            api_key: Some("sk".into()),
            account_id: None,
            headers: vec![],
            base_url: None,
        }
    }

    #[test]
    fn transient_retry_policy_is_bounded_to_retryable_statuses() {
        assert!(retryable_status(429));
        assert!(retryable_status(503));
        assert!(!retryable_status(401));
        assert!(!retryable_status(400));
    }

    #[test]
    fn b1_1_same_env_api_key_different_hosts_by_model_api() {
        let groq = Model {
            provider: "groq",
            id: "llama",
            api: ModelApi::OpenaiCompletions,
            base_url: Some("https://api.groq.com/openai/v1"),
        };
        let xai = lookup_model("xai", "grok-4").unwrap();
        let a = build_request(&groq, &auth(), serde_json::json!([])).unwrap();
        let b = build_request(&xai, &auth(), serde_json::json!([])).unwrap();
        assert_eq!(a.url, "https://api.groq.com/openai/v1/chat/completions");
        assert_eq!(b.url, "https://api.x.ai/v1/responses");
    }

    async fn roundtrip_fixture(spec: HttpRequestSpec) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (captured_tx, captured_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut bytes = vec![0u8; 64 * 1024];
            let count = socket.read(&mut bytes).unwrap();
            captured_tx
                .send(String::from_utf8_lossy(&bytes[..count]).into_owned())
                .unwrap();
            let body =
                "data: {\"choices\":[{\"delta\":{\"content\":\"fixture\"}}]}\n\ndata: [DONE]\n\n";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let mut local = spec;
        let path = url::Url::parse(&local.url).unwrap().path().to_string();
        local.url = format!("http://{address}{path}");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = send_request(&client, &local).await.unwrap();
        assert!(response.contains("fixture"));
        captured_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
    }

    #[tokio::test]
    async fn b1_3_three_dialects_http_vcr_roundtrip_offline() {
        let models = [
            Model {
                provider: "groq",
                id: "llama",
                api: ModelApi::OpenaiCompletions,
                base_url: Some("https://fixture/v1"),
            },
            lookup_model("openai", "gpt-4.1").unwrap(),
            lookup_model("kimi-coding", "kimi-k2").unwrap(),
        ];
        for model in models {
            let request = build_request(
                &model,
                &auth(),
                serde_json::json!([{"role":"user","content":"hi"}]),
            )
            .unwrap();
            let captured = roundtrip_fixture(request).await;
            assert!(captured.starts_with("POST "));
            assert!(captured.contains(model.id));
            if model.api == ModelApi::AnthropicMessages {
                assert!(captured.to_lowercase().contains("x-api-key: sk"));
            } else {
                assert!(captured.to_lowercase().contains("authorization: bearer sk"));
            }
        }
    }

    #[test]
    fn reference_china_provider_protocols_are_preserved() {
        let minimax = build_request(
            &lookup_model("minimax-cn", "MiniMax-M2.1").unwrap(),
            &auth(),
            serde_json::json!({"messages":[],"tools":[]}),
        )
        .unwrap();
        assert_eq!(
            minimax.url,
            "https://api.minimaxi.com/anthropic/v1/messages"
        );
        assert!(
            minimax
                .headers
                .iter()
                .any(|(name, value)| name == "x-api-key" && value == "sk")
        );

        for (provider, id, expected_url) in [
            (
                "zai-coding-cn",
                "glm-4.5",
                "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions",
            ),
            (
                "sensenova",
                "sensenova-6.8-flash-lite",
                "https://token.sensenova.cn/v1/chat/completions",
            ),
        ] {
            let request = build_request(
                &lookup_model(provider, id).unwrap(),
                &auth(),
                serde_json::json!({"messages":[],"tools":[]}),
            )
            .unwrap();
            assert_eq!(request.url, expected_url);
            assert_eq!(request.body["model"], id);
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, value)| name == "authorization" && value == "Bearer sk")
            );
        }
    }

    #[test]
    fn b1_3_openai_completions_shape() {
        let m = Model {
            provider: "groq",
            id: "llama",
            api: ModelApi::OpenaiCompletions,
            base_url: Some("https://h"),
        };
        let r = build_request(
            &m,
            &auth(),
            serde_json::json!([{"role":"user","content":"hi"}]),
        )
        .unwrap();
        assert_eq!(r.url, "https://h/chat/completions");
        assert_eq!(r.body["stream"], true);
        assert!(r.body.get("tool_choice").is_none());
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer sk")
        );
    }

    #[test]
    fn openai_completions_enables_tools_explicitly() {
        let m = Model {
            provider: "sensenova",
            id: "glm-5.2",
            api: ModelApi::OpenaiCompletions,
            base_url: Some("https://token.sensenova.cn/v1"),
        };
        let r = build_request(
            &m,
            &auth(),
            serde_json::json!({
                "messages":[{"role":"user","content":"写 hello.go"}],
                "tools":[{"type":"function","function":{"name":"run_terminal_command","parameters":{"type":"object"}}}]
            }),
        )
        .unwrap();
        assert_eq!(
            r.body["tools"][0]["function"]["name"],
            "run_terminal_command"
        );
        assert_eq!(r.body["tool_choice"], "auto");
    }

    #[test]
    fn openai_completions_honors_required_tool_choice_and_non_stream_retry() {
        let m = Model {
            provider: "sensenova",
            id: "glm-5.2",
            api: ModelApi::OpenaiCompletions,
            base_url: Some("https://token.sensenova.cn/v1"),
        };
        let r = build_request(
            &m,
            &auth(),
            serde_json::json!({
                "messages":[{"role":"user","content":"写 hello.go"}],
                "tools":[{"type":"function","function":{"name":"write_file","parameters":{"type":"object"}}}],
                "tool_choice":"required",
                "stream": false
            }),
        )
        .unwrap();
        assert_eq!(r.body["tool_choice"], "required");
        assert_eq!(r.body["stream"], false);
    }

    #[test]
    fn responses_and_anthropic_honor_required_tool_choice_and_non_stream_mode() {
        let context = serde_json::json!({
            "messages":[{"role":"user","content":"write a file"}],
            "tools":[{"type":"function","function":{"name":"write_file","parameters":{"type":"object"}}}],
            "tool_choice":"required",
            "stream":false
        });
        let responses = build_request(
            &lookup_model("openai", "gpt-4.1").unwrap(),
            &auth(),
            context.clone(),
        )
        .unwrap();
        assert_eq!(responses.body["tool_choice"], "required");
        assert_eq!(responses.body["stream"], false);

        let anthropic = build_request(
            &lookup_model("kimi-coding", "kimi-k2").unwrap(),
            &auth(),
            context,
        )
        .unwrap();
        assert_eq!(
            anthropic.body["tool_choice"],
            serde_json::json!({"type":"any"})
        );
        assert_eq!(anthropic.body["stream"], false);
    }

    #[test]
    fn b1_3_openai_responses_shape() {
        let m = lookup_model("openai", "gpt-4.1").unwrap();
        let r = build_request(&m, &auth(), serde_json::json!("hi")).unwrap();
        assert_eq!(r.url, "https://api.openai.com/v1/responses");
        assert_eq!(r.body["input"], "hi");
    }

    #[test]
    fn b1_3_anthropic_messages_shape() {
        let m = lookup_model("kimi-coding", "kimi-k2").unwrap();
        let r = build_request(
            &m,
            &auth(),
            serde_json::json!([{"role":"user","content":"hi"}]),
        )
        .unwrap();
        assert_eq!(r.url, "https://api.kimi.com/coding/v1/messages");
        assert!(r.headers.iter().any(|(k, v)| k == "x-api-key" && v == "sk"));
        assert_eq!(r.body["max_tokens"], 4096);
    }

    #[test]
    fn tool_history_is_converted_for_responses_and_anthropic_apis() {
        let history = serde_json::json!([
            {"role":"system","content":"sys"},
            {"role":"user","content":"pwd"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"run_terminal_command","arguments":"{\"command\":\"pwd\"}"}}]},
            {"role":"tool","tool_call_id":"c1","content":"/tmp/project\n"}
        ]);

        let openai = build_request(
            &lookup_model("openai", "gpt-4.1").unwrap(),
            &auth(),
            history.clone(),
        )
        .unwrap();
        assert_eq!(openai.body["input"][2]["type"], "function_call");
        assert_eq!(openai.body["input"][3]["type"], "function_call_output");
        assert_eq!(openai.body["input"][3]["call_id"], "c1");

        let anthropic = build_request(
            &lookup_model("kimi-coding", "kimi-k2").unwrap(),
            &auth(),
            history,
        )
        .unwrap();
        assert_eq!(anthropic.body["system"], "sys");
        assert_eq!(
            anthropic.body["messages"][1]["content"][0]["type"],
            "tool_use"
        );
        assert_eq!(
            anthropic.body["messages"][2]["content"][0]["type"],
            "tool_result"
        );
    }

    #[test]
    fn e3_1_cloud_dialects_have_request_shapes() {
        let cases = [
            ("google", "gemini-2.0-flash", ":streamGenerateContent"),
            ("azure-openai-responses", "gpt-4.1", "api-version="),
            (
                "google-vertex",
                "gemini-2.0-flash",
                "publishers/google/models",
            ),
            (
                "amazon-bedrock",
                "anthropic.claude-3-7-sonnet",
                "converse-stream",
            ),
            ("mistral", "mistral-large-latest", "/v1/conversations"),
        ];
        for (provider, id, expected) in cases {
            let m = lookup_model(provider, id).unwrap();
            let request = build_request(&m, &auth(), serde_json::json!([])).unwrap();
            assert!(request.url.contains(expected), "{}", request.url);
        }
        let cloudflare = Model {
            provider: "cloudflare-workers-ai",
            id: "@cf/meta/llama",
            api: ModelApi::OpenaiCompletions,
            base_url: Some("https://api.cloudflare.com/client/v4/accounts/a/ai/v1"),
        };
        assert!(
            build_request(&cloudflare, &auth(), serde_json::json!([]))
                .unwrap()
                .url
                .contains("cloudflare.com")
        );
    }

    #[test]
    fn b1_7_anthropic_auth_token_uses_bearer_override_header() {
        let m = Model {
            provider: "anthropic",
            id: "claude",
            api: ModelApi::AnthropicMessages,
            base_url: Some("https://api.anthropic.com"),
        };
        let bearer = Auth {
            api_key: None,
            account_id: None,
            headers: vec![("authorization".into(), "Bearer token".into())],
            base_url: None,
        };
        let r = build_request(&m, &bearer, serde_json::json!([])).unwrap();
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer token")
        );
        assert!(!r.headers.iter().any(|(k, _)| k == "x-api-key"));
    }
}
