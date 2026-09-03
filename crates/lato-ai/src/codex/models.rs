use crate::{Auth, CustomModel, ModelApi, http_client_for_url};
use std::{collections::HashSet, time::Duration};

// The backend gates its catalog on the Codex wire-client version, not Lato's
// package version. Keep this aligned with the Codex protocol we implement.
const CODEX_CATALOG_CLIENT_VERSION: &str = "0.147.0";

/// Fetch the account's Codex catalog, whose entries use `slug` rather than OpenAI API `id`.
pub async fn refresh_codex_models(base_url: &str, auth: &Auth) -> Result<Vec<CustomModel>, String> {
    let token = auth
        .api_key
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("OpenAI Codex requires an OAuth access token")?;
    let account_id = auth
        .account_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("OpenAI Codex credential is missing its ChatGPT account ID")?;
    let url = format!("{}/codex/models", base_url.trim_end_matches('/'));
    let response = http_client_for_url(&url)
        .get(&url)
        .query(&[("client_version", CODEX_CATALOG_CLIENT_VERSION)])
        .bearer_auth(token)
        .header("chatgpt-account-id", account_id)
        .header("originator", "lato")
        .header("user-agent", concat!("lato/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| format!("Codex model catalog request failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("Codex model catalog request failed: {error}"))?;
    let catalog: CodexModelsResponse = response
        .json()
        .await
        .map_err(|error| format!("invalid Codex model catalog: {error}"))?;
    selectable_models(base_url, catalog)
}

#[derive(serde::Deserialize)]
struct CodexModelsResponse {
    models: Vec<CodexModel>,
}

#[derive(serde::Deserialize)]
struct CodexModel {
    slug: String,
    visibility: String,
    #[serde(default)]
    priority: i64,
}

fn selectable_models(
    base_url: &str,
    mut catalog: CodexModelsResponse,
) -> Result<Vec<CustomModel>, String> {
    catalog.models.sort_by_key(|model| model.priority);
    let mut seen = HashSet::new();
    let models: Vec<_> = catalog
        .models
        .into_iter()
        .filter(|model| model.visibility == "list" && !model.slug.trim().is_empty())
        .filter(|model| seen.insert(model.slug.clone()))
        .map(|model| CustomModel {
            provider: "openai-codex".into(),
            id: model.slug,
            api: ModelApi::OpenaiCodexResponses,
            base_url: base_url.trim_end_matches('/').into(),
            env: "LATO_API_KEY".into(),
        })
        .collect();
    if models.is_empty() {
        return Err("Codex model catalog returned no selectable models".into());
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn fetches_account_catalog_and_preserves_codex_dialect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let request = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
            assert!(request.starts_with(&format!(
                "get /backend-api/codex/models?client_version={CODEX_CATALOG_CLIENT_VERSION} "
            )));
            assert!(request.contains("authorization: bearer test-token\r\n"));
            assert!(request.contains("chatgpt-account-id: test-account\r\n"));
            assert!(request.contains("originator: lato\r\n"));
            let body = r#"{"models":[{"slug":"second","visibility":"list","priority":2},{"slug":"hidden","visibility":"hide","priority":0},{"slug":"first","visibility":"list","priority":1,"supported_in_api":false},{"slug":"first","visibility":"list","priority":3}]}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let base_url = format!("http://{address}/backend-api/");
        let models = refresh_codex_models(
            &base_url,
            &Auth {
                api_key: Some("test-token".into()),
                account_id: Some("test-account".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(
            models
                .iter()
                .all(|model| model.api == ModelApi::OpenaiCodexResponses
                    && model.provider == "openai-codex")
        );
        assert_eq!(models[0].base_url, base_url.trim_end_matches('/'));
    }

    #[test]
    fn rejects_empty_or_wrong_catalog_instead_of_using_obsolete_models() {
        for body in [
            r#"{"models":[]}"#,
            r#"{"models":[{"slug":"hidden","visibility":"hide"}]}"#,
        ] {
            assert!(
                selectable_models(
                    "https://chatgpt.com/backend-api",
                    serde_json::from_str(body).unwrap()
                )
                .is_err()
            );
        }
        assert!(
            serde_json::from_str::<CodexModelsResponse>(r#"{"data":[{"id":"wrong-api"}]}"#)
                .is_err()
        );
    }
}
