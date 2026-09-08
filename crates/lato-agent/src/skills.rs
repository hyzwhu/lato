use std::sync::Arc;

use async_trait::async_trait;
use lato_core::{Retryability, SkillInvocationOrigin, ToolCapability, ToolContext, ToolError};
use lato_extensions::skills::{SkillCatalog, SkillDiscovery, SkillInvokeError};
use lato_tools::{
    BuiltinToolEnvironment, ResolvedSkill, RuntimeBuildError, SkillResolver, ToolRuntime,
};
use lato_workspace::{FileLocks, SessionTrust};
use std::path::PathBuf;
use tokio::sync::RwLock;

/// Session-owned indirection used by the built-in `skill` tool.
///
/// The handle itself is stable for the lifetime of the tool runtime. The
/// catalog installed in it changes only at a turn boundary, and every
/// invocation takes an immutable `Arc` snapshot before resolving.
#[derive(Clone)]
pub struct SessionSkillHandle {
    catalog: Arc<RwLock<Arc<SkillCatalog>>>,
}

/// Opaque, construction-safe pairing of a tool runtime and its skill catalog
/// handle. Consumers cannot substitute either half after construction.
pub struct SkillRuntimeBinding {
    runtime: Arc<ToolRuntime>,
    handle: SessionSkillHandle,
}

impl SkillRuntimeBinding {
    pub fn builtin(
        cwd: PathBuf,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
    ) -> Result<Self, RuntimeBuildError> {
        Self::builtin_for_capabilities(cwd, locks, trust, None)
    }

    pub fn builtin_for_capabilities(
        cwd: PathBuf,
        locks: Arc<FileLocks>,
        trust: SessionTrust,
        capabilities: Option<&[ToolCapability]>,
    ) -> Result<Self, RuntimeBuildError> {
        Self::build(|resolver| {
            lato_tools::builtin_tool_runtime_for_capabilities(
                BuiltinToolEnvironment {
                    cwd,
                    locks,
                    trust,
                    skill_resolver: Some(resolver),
                },
                capabilities,
            )
        })
    }

    pub(crate) fn build(
        builder: impl FnOnce(Arc<dyn SkillResolver>) -> Result<Arc<ToolRuntime>, RuntimeBuildError>,
    ) -> Result<Self, RuntimeBuildError> {
        let handle = SessionSkillHandle::default();
        let runtime = builder(Arc::new(handle.clone()))?;
        Ok(Self { runtime, handle })
    }

    pub(crate) fn into_parts(self) -> (Arc<ToolRuntime>, SessionSkillHandle) {
        (self.runtime, self.handle)
    }
}

impl Default for SessionSkillHandle {
    fn default() -> Self {
        Self::new(SkillCatalog::from_discovery(SkillDiscovery::default()))
    }
}

impl SessionSkillHandle {
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self {
            catalog: Arc::new(RwLock::new(catalog)),
        }
    }

    pub async fn install(&self, catalog: Arc<SkillCatalog>) {
        *self.catalog.write().await = catalog;
    }

    pub async fn snapshot(&self) -> Arc<SkillCatalog> {
        let catalog = self.catalog.read().await;
        Arc::clone(&catalog)
    }
}

#[async_trait]
impl SkillResolver for SessionSkillHandle {
    async fn invoke(
        &self,
        context: &ToolContext,
        skill: &str,
        args: Option<&str>,
    ) -> Result<ResolvedSkill, ToolError> {
        let catalog = self.snapshot().await;
        let invocation = catalog
            .invoke(
                SkillInvocationOrigin::Model,
                skill,
                args,
                context.session_id.as_str(),
            )
            .map_err(skill_error)?;
        Ok(ResolvedSkill {
            qualified_name: invocation.qualified_name,
            message: invocation.message,
            allowed_tool_specs: invocation
                .allowed_tools
                .map(|specs| specs.iter().cloned().collect()),
            body_hash: invocation.body_hash,
        })
    }
}

fn skill_error(error: SkillInvokeError) -> ToolError {
    let code = match &error {
        SkillInvokeError::NotFound { .. } => "skill.not_found",
        SkillInvokeError::Ambiguous { .. } => "skill.ambiguous",
        SkillInvokeError::UserInvocationDisabled { .. } => "skill.user_invocation_disabled",
        SkillInvokeError::ModelInvocationDisabled { .. } => "skill.model_invocation_disabled",
        SkillInvokeError::ExpansionTooLarge { .. } => "skill.expansion_too_large",
    };
    ToolError::new(code, error.to_string(), Retryability::Never)
}
