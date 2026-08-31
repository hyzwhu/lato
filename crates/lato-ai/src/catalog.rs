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
    matches!(
        api,
        ModelApi::OpenaiCompletions | ModelApi::OpenaiResponses | ModelApi::AnthropicMessages
    )
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
        provider: "kimi-coding",
        id: "kimi-k2",
        api: ModelApi::AnthropicMessages,
        base_url: Some("https://api.kimi.com/coding"),
    },
    Model {
        provider: "google",
        id: "gemini-2.0-flash",
        api: ModelApi::GoogleGenerativeAi,
        base_url: Some("https://generativelanguage.googleapis.com"),
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a1_6_unsupported_google() {
        let m = lookup_model("google", "gemini-2.0-flash").unwrap();
        assert!(!phase0_supported(m.api));
    }
}
