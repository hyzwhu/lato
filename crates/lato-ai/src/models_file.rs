use crate::{Auth, HttpRequestSpec, ModelApi};
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
    match model.api {
        ModelApi::OpenaiCompletions => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{}/chat/completions", model.base_url.trim_end_matches('/')),
            headers: bearer(auth),
            body: serde_json::json!({"model":model.id,"messages":messages,"stream":true}),
        }),
        ModelApi::OpenaiResponses => Ok(HttpRequestSpec {
            method: "POST",
            url: format!("{}/responses", model.base_url.trim_end_matches('/')),
            headers: bearer(auth),
            body: serde_json::json!({"model":model.id,"input":messages,"stream":true}),
        }),
        _ => {
            // Static models cover the remaining dialect implementations; create a short-lived
            // equivalent request through the same protocol is intentionally rejected until its
            // custom URL contract is explicit.
            Err("custom model dialect unsupported".into())
        }
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
