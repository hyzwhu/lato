#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    InvalidInput,
    Configuration,
    Model,
    Tool,
    Policy,
    Sandbox,
    Storage,
    Extension,
    Task,
    InternalInvariant,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Retryability {
    Never,
    Safe,
    AfterBackoff,
    RequiresDecision,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AgentError {
    pub code: String,
    pub category: ErrorCategory,
    pub message: String,
    pub retryability: Retryability,
}

impl AgentError {
    pub fn new(
        code: impl Into<String>,
        category: ErrorCategory,
        message: impl Into<String>,
        retryability: Retryability,
    ) -> Self {
        Self {
            code: code.into(),
            category,
            message: message.into(),
            retryability,
        }
    }
}
