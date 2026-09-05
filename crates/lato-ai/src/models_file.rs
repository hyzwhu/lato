use crate::{
    Auth, CONTEXT_HARD_LIMIT_BYTES, HttpRequestSpec, ModelApi, ModelCallReport, ModelStream,
    StreamPiece,
    api::{anthropic_request_body, openai_chat_body, responses_request_body},
    http_client_for_url, stream_http_request_with_tool_choice_fallback_with_report,
};
use async_trait::async_trait;
use lato_core::{ModelError, ModelErrorKind, Retryability};
use std::path::Path;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CustomModel {
    pub provider: String,
    pub id: String,
    pub api: ModelApi,
    pub base_url: String,
    pub env: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub model_family: Option<String>,
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn old_custom_model_files_keep_optional_metadata_empty() {
        let model: CustomModel = serde_json::from_value(serde_json::json!({
            "provider":"p", "id":"m", "api":"openai-responses",
            "base_url":"https://example.invalid", "env":"KEY"
        }))
        .unwrap();
        assert_eq!(model.context_window, None);
        assert_eq!(model.model_family, None);
    }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ModelsDocument {
    List(Vec<CustomModel>),
    Object { models: Vec<CustomModel> },
}

pub fn load_models_json(path: &Path) -> Result<Vec<CustomModel>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let doc: ModelsDocument = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let models = match doc {
        ModelsDocument::List(v) => v,
        ModelsDocument::Object { models } => models,
    };
    for model in &models {
        if model.base_url.trim().is_empty() || model.env.trim().is_empty() {
            return Err("custom model requires base_url and env".into());
        }
    }
    Ok(models)
}

pub fn refresh_models_from_openai_response(
    provider: &str,
    api: ModelApi,
    base_url: &str,
    env: &str,
    response: &serde_json::Value,
) -> Result<Vec<CustomModel>, String> {
    let data = response
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or("models response missing data")?;
    Ok(data
        .iter()
        .filter_map(|item| item.get("id").and_then(|v| v.as_str()))
        .map(|id| CustomModel {
            provider: provider.into(),
            id: id.into(),
            api,
            base_url: base_url.into(),
            env: env.into(),
            context_window: None,
            model_family: None,
        })
        .collect())
}

pub async fn refresh_openai_compatible_models(
    provider: &str,
    api: ModelApi,
    base_url: &str,
    env_name: &str,
    api_key: Option<&str>,
) -> Result<Vec<CustomModel>, String> {
    let models_url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = http_client_for_url(&models_url);
    let mut request = client.get(models_url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response: serde_json::Value = request
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    refresh_models_from_openai_response(provider, api, base_url, env_name, &response)
}

pub fn custom_model_auth(
    model: &CustomModel,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<Auth> {
    env(&model.env).map(|key| Auth {
        api_key: Some(key),
        account_id: None,
        headers: vec![],
        base_url: Some(model.base_url.clone()),
    })
}

pub fn build_custom_request(
    model: &CustomModel,
    auth: &Auth,
    messages: serde_json::Value,
) -> Result<HttpRequestSpec, String> {
    // Request construction is synchronous and does not retain model references.
    // Converting owned catalog fields here avoids leaking custom configuration.
    let context = messages;
    let messages = context
        .get("messages")
        .cloned()
        .unwrap_or_else(|| context.clone());
    let tools = context
        .get("tools")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let tool_choice = context.get("tool_choice").cloned();
    let stream = context
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    match model.api {
        ModelApi::OpenaiCompletions => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
            headers: bearer(auth),
            body: openai_chat_body(&model.id, messages, tools, tool_choice, stream),
        }),
        ModelApi::OpenaiResponses => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{}/responses", model.base_url.trim_end_matches('/')),
            headers: bearer(auth),
            body: responses_request_body(Some(&model.id), messages, tools, tool_choice, stream),
        }),
        ModelApi::AnthropicMessages => {
            let mut headers = auth.headers.clone();
            if let Some(key) = &auth.api_key {
                headers.push(("x-api-key".into(), key.clone()));
            }
            headers.push(("anthropic-version".into(), "2023-06-01".into()));
            headers.push(("content-type".into(), "application/json".into()));
            Ok(HttpRequestSpec {
                method: "POST",
                url: format!("{}/v1/messages", model.base_url.trim_end_matches('/')),
                headers,
                body: anthropic_request_body(&model.id, messages, tools, tool_choice, stream),
            })
        }
        _ => {
            // Static models cover the remaining dialect implementations; create a short-lived
            // equivalent request through the same protocol is intentionally rejected until its
            // custom URL contract is explicit.
            Err("custom model dialect unsupported".into())
        }
    }
}

