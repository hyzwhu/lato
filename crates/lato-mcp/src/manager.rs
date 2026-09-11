//! Session-facing MCP manager.
//!
//! Security invariant: this type owns transport lifecycle, generation-scoped
//! schema discovery, and a **transport-level** `call_tool` RPC. It must **not**
//! expose a model-facing execution channel around `ToolRuntime`. Model-visible
//! MCP invocation happens only through `lato-tools` providers (`use_tool` /
//! optional direct expand) after an execution grant is present.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
    time::Instant,
};

use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    config::McpDescriptorSet,
    error::McpError,
    lifecycle::{self, McpServerHandle, initialize_with_resolver, start_server_with_resolver},
    names::qualify_tool,
    protocol::{InitializeResult, tools_call_params},
    registry::{
        MAX_TOOLS_LIST_PAGES, McpSchemaCache, McpToolDescriptor, parse_tools_list_result,
        tools_list_params,
    },
    transport::{McpDnsResolver, SystemMcpDnsResolver},
};

pub struct McpManager {
    generation: u64,
    descriptors: Arc<McpDescriptorSet>,
    cancel: CancellationToken,
    servers: Mutex<HashMap<String, McpServerHandle>>,
    cache: RwLock<Arc<McpSchemaCache>>,
    discovered: Mutex<HashSet<String>>,
}

