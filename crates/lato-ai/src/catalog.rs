#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelApi {
    OpenaiCompletions,
    OpenaiResponses,
    OpenaiCodexResponses,
    AzureOpenaiResponses,
    AnthropicMessages,
    GoogleGenerativeAi,
    GoogleVertex,
    BedrockConverseStream,
    MistralConversations,
    PiMessages,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub provider: &'static str,
    pub id: &'static str,
    pub api: ModelApi,
    pub base_url: Option<&'static str>,
}

pub fn phase0_supported(api: ModelApi) -> bool {
    crate::dialect_implemented(api)
}

pub fn lookup_model(provider: &str, id: &str) -> Option<Model> {
    CATALOG
        .iter()
        .find(|m| m.provider == provider && m.id == id)
        .cloned()
}

pub const CATALOG: &[Model] = &[
    Model {
        provider: "openai",
        id: "gpt-4.1",
        api: ModelApi::OpenaiResponses,
        base_url: Some("https://api.openai.com/v1"),
    },
    Model {
        provider: "xai",
        id: "grok-4",
        api: ModelApi::OpenaiResponses,
        base_url: Some("https://api.x.ai/v1"),
    },
    Model {
        provider: "fireworks",
        id: "accounts/fireworks/models/llama-v3p1-8b-instruct",
        api: ModelApi::OpenaiCompletions,
        base_url: Some("https://api.fireworks.ai/inference/v1"),
    },
    Model {
        provider: "fireworks",
        id: "accounts/fireworks/models/claude-fixture-unimplemented",
        api: ModelApi::PiMessages,
        base_url: Some("https://api.fireworks.ai/inference"),
    },
    Model {
        provider: "kimi-coding",
        id: "kimi-k2",
        api: ModelApi::AnthropicMessages,
        base_url: Some("https://api.kimi.com/coding"),
    },
    Model {
        provider: "openai-codex",
        id: "codex-mini-latest",
        api: ModelApi::OpenaiCodexResponses,
        base_url: Some("https://chatgpt.com/backend-api"),
    },
    Model {
        provider: "google",
        id: "gemini-2.0-flash",
        api: ModelApi::GoogleGenerativeAi,
        base_url: Some("https://generativelanguage.googleapis.com"),
    },
    Model {
        provider: "azure-openai-responses",
        id: "gpt-4.1",
        api: ModelApi::AzureOpenaiResponses,
        base_url: Some("https://example.openai.azure.com/openai/deployments/gpt-4.1"),
    },
    Model {
        provider: "google-vertex",
        id: "gemini-2.0-flash",
        api: ModelApi::GoogleVertex,
        base_url: Some(
            "https://us-central1-aiplatform.googleapis.com/v1/projects/test/locations/us-central1",
        ),
    },
    Model {
        provider: "amazon-bedrock",
        id: "anthropic.claude-3-7-sonnet",
        api: ModelApi::BedrockConverseStream,
        base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
    },
    Model {
        provider: "mistral",
        id: "mistral-large-latest",
        api: ModelApi::MistralConversations,
        base_url: Some("https://api.mistral.ai"),
    },
    Model {
        provider: "radius",
        id: "radius-test",
        api: ModelApi::PiMessages,
        base_url: Some("https://api.example.invalid"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn e3_1_google_supported_after_dialect_connection() {
        let m = lookup_model("google", "gemini-2.0-flash").unwrap();
        assert!(phase0_supported(m.api));
    }

    #[test]
    fn b1_2_fireworks_mixed_supported_flags() {
        let yes = lookup_model(
            "fireworks",
            "accounts/fireworks/models/llama-v3p1-8b-instruct",
        )
        .unwrap();
        let no = lookup_model(
            "fireworks",
            "accounts/fireworks/models/claude-fixture-unimplemented",
        )
        .unwrap();
        assert!(phase0_supported(yes.api));
        assert!(!phase0_supported(no.api));
    }
}
