//! Session-facing MCP manager skeleton.
//!
//! Security invariant: this type owns transport lifecycle only. It must **not**
//! expose a model-facing `tools/call` bypass around `ToolRuntime`. Tool execution
//! channels are registered later via `lato-tools` providers.

use std::{
    collections::HashMap,
    sync::Arc,
    time::Instant,
};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    config::McpDescriptorSet,
    error::McpError,
    lifecycle::{
        self, McpServerHandle, initialize_with_resolver, start_server_with_resolver,
    },
    protocol::InitializeResult,
    transport::{McpDnsResolver, SystemMcpDnsResolver},
};

pub struct McpManager {
    generation: u64,
    descriptors: Arc<McpDescriptorSet>,
    cancel: CancellationToken,
    servers: Mutex<HashMap<String, McpServerHandle>>,
}

impl McpManager {
    pub fn new(descriptors: Arc<McpDescriptorSet>, cancel: CancellationToken) -> Self {
        Self {
            generation: descriptors.generation,
            descriptors,
            cancel,
            servers: Mutex::new(HashMap::new()),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn descriptors(&self) -> Arc<McpDescriptorSet> {
        Arc::clone(&self.descriptors)
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
        let handle = start_server_with_resolver(
            spec,
            self.generation,
            self.cancel.child_token(),
            resolver,
        )
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

    pub async fn initialize_server(
        &self,
        server_name: &str,
    ) -> Result<InitializeResult, McpError> {
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
        let _ = lifecycle::shutdown_server(handle, Instant::now() + std::time::Duration::from_secs(1))
            .await;
        Ok(())
    }

    /// SessionEnd-friendly bounded shutdown of every held server.
    pub async fn shutdown_all(&self, deadline: Instant) -> Result<(), McpError> {
        let mut servers = self.servers.lock().await;
        let handles: Vec<_> = servers.drain().map(|(_, handle)| handle).collect();
        drop(servers);
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
