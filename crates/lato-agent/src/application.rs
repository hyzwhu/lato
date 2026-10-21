//! Immutable application authority inherited by delegated runtime sessions.

use std::sync::{Arc, OnceLock};

use lato_core::{
    AgentProfile, SessionId, Tool, ToolCapability, VerificationPolicy, WorkspaceIntent,
};
use lato_tools::{RuntimeBuildError, ToolRuntime};

use crate::{AgentConfiguration, AgentMode, ApprovalRequest, ToolApproval};

pub(crate) type ApplicationSlot = Arc<OnceLock<Arc<ApplicationBinding>>>;

pub(crate) struct ApplicationBinding {
    pub configuration: AgentConfiguration,
    pub tools: Vec<Arc<dyn Tool>>,
    pub updates: Option<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>,
}

impl ApplicationBinding {
    pub fn fingerprint(&self) -> Option<String> {
        (!self.configuration.is_legacy_default()).then(|| self.configuration.marker())
    }

    pub fn restrict(
        &self,
        runtime: &ToolRuntime,
        capabilities: &[ToolCapability],
    ) -> Result<Arc<ToolRuntime>, RuntimeBuildError> {
        // The parent-only Workflow manager is intentionally absent from children.
        let parent = match &self.configuration.tools {
            Some(tools) => runtime.restricted_to(
                &tools
                    .iter()
                    .filter(|n| n.as_str() != "builtin:workflow")
                    .cloned()
                    .collect::<Vec<_>>(),
            )?,
            None => {
                runtime.restricted_for_capabilities(&AgentProfile::worker().capabilities, false)?
            }
        };
        let read_only = !capabilities.iter().any(|c| {
            matches!(
                c,
                ToolCapability::FileWrite
                    | ToolCapability::NetworkWrite
                    | ToolCapability::ProcessSpawn
            )
        });
        Ok(Arc::new(
            parent.restricted_for_capabilities(capabilities, read_only)?,
        ))
    }
}

pub(crate) fn application_profile(mut base: AgentProfile) -> AgentProfile {
    base.name = format!("application-{}", base.name);
    base.instructions="Complete the delegated business objective using only inherited application tools. Use actual tool results as evidence, ask for missing information, and report failures honestly. Return the format required by the task or its output contract.".into();
    if base.workspace == WorkspaceIntent::IsolatedWorktree {
        base.workspace = WorkspaceIntent::SharedSerializedWrite;
    }
    base.verification = VerificationPolicy::Programmatic;
    base
}

pub(crate) fn general_application(binding: Option<&ApplicationBinding>) -> bool {
    binding.is_some_and(|b| b.configuration.mode == AgentMode::General)
}

/// Preserve the original policy request/fingerprint while routing UI ownership.
pub(crate) struct OwnerApproval {
    pub inner: Arc<dyn ToolApproval>,
    pub owner: SessionId,
}

#[async_trait::async_trait]
impl ToolApproval for OwnerApproval {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        self.inner
            .approve_delegated(request, &serde_json::Value::Null, &self.owner)
            .await
    }
    async fn approve_with_arguments(
        &self,
        request: &ApprovalRequest,
        arguments: &serde_json::Value,
    ) -> bool {
        self.inner
            .approve_delegated(request, arguments, &self.owner)
            .await
    }
}
