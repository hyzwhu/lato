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
            body: serde_json::json!({"model": model.id, "messages": messages, "tools":openai_tools, "stream": true}),
        }),
        ModelApi::OpenaiResponses | ModelApi::OpenaiCodexResponses => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/responses"),
            headers: bearer_headers(auth),
            body: serde_json::json!({"model": model.id, "input": messages, "tools":responses_tools(openai_tools), "stream": true}),
        }),
        ModelApi::AzureOpenaiResponses => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{base}/responses?api-version=2025-04-01-preview"),
            headers: api_key_headers(auth),
            body: serde_json::json!({"input": messages, "tools":responses_tools(openai_tools), "stream": true}),
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
                body: serde_json::json!({"model": model.id, "messages": messages, "tools":anthropic_tools(openai_tools), "max_tokens": 4096, "stream": true}),
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

fn responses_tools(openai_tools: serde_json::Value) -> serde_json::Value {
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

fn bearer_headers(auth: &Auth) -> Vec<(String, String)> {
    let mut headers = auth.headers.clone();
    if let Some(k) = &auth.api_key {
        headers.push(("authorization".into(), format!("Bearer {k}")));
    }
    headers.push(("content-type".into(), "application/json".into()));
    headers
}

pub async fn send_request(
    client: &reqwest::Client,
    spec: &HttpRequestSpec,
) -> Result<String, String> {
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
                let text = resp.text().await.map_err(|e| e.to_string())?;
                if status.is_success() {
                    return Ok(text);
                }
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
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer sk")
        );
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