impl McpManager {
    pub fn new(descriptors: Arc<McpDescriptorSet>, cancel: CancellationToken) -> Self {
        let generation = descriptors.generation;
        Self {
            generation,
            descriptors,
            cancel,
            servers: Mutex::new(HashMap::new()),
            cache: RwLock::new(Arc::new(McpSchemaCache::new(generation))),
            discovered: Mutex::new(HashSet::new()),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn descriptors(&self) -> Arc<McpDescriptorSet> {
        Arc::clone(&self.descriptors)
    }

    /// Snapshot of the generation-scoped schema cache.
    pub fn cache(&self) -> Arc<McpSchemaCache> {
        Arc::clone(
            &self
                .cache
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub async fn ensure_started(&self, server_name: &str) -> Result<(), McpError> {
        self.ensure_started_with_resolver(server_name, &SystemMcpDnsResolver)
            .await
    }

    pub async fn ensure_started_with_resolver(
        &self,
        server_name: &str,
        resolver: &dyn McpDnsResolver,
    ) -> Result<(), McpError> {
        {
            let servers = self.servers.lock().await;
            if servers.contains_key(server_name) {
                return Ok(());
            }
        }
        let spec = self
            .descriptors
            .servers
            .iter()
            .find(|server| server.server_name == server_name)
            .ok_or_else(|| McpError::NotRunning(server_name.to_owned()))?;
        let handle =
            start_server_with_resolver(spec, self.generation, self.cancel.child_token(), resolver)
                .await?;
        let mut servers = self.servers.lock().await;
        // Another task may have won the race.
        if servers.contains_key(server_name) {
            let _ = lifecycle::shutdown_server(handle, Instant::now()).await;
            return Ok(());
        }
        servers.insert(server_name.to_owned(), handle);
        Ok(())
    }

    pub async fn initialize_server(&self, server_name: &str) -> Result<InitializeResult, McpError> {
        self.initialize_server_with_resolver(server_name, &SystemMcpDnsResolver)
            .await
    }

    pub async fn initialize_server_with_resolver(
        &self,
        server_name: &str,
        resolver: &dyn McpDnsResolver,
    ) -> Result<InitializeResult, McpError> {
        self.ensure_started_with_resolver(server_name, resolver)
            .await?;
        let mut servers = self.servers.lock().await;
        let handle = servers
            .get_mut(server_name)
            .ok_or_else(|| McpError::NotRunning(server_name.to_owned()))?;
        if let Some(existing) = handle.initialize_result().cloned() {
            return Ok(existing);
        }
        initialize_with_resolver(handle, resolver).await
    }

    /// Ensure `initialize` + `tools/list` have run for `server` and are cached
    /// for this snapshot generation. Idempotent within a generation.
    pub async fn ensure_discovered(&self, server: &str) -> Result<(), McpError> {
        self.ensure_discovered_with_resolver(server, &SystemMcpDnsResolver)
            .await
    }

    pub async fn ensure_discovered_with_resolver(
        &self,
        server: &str,
        resolver: &dyn McpDnsResolver,
    ) -> Result<(), McpError> {
        {
            let discovered = self.discovered.lock().await;
            if discovered.contains(server) {
                return Ok(());
            }
        }

        self.initialize_server_with_resolver(server, resolver)
            .await?;

        let plugin_name = self
            .descriptors
            .servers
            .iter()
            .find(|spec| spec.server_name == server)
            .map(|spec| spec.plugin_name.clone())
            .unwrap_or_else(|| "unknown".into());

        let tools = {
            let mut servers = self.servers.lock().await;
            let handle = servers
                .get_mut(server)
                .ok_or_else(|| McpError::NotRunning(server.to_owned()))?;
            list_all_tools(handle, resolver).await?
        };

        let mut discovered = self.discovered.lock().await;
        if discovered.contains(server) {
            return Ok(());
        }
        {
            let mut cache_guard = self
                .cache
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = Arc::make_mut(&mut *cache_guard);
            let filtered: Vec<Value> = match &self.descriptors.allowed_tools {
                None => tools,
                Some(allow) => tools
                    .into_iter()
                    .filter(|tool| {
                        tool.get("name")
                            .and_then(|name| name.as_str())
                            .map(|name| allow.contains(&qualify_tool(server, name)))
                            .unwrap_or(false)
                    })
                    .collect(),
            };
            cache.ingest_server_tools(server, &plugin_name, &filtered);
        }
        discovered.insert(server.to_owned());
        Ok(())
    }

    /// Lookup by qualified wire name (`server__tool`) or `server/tool` parts.
    pub fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor> {
        let cache = self
            .cache
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let found = cache.lookup(qualified_or_parts).cloned()?;
        if self.descriptors.allows_tool(&found.qualified_name) {
            Some(found)
        } else {
            None
        }
    }

    fn deny_if_tool_outside_ceiling(&self, server: &str, tool_name: &str) -> Result<(), McpError> {
        let qualified = qualify_tool(server, tool_name);
        if self.descriptors.allows_tool(&qualified) {
            Ok(())
        } else {
            Err(McpError::CapabilityDenied(qualified))
        }
    }

    pub async fn health(&self, server_name: &str) -> Result<bool, McpError> {
        let mut servers = self.servers.lock().await;
        let Some(handle) = servers.get_mut(server_name) else {
            return Ok(false);
        };
        lifecycle::health(handle).await
    }

    pub async fn mark_unhealthy_and_reap(&self, server_name: &str) -> Result<(), McpError> {
        let mut servers = self.servers.lock().await;
        let Some(handle) = servers.remove(server_name) else {
            return Ok(());
        };
        {
            let mut discovered = self.discovered.lock().await;
            discovered.remove(server_name);
        }
        let _ =
            lifecycle::shutdown_server(handle, Instant::now() + std::time::Duration::from_secs(1))
                .await;
        Ok(())
    }

    /// Discover every configured server for this generation. Per-server failures
    /// are isolated — one unhealthy server does not block the rest.
    pub async fn ensure_all_discovered(&self) -> Result<(), McpError> {
        self.ensure_all_discovered_with_resolver(&SystemMcpDnsResolver)
            .await
    }

    pub async fn ensure_all_discovered_with_resolver(
        &self,
        resolver: &dyn McpDnsResolver,
    ) -> Result<(), McpError> {
        let names: Vec<String> = self
            .descriptors
            .servers
            .iter()
            .map(|server| server.server_name.clone())
            .collect();
        let mut first_error: Option<McpError> = None;
        for name in names {
            if let Err(error) = self.ensure_discovered_with_resolver(&name, resolver).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) if self.cache().is_empty() => Err(error),
            _ => Ok(()),
        }
    }

    /// Transport-level `tools/call`.
    ///
    /// **Not model-facing.** Callers (the `use_tool` / direct-expand Tool impls)
    /// must only invoke this after `ToolRuntime` has issued an execution grant.
    /// Do not route agent/model results around the ToolRuntime membrane.
    pub async fn call_tool(
        &self,
        server: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value, McpError> {
        self.call_tool_with_resolver(server, tool_name, arguments, &SystemMcpDnsResolver)
            .await
    }

    pub async fn call_tool_with_resolver(
        &self,
        server: &str,
        tool_name: &str,
        arguments: Value,
        resolver: &dyn McpDnsResolver,
    ) -> Result<Value, McpError> {
        self.deny_if_tool_outside_ceiling(server, tool_name)?;
        self.ensure_discovered_with_resolver(server, resolver)
            .await?;
        let mut servers = self.servers.lock().await;
        let handle = servers
            .get_mut(server)
            .ok_or_else(|| McpError::NotRunning(server.to_owned()))?;
        let params = Some(tools_call_params(tool_name, arguments));
        lifecycle::rpc_with_resolver(handle, "tools/call", params, resolver).await
    }

    /// SessionEnd-friendly bounded shutdown of every held server.
    pub async fn shutdown_all(&self, deadline: Instant) -> Result<(), McpError> {
        let mut servers = self.servers.lock().await;
        let handles: Vec<_> = servers.drain().map(|(_, handle)| handle).collect();
        drop(servers);
        {
            let mut discovered = self.discovered.lock().await;
            discovered.clear();
        }
        for handle in handles {
            let _ = lifecycle::shutdown_server(handle, deadline).await;
        }
        self.cancel.cancel();
        Ok(())
    }

    pub async fn running_servers(&self) -> Vec<String> {
        self.servers.lock().await.keys().cloned().collect()
    }
}

async fn list_all_tools(
    handle: &mut McpServerHandle,
    resolver: &dyn McpDnsResolver,
) -> Result<Vec<Value>, McpError> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_TOOLS_LIST_PAGES {
        let params = Some(tools_list_params(cursor.as_deref()));
        let result = lifecycle::rpc_with_resolver(handle, "tools/list", params, resolver).await?;
        let (page, next) = parse_tools_list_result(result)?;
        all.extend(page);
        match next {
            Some(next_cursor) => cursor = Some(next_cursor),
            None => return Ok(all),
        }
    }
    Err(McpError::protocol(format!(
        "tools/list exceeded {MAX_TOOLS_LIST_PAGES} pages"
    )))
}