pub struct CustomHttpModelStream {
    model: CustomModel,
    auth: Auth,
    client: reqwest::Client,
}

impl CustomHttpModelStream {
    pub fn new(model: CustomModel, auth: Auth) -> Self {
        let client = http_client_for_url(&model.base_url);
        Self {
            model,
            auth,
            client,
        }
    }
}

#[async_trait]
impl ModelStream for CustomHttpModelStream {
    async fn stream(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: tokio::sync::mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.stream_with_report(prompt_bytes, context, tx)
            .await
            .map(|_| ())
    }

    async fn stream_with_report(
        &self,
        prompt_bytes: usize,
        context: serde_json::Value,
        tx: tokio::sync::mpsc::Sender<StreamPiece>,
    ) -> Result<ModelCallReport, ModelError> {
        if prompt_bytes > CONTEXT_HARD_LIMIT_BYTES {
            return Err(ModelError::new(
                "model.context_overflow",
                "context exceeds hard limit; compact required",
                Retryability::Never,
            )
            .with_kind(ModelErrorKind::ContextOverflow));
        }
        if self.model.api == ModelApi::OpenaiCodexResponses {
            let request = crate::codex::build_codex_request_for_model(
                &self.model.id,
                Some(&self.model.base_url),
                &self.auth,
                &context,
            )
            .map_err(legacy_model_error)?;
            return crate::codex::stream_codex_with_report(&self.client, &request, tx).await;
        }
        let request =
            build_custom_request(&self.model, &self.auth, context).map_err(legacy_model_error)?;
        stream_http_request_with_tool_choice_fallback_with_report(&self.client, request, tx).await
    }
}

fn legacy_model_error(error: impl std::fmt::Display) -> ModelError {
    ModelError::new(
        "model.stream_interrupted",
        error.to_string(),
        Retryability::AfterBackoff,
    )
    .with_kind(ModelErrorKind::Transport)
}

fn bearer(auth: &Auth) -> Vec<(String, String)> {
    let mut headers = auth.headers.clone();
    if let Some(key) = &auth.api_key {
        headers.push(("authorization".into(), format!("Bearer {key}")));
    }
    headers.push(("content-type".into(), "application/json".into()));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e4_1_models_json_custom_endpoint_lists_and_builds_sampling_request() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("models.json");
        std::fs::write(&path, r#"{"models":[{"provider":"local","id":"qwen","api":"openai-completions","base_url":"http://127.0.0.1:8080/v1","env":"LOCAL_KEY"}]}"#).unwrap();
        let models = load_models_json(&path).unwrap();
        assert_eq!(models[0].provider, "local");
        let auth = custom_model_auth(&models[0], &|name| {
            (name == "LOCAL_KEY").then(|| "key".into())
        })
        .unwrap();
        let req = build_custom_request(&models[0], &auth, serde_json::json!([])).unwrap();
        assert_eq!(req.url, "http://127.0.0.1:8080/v1/chat/completions");
    }

    #[tokio::test]
    async fn provider_models_are_fetched_with_bearer_auth() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let count = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("GET /v1/models"));
            assert!(request.to_lowercase().contains("authorization: bearer key"));
            let body = r#"{"data":[{"id":"live-a"},{"id":"live-b"}]}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let models = refresh_openai_compatible_models(
            "minimax-cn",
            ModelApi::OpenaiCompletions,
            &format!("http://{address}/v1"),
            "MINIMAX_API_KEY",
            Some("key"),
        )
        .await
        .unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["live-a", "live-b"]
        );
    }

    #[test]
    fn e4_1_llama_cpp_refresh_models_fixture() {
        let models = refresh_models_from_openai_response(
            "llama.cpp",
            ModelApi::OpenaiCompletions,
            "http://127.0.0.1:8080/v1",
            "LLAMA_API_KEY",
            &serde_json::json!({"data":[{"id":"local-7b"},{"id":"local-13b"}]}),
        )
        .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "local-7b");
    }

    #[test]
    fn e4_1_custom_model_requires_explicit_env_and_url() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("models.json");
        std::fs::write(
            &path,
            r#"[{"provider":"bad","id":"x","api":"openai-completions","base_url":"","env":"KEY"}]"#,
        )
        .unwrap();
        assert!(load_models_json(&path).is_err());
    }
}
