use crate::{
    Auth, CONTEXT_HARD_LIMIT_BYTES, HttpRequestSpec, ModelApi, ModelStream, StreamPiece,
    http_client_for_url, parse_stream_body, send_request,
};
use async_trait::async_trait;
use std::path::Path;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CustomModel {
    pub provider: String,
    pub id: String,
    pub api: ModelApi,
    pub base_url: String,
    pub env: String,
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
    let mut request =
        reqwest::Client::new().get(format!("{}/models", base_url.trim_end_matches('/')));
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
    match model.api {
        ModelApi::OpenaiCompletions => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
            headers: bearer(auth),
            body: serde_json::json!({"model":model.id,"messages":messages,"tools":tools,"stream":true}),
        }),
        ModelApi::OpenaiResponses => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{}/responses", model.base_url.trim_end_matches('/')),
            headers: bearer(auth),
            body: serde_json::json!({"model":model.id,"input":messages,"tools":tools,"stream":true}),
        }),
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
    ) -> Result<(), String> {
        if prompt_bytes > CONTEXT_HARD_LIMIT_BYTES {
            return Err("context exceeds hard limit; compact required".into());
        }
        let request = build_custom_request(&self.model, &self.auth, context)?;
        let body = send_request(&self.client, &request).await?;
        for piece in parse_stream_body(&body) {
            tx.send(piece)
                .await
                .map_err(|_| "stream receiver closed".to_string())?;
        }
        Ok(())
    }
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
