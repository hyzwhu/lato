use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::MAX_PAYLOAD_BYTES;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum HookEventName {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    PreCompact,
    PostCompact,
}

impl HookEventName {
    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value
            .bytes()
            .filter(|byte| byte.is_ascii_alphanumeric())
            .map(|byte| byte.to_ascii_lowercase())
            .collect::<Vec<_>>();
        match normalized.as_slice() {
            b"sessionstart" => Some(Self::SessionStart),
            b"sessionend" => Some(Self::SessionEnd),
            b"userpromptsubmit" | b"promptsubmit" => Some(Self::UserPromptSubmit),
            b"pretooluse" | b"beforetool" => Some(Self::PreToolUse),
            b"posttooluse" | b"aftertool" => Some(Self::PostToolUse),
            b"stop" => Some(Self::Stop),
            b"precompact" | b"beforecompact" => Some(Self::PreCompact),
            b"postcompact" | b"aftercompact" => Some(Self::PostCompact),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
        }
    }

    pub const fn mode(self) -> HookMode {
        match self {
            Self::SessionStart | Self::SessionEnd | Self::PreCompact | Self::PostCompact => {
                HookMode::Observe
            }
            Self::UserPromptSubmit => HookMode::Prompt,
            Self::PreToolUse => HookMode::Tool,
            Self::PostToolUse => HookMode::PostTool,
            Self::Stop => HookMode::Stop,
        }
    }

    pub const fn default_timeout_ms(self) -> u64 {
        match self {
            Self::UserPromptSubmit => 30_000,
            Self::PostToolUse | Self::Stop => 600_000,
            Self::SessionEnd => 1_500,
            _ => 5_000,
        }
    }
}

impl<'de> Deserialize<'de> for HookEventName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| serde::de::Error::custom("unknown hook event"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookMode {
    Observe,
    Prompt,
    Tool,
    PostTool,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookEventEnvelope {
    pub schema_version: u16,
    pub event: HookEventName,
    pub generation: u64,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub payload: Value,
}

impl HookEventEnvelope {
    pub fn new(
        event: HookEventName,
        generation: u64,
        session_id: impl Into<String>,
        turn_id: Option<String>,
        payload: Value,
    ) -> Result<Self, HookEnvelopeError> {
        let envelope = Self {
            schema_version: 1,
            event,
            generation,
            session_id: session_id.into(),
            turn_id,
            payload,
        };
        if serde_json::to_vec(&envelope)
            .map_err(|_| HookEnvelopeError::Serialize)?
            .len()
            > MAX_PAYLOAD_BYTES
        {
            return Err(HookEnvelopeError::TooLarge);
        }
        Ok(envelope)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum HookEnvelopeError {
    #[error("hook event payload exceeds 128 KiB")]
    TooLarge,
    #[error("hook event payload cannot be serialized")]
    Serialize,
}
