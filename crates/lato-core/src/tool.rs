// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/common/xai-tool-runtime/src/tool.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/common/xai-tool-runtime/src/dispatch.rs
// License: Apache-2.0
// Lato changes: reduced runtime concepts to descriptors, a JSON invoke membrane, and typed errors

use crate::{
    AgentError, ErrorCategory, ExecutionGrant, Retryability, SessionId, ToolCallId, TurnId,
};
use async_trait::async_trait;
use semver::Version;
use serde_json::Value;
use std::{collections::HashSet, fmt, str::FromStr};
use tokio_util::sync::CancellationToken;

#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    pub fn parse(value: impl Into<String>) -> Result<Self, ToolNameError> {
        let value = value.into();
        let Some((namespace, name)) = value.split_once(':') else {
            return Err(ToolNameError::MissingNamespace);
        };
        if namespace.is_empty() || name.is_empty() {
            return Err(ToolNameError::EmptyPart);
        }
        if value.matches(':').count() != 1
            || !namespace.chars().all(valid_name_char)
            || !name.chars().all(valid_name_char)
        {
            return Err(ToolNameError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn namespace(&self) -> &str {
        self.0.split_once(':').unwrap().0
    }

    pub fn local_name(&self) -> &str {
        self.0.split_once(':').unwrap().1
    }
}

fn valid_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ToolName {
    type Err = ToolNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ToolNameError {
    #[error("tool name must include a namespace")]
    MissingNamespace,
    #[error("tool namespace and name must not be empty")]
    EmptyPart,
    #[error("tool name contains an invalid character")]
    InvalidCharacter,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    FileRead,
    FileWrite,
    #[serde(alias = "process")]
    ProcessSpawn,
    NetworkRead,
    NetworkWrite,
    #[serde(alias = "task")]
    TaskControl,
    #[serde(alias = "memory")]
    ExtensionInvoke,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    None,
    #[serde(alias = "workspace_read")]
    ReadOnly,
    #[serde(alias = "workspace_write")]
    WorkspaceMutation,
    ExternalMutation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency {
    Parallel,
    Serial,
    ResourceKeyed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIdempotency {
    Idempotent,
    WithKey,
    NonIdempotent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCancellation {
    Cooperative,
    KillProcess,
    Unsupported,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    serde::Deserialize,
    serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolLayer {
    #[default]
    Builtin,
    User,
    TrustedProject,
    SessionOverride,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolReplacement {
    pub target: ToolName,
    pub compatible_major: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolSource {
    pub layer: ToolLayer,
    pub id: String,
    pub replacement: Option<ToolReplacement>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolDescriptor {
    pub name: ToolName,
    pub version: Version,
    pub description: String,
    pub input_schema: Value,
    pub capabilities: Vec<ToolCapability>,
    pub side_effect: SideEffect,
    pub concurrency: ToolConcurrency,
    pub idempotency: ToolIdempotency,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub cancellation: ToolCancellation,
    pub source: ToolSource,
}

impl ToolDescriptor {
    pub fn validate(&self) -> Result<(), DescriptorError> {
        if self.description.trim().is_empty() {
            return Err(DescriptorError::EmptyDescription);
        }
        if !self.input_schema.is_object() {
            return Err(DescriptorError::SchemaNotObject);
        }
        if self.timeout_ms == 0 {
            return Err(DescriptorError::ZeroTimeout);
        }
        if self.max_output_bytes == 0 {
            return Err(DescriptorError::ZeroOutputLimit);
        }
        let mut capabilities = HashSet::new();
        if self
            .capabilities
            .iter()
            .any(|value| !capabilities.insert(value))
        {
            return Err(DescriptorError::DuplicateCapability);
        }
        self.validate_policy_metadata()?;
        Ok(())
    }

    pub fn validate_policy_metadata(&self) -> Result<(), DescriptorError> {
        let mismatch = self.capabilities.iter().any(|capability| match capability {
            ToolCapability::FileWrite => {
                matches!(self.side_effect, SideEffect::None | SideEffect::ReadOnly)
            }
            ToolCapability::NetworkWrite => self.side_effect != SideEffect::ExternalMutation,
            ToolCapability::ProcessSpawn => self.side_effect == SideEffect::None,
            _ => false,
        });
        if mismatch {
            return Err(DescriptorError::CapabilitySideEffectMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DescriptorError {
    #[error("tool description must not be empty")]
    EmptyDescription,
    #[error("tool input schema must be a JSON object")]
    SchemaNotObject,
    #[error("tool timeout must be greater than zero")]
    ZeroTimeout,
    #[error("tool output limit must be greater than zero")]
    ZeroOutputLimit,
    #[error("tool capabilities must not contain duplicates")]
    DuplicateCapability,
    #[error("tool capability and side effect metadata are inconsistent")]
    CapabilitySideEffectMismatch,
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub cancellation: CancellationToken,
    pub execution_grant: Option<ExecutionGrant>,
}

impl ToolContext {
    pub fn with_execution_grant(mut self, grant: ExecutionGrant) -> Self {
        self.execution_grant = Some(grant);
        self
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ToolOutput {
    pub content: String,
    pub metadata: Value,
    pub truncated: bool,
    pub artifact_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryability: Retryability,
}

impl ToolError {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retryability: Retryability,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryability,
        }
    }
}

impl From<ToolError> for AgentError {
    fn from(error: ToolError) -> Self {
        AgentError::new(
            error.code,
            ErrorCategory::Tool,
            error.message,
            error.retryability,
        )
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    /// Optional tool-specific detail shown in the human approval summary and
    /// bound into the approval fingerprint (Phase 7B7). Must be deterministic
    /// for identical arguments and must not contain secrets or absolute paths.
    fn approval_detail(&self, arguments: &Value) -> Option<String> {
        let _ = arguments;
        None
    }

    /// Optional pre-policy validation. Runs in the ToolRuntime membrane
    /// BEFORE the policy decision and any approval request, so frozen
    /// validation orders that place tool-side gates ahead of
    /// policy/approval (e.g. agentfield `start`: schema → allowlist →
    /// policy) are observable without prompting the user for a call that
    /// can never execute. Rejections must use the tool's stable error
    /// codes and have zero side effects; `invoke` remains authoritative
    /// and repeats its own checks (state may change between this hook and
    /// execution).
    fn validate_pre_policy(&self, arguments: &Value) -> Result<(), ToolError> {
        let _ = arguments;
        Ok(())
    }

    async fn invoke(&self, context: ToolContext, arguments: Value)
    -> Result<ToolOutput, ToolError>;
}
