#[derive(Clone, Copy, Debug)]
pub struct ApiKeyPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub default_base_url: Option<&'static str>,
    pub env: &'static [&'static str],
}

// Ported as data from Pi's env-api-key provider catalog (snapshot: 2026-08-31).
// Protocol selection intentionally lives on each Model, not on these presets.
pub const API_KEY_PRESETS: &[ApiKeyPreset] = &[
    p(
        "openai",
        "OpenAI",
        Some("https://api.openai.com/v1"),
        &["OPENAI_API_KEY"],
    ),
    p(
        "azure-openai-responses",
        "Azure OpenAI",
        None,
        &["AZURE_OPENAI_API_KEY"],
    ),
    p(
        "google",
        "Google AI",
        Some("https://generativelanguage.googleapis.com"),
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    ),
    p(
        "deepseek",
        "DeepSeek",
        Some("https://api.deepseek.com/v1"),
        &["DEEPSEEK_API_KEY"],
    ),
    p(
        "nvidia",
        "NVIDIA",
        Some("https://integrate.api.nvidia.com/v1"),
        &["NVIDIA_API_KEY"],
    ),
    p(
        "groq",
        "Groq",
        Some("https://api.groq.com/openai/v1"),
        &["GROQ_API_KEY"],
    ),
    p(
        "cerebras",
        "Cerebras",
        Some("https://api.cerebras.ai/v1"),
        &["CEREBRAS_API_KEY"],
    ),
    p(
        "mistral",
        "Mistral",
        Some("https://api.mistral.ai"),
        &["MISTRAL_API_KEY"],
    ),
    p(
        "huggingface",
        "Hugging Face",
        Some("https://router.huggingface.co/v1"),
        &["HF_TOKEN", "HUGGINGFACE_API_KEY"],
    ),
    p(
        "fireworks",
        "Fireworks",
        Some("https://api.fireworks.ai/inference"),
        &["FIREWORKS_API_KEY"],
    ),
    p(
        "together",
        "Together",
        Some("https://api.together.xyz/v1"),
        &["TOGETHER_API_KEY"],
    ),
    p("baseten", "Baseten", None, &["BASETEN_API_KEY"]),
    p(
        "vercel-ai-gateway",
        "Vercel AI Gateway",
        Some("https://ai-gateway.vercel.sh/v1"),
        &["AI_GATEWAY_API_KEY"],
    ),
    p(
        "zai",
        "Z.AI",
        Some("https://api.z.ai/api/paas/v4"),
        &["ZAI_API_KEY"],
    ),
    p(
        "zai-coding-cn",
        "Z.AI Coding CN",
        Some("https://open.bigmodel.cn/api/paas/v4"),
        &["ZAI_API_KEY"],
    ),
    p("opencode", "OpenCode", None, &["OPENCODE_API_KEY"]),
    p("opencode-go", "OpenCode Go", None, &["OPENCODE_API_KEY"]),
    p("ant-ling", "Ant Ling", None, &["ANT_LING_API_KEY"]),
    p(
        "minimax",
        "MiniMax",
        Some("https://api.minimax.io"),
        &["MINIMAX_API_KEY"],
    ),
    p(
        "minimax-cn",
        "MiniMax CN",
        Some("https://api.minimaxi.com"),
        &["MINIMAX_API_KEY"],
    ),
    p(
        "moonshotai",
        "Moonshot",
        Some("https://api.moonshot.ai/v1"),
        &["MOONSHOT_API_KEY"],
    ),
    p(
        "moonshotai-cn",
        "Moonshot CN",
        Some("https://api.moonshot.cn/v1"),
        &["MOONSHOT_API_KEY"],
    ),
    p("qwen-token-plan", "Qwen Token Plan", None, &["QWEN_TOKEN"]),
    p(
        "qwen-token-plan-individual",
        "Qwen Token Plan Individual",
        None,
        &["QWEN_TOKEN"],
    ),
    p(
        "qwen-token-plan-cn",
        "Qwen Token Plan CN",
        None,
        &["QWEN_TOKEN_CN"],
    ),
    p("xiaomi", "Xiaomi", None, &["XIAOMI_API_KEY"]),
    p(
        "xiaomi-token-plan-cn",
        "Xiaomi Token Plan CN",
        None,
        &["XIAOMI_API_KEY"],
    ),
    p(
        "xiaomi-token-plan-ams",
        "Xiaomi Token Plan AMS",
        None,
        &["XIAOMI_API_KEY"],
    ),
    p(
        "xiaomi-token-plan-sgp",
        "Xiaomi Token Plan SGP",
        None,
        &["XIAOMI_API_KEY"],
    ),
    p("xai", "xAI", Some("https://api.x.ai/v1"), &["XAI_API_KEY"]),
    p(
        "openrouter",
        "OpenRouter",
        Some("https://openrouter.ai/api/v1"),
        &["OPENROUTER_API_KEY"],
    ),
    p(
        "kimi-coding",
        "Kimi Coding",
        Some("https://api.kimi.com/coding"),
        &["KIMI_API_KEY"],
    ),
    p(
        "github-copilot",
        "GitHub Copilot Token",
        None,
        &["COPILOT_GITHUB_TOKEN"],
    ),
    p(
        "anthropic",
        "Anthropic",
        Some("https://api.anthropic.com"),
        &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
    ),
];

const fn p(
    id: &'static str,
    name: &'static str,
    default_base_url: Option<&'static str>,
    env: &'static [&'static str],
) -> ApiKeyPreset {
    ApiKeyPreset {
        id,
        name,
        default_base_url,
        env,
    }
}

pub fn preset(id: &str) -> Option<&'static ApiKeyPreset> {
    API_KEY_PRESETS.iter().find(|preset| preset.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_presets_are_data_and_shared_env_ids_remain_distinct() {
        assert!(API_KEY_PRESETS.len() >= 34);
        assert_eq!(
            preset("opencode").unwrap().env,
            preset("opencode-go").unwrap().env
        );
        assert_ne!(
            preset("opencode").unwrap().id,
            preset("opencode-go").unwrap().id
        );
        assert_eq!(
            preset("moonshotai").unwrap().env,
            preset("moonshotai-cn").unwrap().env
        );
    }
}
