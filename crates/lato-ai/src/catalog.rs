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
    pub context_window: Option<u64>,
    pub model_family: Option<&'static str>,
}

impl Model {
    pub fn metadata(&self) -> crate::ModelMetadata {
        crate::ModelMetadata {
            context_window: self.context_window,
            model_family: self
                .model_family
                .map(str::to_owned)
                .or_else(|| Some(format!("{}/{}", self.provider, self.api.as_str()))),
        }
    }
}

impl ModelApi {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCompletions => "openai-completions",
            Self::OpenaiResponses => "openai-responses",
            Self::OpenaiCodexResponses => "openai-codex-responses",
            Self::AzureOpenaiResponses => "azure-openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGenerativeAi => "google-generative-ai",
            Self::GoogleVertex => "google-vertex",
            Self::BedrockConverseStream => "bedrock-converse-stream",
            Self::MistralConversations => "mistral-conversations",
            Self::PiMessages => "pi-messages",
        }
    }
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
        context_window: Some(1_047_576),
        model_family: Some("openai"),
    },
    Model {
        provider: "xai",
        id: "grok-4",
        api: ModelApi::OpenaiResponses,
        base_url: Some("https://api.x.ai/v1"),
        context_window: Some(256_000),
        model_family: Some("grok"),
    },
    Model {
        provider: "fireworks",
        id: "accounts/fireworks/models/llama-v3p1-8b-instruct",
        api: ModelApi::OpenaiCompletions,
        base_url: Some("https://api.fireworks.ai/inference/v1"),
        context_window: Some(131_072),
        model_family: Some("llama"),
    },
    Model {
        provider: "fireworks",
        id: "accounts/fireworks/models/claude-fixture-unimplemented",
        api: ModelApi::PiMessages,
        base_url: Some("https://api.fireworks.ai/inference"),
        context_window: None,
        model_family: None,
    },
    Model {
        provider: "kimi-coding",
        id: "kimi-k2",
        api: ModelApi::AnthropicMessages,
        base_url: Some("https://api.kimi.com/coding"),
        context_window: Some(131_072),
        model_family: Some("kimi"),
    },
    Model {
        provider: "openai-codex",
        id: "codex-mini-latest",
        api: ModelApi::OpenaiCodexResponses,
        base_url: Some("https://chatgpt.com/backend-api"),
        context_window: Some(200_000),
        model_family: Some("openai"),
    },
    Model {
        provider: "google",
        id: "gemini-2.0-flash",
        api: ModelApi::GoogleGenerativeAi,
        base_url: Some("https://generativelanguage.googleapis.com"),
        context_window: Some(1_048_576),
        model_family: Some("gemini"),
    },
    Model {
        provider: "azure-openai-responses",
        id: "gpt-4.1",
        api: ModelApi::AzureOpenaiResponses,
        base_url: Some("https://example.openai.azure.com/openai/deployments/gpt-4.1"),
        context_window: Some(1_047_576),
        model_family: Some("openai"),
    },
    Model {
        provider: "google-vertex",
        id: "gemini-2.0-flash",
        api: ModelApi::GoogleVertex,
        base_url: Some(
            "https://us-central1-aiplatform.googleapis.com/v1/projects/test/locations/us-central1",
        ),
        context_window: Some(1_048_576),
        model_family: Some("gemini"),
    },
    Model {
        provider: "amazon-bedrock",
        id: "anthropic.claude-3-7-sonnet",
        api: ModelApi::BedrockConverseStream,
        base_url: Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
        context_window: Some(200_000),
        model_family: Some("anthropic"),
    },
    Model {
        provider: "minimax-cn",
        id: "MiniMax-M2.1",
        api: ModelApi::AnthropicMessages,
        base_url: Some("https://api.minimaxi.com/anthropic"),
        context_window: Some(204_800),
        model_family: Some("minimax"),
    },
    Model {
        provider: "minimax",
        id: "MiniMax-M2.1",
        api: ModelApi::AnthropicMessages,
        base_url: Some("https://api.minimax.io/anthropic"),
        context_window: Some(204_800),
        model_family: Some("minimax"),
    },
    Model {
        provider: "zai",
        id: "glm-4.5",
        api: ModelApi::OpenaiCompletions,
        base_url: Some("https://api.z.ai/api/coding/paas/v4"),
        context_window: Some(131_072),
        model_family: Some("glm"),
    },
    Model {
        provider: "zai-coding-cn",
        id: "glm-4.5",
        api: ModelApi::OpenaiCompletions,
        base_url: Some("https://open.bigmodel.cn/api/coding/paas/v4"),
        context_window: Some(131_072),
        model_family: Some("glm"),
    },
    Model {
        provider: "zhipu",
        id: "glm-4.5",
        api: ModelApi::OpenaiCompletions,
        base_url: Some("https://open.bigmodel.cn/api/coding/paas/v4"),
        context_window: Some(131_072),
        model_family: Some("glm"),
    },
    Model {
        provider: "sensenova",
        id: "sensenova-6.8-flash-lite",
        api: ModelApi::OpenaiCompletions,
        base_url: Some("https://token.sensenova.cn/v1"),
        context_window: None,
        model_family: None,
    },
    Model {
        provider: "mistral",
        id: "mistral-large-latest",
        api: ModelApi::MistralConversations,
        base_url: Some("https://api.mistral.ai"),
        context_window: Some(131_072),
        model_family: Some("mistral"),
    },
    Model {
        provider: "radius",
        id: "radius-test",
        api: ModelApi::PiMessages,
        base_url: Some("https://api.example.invalid"),
        context_window: Some(10_000),
        model_family: Some("fixture-a"),
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
    fn reference_china_models_are_advertised_with_provider_owned_protocols() {
        for (provider, id, api) in [
            ("minimax-cn", "MiniMax-M2.1", ModelApi::AnthropicMessages),
            ("zai-coding-cn", "glm-4.5", ModelApi::OpenaiCompletions),
            (
                "sensenova",
                "sensenova-6.8-flash-lite",
                ModelApi::OpenaiCompletions,
            ),
        ] {
            let model = lookup_model(provider, id).unwrap();
            assert_eq!(model.api, api);
            assert!(phase0_supported(model.api));
        }
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

    #[test]
    fn supported_catalog_entries_have_compaction_metadata() {
        for model in CATALOG.iter().filter(|model| phase0_supported(model.api)) {
            let metadata = model.metadata();
            assert!(
                metadata
                    .model_family
                    .as_deref()
                    .is_some_and(|family| !family.is_empty()),
                "missing family for {}/{}",
                model.provider,
                model.id
            );
            if model.provider != "sensenova" {
                assert!(
                    metadata.context_window.is_some_and(|window| window > 0),
                    "missing context window for {}/{}",
                    model.provider,
                    model.id
                );
            }
        }
    }
}
