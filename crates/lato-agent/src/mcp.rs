// Derived from: Lato crates/lato-agent/src/skills.rs (generation-paired session handle pattern)
// License: Apache-2.0
// Lato changes: binds generation-scoped McpManager into ToolRuntime via McpToolBackend;
// transport call_tool is reachable only after an execution grant — never a second channel.

//! Session-owned MCP manager binding for the unified ToolRuntime safety membrane.
//!
//! `McpManager` owns transport lifecycle only. Model-visible MCP invocation must
//! always enter through `ToolRuntime::prepare_scoped` → PolicyEngine → approval →
//! `execute` → PostToolUse. This handle is the generation-paired backend installed
//! into `search_tool` / `use_tool` (and optional direct expand) at turn boundaries.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lato_core::{Retryability, ToolContext, ToolError};
use lato_extensions::{PluginSnapshot, materialize_mcp};
use lato_mcp::{
    McpDescriptorSet, McpManager, McpSearchHit, McpToolDescriptor, search_tools,
};
use lato_tools::McpToolBackend;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Session-owned indirection used by progressive MCP discovery tools.
///
/// The handle is stable for the lifetime of the tool runtime. The manager
/// installed in it changes only at a turn boundary (generation-paired with
/// [`lato_extensions::skills::SkillCatalog`] / [`lato_extensions::hooks::HookRegistry`]).
#[derive(Clone)]
pub struct SessionMcpHandle {
    inner: Arc<RwLock<SessionMcpState>>,
}

struct SessionMcpState {
    manager: Option<Arc<McpManager>>,
    generation: u64,
}

impl SessionMcpHandle {
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(SessionMcpState {
                manager: None,
                generation: 0,
            })),
        }
    }

    pub async fn snapshot_generation(&self) -> u64 {
        self.inner.read().await.generation
    }

    pub async fn manager(&self) -> Option<Arc<McpManager>> {
        self.inner.read().await.manager.clone()
    }

    /// Install a generation-scoped manager for the upcoming turn.
    ///
    /// Replaces any previous manager. The retired manager is shut down with a
    /// bounded deadline so process-tree resources are reaped.
    pub async fn install(&self, manager: Arc<McpManager>) {
        let generation = manager.generation();
        let previous = {
            let mut state = self.inner.write().await;
            let previous = state.manager.take();
            state.manager = Some(manager);
            state.generation = generation;
            previous
        };
        if let Some(previous) = previous {
            let _ = previous
                .shutdown_all(Instant::now() + Duration::from_secs(2))
                .await;
        }
    }

    /// Materialize descriptors from a frozen snapshot and install a fresh manager.
    pub async fn install_from_snapshot(
        &self,
        snapshot: &PluginSnapshot,
        cancel: CancellationToken,
    ) -> Arc<McpManager> {
        let descriptors = materialize_mcp(snapshot);
        let manager = Arc::new(McpManager::new(descriptors, cancel));
        self.install(Arc::clone(&manager)).await;
        manager
    }

    /// Install an empty generation-scoped manager (no servers).
    pub async fn install_empty(&self, generation: u64, cancel: CancellationToken) {
        let manager = Arc::new(McpManager::new(
            McpDescriptorSet::empty(generation),
            cancel,
        ));
        self.install(manager).await;
    }

    /// SessionEnd-friendly bounded shutdown of the active manager.
    pub async fn shutdown(&self, deadline: Instant) {
        let manager = {
            let mut state = self.inner.write().await;
            state.generation = 0;
            state.manager.take()
        };
        if let Some(manager) = manager {
            let _ = manager.shutdown_all(deadline).await;
        }
    }
}

impl Default for SessionMcpHandle {
    fn default() -> Self {
        Self::empty()
    }
}

#[async_trait]
impl McpToolBackend for SessionMcpHandle {
    async fn ensure_index(&self) -> Result<(), ToolError> {
        let Some(manager) = self.manager().await else {
            return Ok(());
        };
        manager
            .ensure_all_discovered()
            .await
            .map_err(map_session_mcp_error)
    }

    fn search(&self, query: &str) -> Vec<McpSearchHit> {
        let Ok(state) = self.inner.try_read() else {
            return Vec::new();
        };
        match &state.manager {
            Some(manager) => search_tools(manager.cache().as_ref(), query),
            None => Vec::new(),
        }
    }

    fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor> {
        let Ok(state) = self.inner.try_read() else {
            return None;
        };
        state
            .manager
            .as_ref()
            .and_then(|manager| manager.lookup(qualified_or_parts))
    }

    fn tools_for_servers(&self, servers: &[String]) -> Vec<McpToolDescriptor> {
        let Ok(state) = self.inner.try_read() else {
            return Vec::new();
        };
        let Some(manager) = &state.manager else {
            return Vec::new();
        };
        let cache = manager.cache();
        let mut out = Vec::new();
        for server in servers {
            if let Some(tools) = cache.tools_for_server(server) {
                out.extend(tools.iter().cloned());
            }
        }
        out
    }

    async fn call_tool(
        &self,
        context: &ToolContext,
        server: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        // Membrane invariant: ToolRuntime must have attached a grant before any
        // transport RPC. Refuse even if a manager is installed.
        if context.execution_grant.is_none() {
            return Err(ToolError::new(
                "policy.grant_missing",
                "MCP tools/call requires a ToolRuntime execution grant",
                Retryability::Never,
            ));
        }
        if context.cancellation.is_cancelled() {
            return Err(ToolError::new(
                "tool.cancelled",
                "tool call was cancelled",
                Retryability::Never,
            ));
        }
        let Some(manager) = self.manager().await else {
            return Err(ToolError::new(
                "mcp.manager_unbound",
                "MCP manager is not bound for this turn",
                Retryability::Never,
            ));
        };
        manager
            .call_tool(server, name, arguments)
            .await
            .map_err(map_session_mcp_error)
    }
}

fn map_session_mcp_error(error: lato_mcp::McpError) -> ToolError {
    let (code, retry) = match &error {
        lato_mcp::McpError::Cancelled => ("tool.cancelled", Retryability::Never),
        lato_mcp::McpError::Timeout { .. } => ("mcp.timeout", Retryability::AfterBackoff),
        lato_mcp::McpError::UnsafeUrl => ("mcp.unsafe_url", Retryability::Never),
        lato_mcp::McpError::NotRunning(_) => ("mcp.not_running", Retryability::AfterBackoff),
        lato_mcp::McpError::Unhealthy => ("mcp.unhealthy", Retryability::AfterBackoff),
        lato_mcp::McpError::Rpc { .. } => ("mcp.rpc_error", Retryability::Never),
        lato_mcp::McpError::Protocol { .. } => ("mcp.protocol", Retryability::Never),
        _ => ("mcp.execution_failed", Retryability::Never),
    };
    ToolError::new(code, error.to_string(), retry)
}
