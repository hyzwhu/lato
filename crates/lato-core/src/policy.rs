use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Ask,
    Auto,
    Always,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct ApprovalFingerprint(pub String);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct GrantId(pub u64);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxProfile {
    #[default]
    Off,
    Workspace,
    ReadOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Deny,
    PublicHttpsRead,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct EnvironmentPolicy {
    pub allowed_keys: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SandboxObligation {
    pub profile: SandboxProfile,
    pub workspace_root: PathBuf,
    pub writable_roots: Vec<PathBuf>,
    pub network: NetworkPolicy,
    pub environment: EnvironmentPolicy,
}

impl SandboxObligation {
    pub fn off(workspace_root: impl AsRef<Path>) -> Self {
        Self::for_profile(SandboxProfile::Off, workspace_root)
    }

    pub fn workspace(workspace_root: impl AsRef<Path>) -> Self {
        Self::for_profile(SandboxProfile::Workspace, workspace_root)
    }

    pub fn read_only(workspace_root: impl AsRef<Path>) -> Self {
        Self::for_profile(SandboxProfile::ReadOnly, workspace_root)
    }

    pub fn for_profile(profile: SandboxProfile, workspace_root: impl AsRef<Path>) -> Self {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        let writable_roots = if profile == SandboxProfile::Workspace {
            vec![workspace_root.clone()]
        } else {
            Vec::new()
        };
        Self {
            profile,
            workspace_root,
            writable_roots,
            network: NetworkPolicy::Deny,
            environment: EnvironmentPolicy::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PolicyRequest {
    pub session_id: crate::SessionId,
    pub turn_id: crate::TurnId,
    pub call_id: crate::ToolCallId,
    pub tool_name: crate::ToolName,
    pub arguments_digest: String,
    pub capabilities: Vec<crate::ToolCapability>,
    pub side_effect: crate::SideEffect,
    pub mode: PolicyMode,
    pub project_trusted: bool,
    pub sandbox: SandboxObligation,
    /// Tool-provided approval detail (Phase 7B7). Part of the approval
    /// fingerprint, so any change between approval and execution invalidates
    /// the grant. `None` keeps the generic capability/side-effect summary.
    #[serde(default)]
    pub detail: Option<String>,
    /// Whether the Plan-mode capability trim applies to this evaluation.
    #[serde(default)]
    pub plan_mode: bool,
    /// Descriptor layer of the tool; the Plan-mode overlay denies non-builtin
    /// tools even when they report read-only effects (spec §4).
    #[serde(default)]
    pub tool_layer: crate::ToolLayer,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ExecutionGrant {
    pub id: GrantId,
    pub fingerprint: ApprovalFingerprint,
    pub sandbox: SandboxObligation,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ApprovalRequest {
    pub request: PolicyRequest,
    pub fingerprint: ApprovalFingerprint,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PolicyDenial {
    pub code: String,
    pub message: String,
}

impl PolicyDenial {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow(ExecutionGrant),
    RequireApproval(ApprovalRequest),
    Deny(PolicyDenial),
}
