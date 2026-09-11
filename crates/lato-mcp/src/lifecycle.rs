//! Bounded MCP server lifecycle: start, initialize, health, cancel, reap, shutdown.
//!
//! Derived from: Lato hooks SessionEnd / process-tree reap patterns.
//! License: Apache-2.0 (workspace)
//! Lato changes: generation-tagged handles over stdio + streamable HTTP; no ToolRuntime bypass.

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{McpServerSpec, McpTransportKind},
    error::McpError,
    protocol::{self, InitializeResult},
    transport::{
        HttpSession, McpDnsResolver, StdioSession, SystemMcpDnsResolver, pid_alive,
        process_group_alive,
    },
};

pub enum TransportSession {
    Stdio(StdioSession),
    Http(HttpSession),
}

pub struct McpServerHandle {
    pub generation: u64,
    pub server_id: String,
    pub server_name: String,
    pub transport_kind: McpTransportKind,
    pub timeout_ms: u64,
    session: TransportSession,
    initialize_result: Option<InitializeResult>,
    unhealthy: bool,
}

impl McpServerHandle {
    pub fn initialize_result(&self) -> Option<&InitializeResult> {
        self.initialize_result.as_ref()
    }

    pub fn is_unhealthy(&self) -> bool {
        self.unhealthy
    }

    pub fn stdio_pid(&self) -> Option<u32> {
        match &self.session {
            TransportSession::Stdio(session) => Some(session.pid()),
            TransportSession::Http(_) => None,
        }
    }

    pub fn child_alive(&mut self) -> bool {
        match &mut self.session {
            TransportSession::Stdio(session) => session.is_alive(),
            TransportSession::Http(_) => !self.unhealthy,
        }
    }
}

pub async fn start_server(
    spec: &McpServerSpec,
    generation: u64,
    cancel: CancellationToken,
) -> Result<McpServerHandle, McpError> {
    start_server_with_resolver(spec, generation, cancel, &SystemMcpDnsResolver).await
}

pub async fn start_server_with_resolver(
    spec: &McpServerSpec,
    generation: u64,
    cancel: CancellationToken,
    resolver: &dyn McpDnsResolver,
) -> Result<McpServerHandle, McpError> {
    let session_cancel = cancel.child_token();
    let session = match spec.transport {
        McpTransportKind::Stdio => {
            TransportSession::Stdio(StdioSession::start(spec, session_cancel).await?)
        }
        McpTransportKind::StreamableHttp => {
            TransportSession::Http(HttpSession::start(spec, session_cancel, resolver).await?)
        }
    };
    drop(cancel);
    Ok(McpServerHandle {
        generation,
        server_id: spec.id.clone(),
        server_name: spec.server_name.clone(),
        transport_kind: spec.transport,
        timeout_ms: spec.timeout_ms,
        session,
        initialize_result: None,
        unhealthy: false,
    })
}

pub async fn initialize(handle: &mut McpServerHandle) -> Result<InitializeResult, McpError> {
    initialize_with_resolver(handle, &SystemMcpDnsResolver).await
}

pub async fn initialize_with_resolver(
    handle: &mut McpServerHandle,
    resolver: &dyn McpDnsResolver,
) -> Result<InitializeResult, McpError> {
    if handle.unhealthy {
        return Err(McpError::Unhealthy);
    }
    let timeout = Duration::from_millis(handle.timeout_ms);
    let result_value = match request_raw(
        handle,
        "initialize",
        Some(protocol::initialize_params()),
        timeout,
        resolver,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            mark_transport_fault(handle, &error).await;
            return Err(error);
        }
    };
    let parsed = match protocol::parse_initialize_result(result_value) {
        Ok(parsed) => parsed,
        Err(error) => {
            handle.unhealthy = true;
            return Err(error);
        }
    };
    if let Err(error) = notify_raw(
        handle,
        "notifications/initialized",
        Some(serde_json::json!({})),
        timeout,
        resolver,
    )
    .await
    {
        mark_transport_fault(handle, &error).await;
        return Err(error);
    }
    handle.initialize_result = Some(parsed.clone());
    Ok(parsed)
}

pub async fn health(handle: &mut McpServerHandle) -> Result<bool, McpError> {
    if handle.unhealthy {
        return Ok(false);
    }
    match &mut handle.session {
        TransportSession::Stdio(session) => {
            let alive = session.is_alive();
            if !alive {
                handle.unhealthy = true;
            }
            Ok(alive)
        }
        TransportSession::Http(_) => Ok(handle.initialize_result.is_some()),
    }
}

pub async fn rpc(
    handle: &mut McpServerHandle,
    method: &str,
    params: Option<Value>,
) -> Result<Value, McpError> {
    rpc_with_resolver(handle, method, params, &SystemMcpDnsResolver).await
}

pub async fn rpc_with_resolver(
    handle: &mut McpServerHandle,
    method: &str,
    params: Option<Value>,
    resolver: &dyn McpDnsResolver,
) -> Result<Value, McpError> {
    if handle.unhealthy {
        return Err(McpError::Unhealthy);
    }
    let timeout = Duration::from_millis(handle.timeout_ms);
    match request_raw(handle, method, params, timeout, resolver).await {
        Ok(value) => Ok(value),
        Err(error) => {
            mark_transport_fault(handle, &error).await;
            Err(error)
        }
    }
}

pub async fn shutdown_server(
    handle: McpServerHandle,
    deadline: Instant,
) -> Result<(), McpError> {
    match handle.session {
        TransportSession::Stdio(session) => session.shutdown(deadline).await,
        TransportSession::Http(session) => {
            session.shutdown();
            let _ = deadline;
            Ok(())
        }
    }
}

async fn request_raw(
    handle: &mut McpServerHandle,
    method: &str,
    params: Option<Value>,
    timeout: Duration,
    resolver: &dyn McpDnsResolver,
) -> Result<Value, McpError> {
    match &mut handle.session {
        TransportSession::Stdio(session) => session.request(method, params, timeout).await,
        TransportSession::Http(session) => {
            session.request(method, params, timeout, resolver).await
        }
    }
}

async fn notify_raw(
    handle: &mut McpServerHandle,
    method: &str,
    params: Option<Value>,
    timeout: Duration,
    resolver: &dyn McpDnsResolver,
) -> Result<(), McpError> {
    match &mut handle.session {
        TransportSession::Stdio(session) => session.notify(method, params).await,
        TransportSession::Http(session) => {
            session.notify(method, params, timeout, resolver).await
        }
    }
}

async fn mark_transport_fault(handle: &mut McpServerHandle, error: &McpError) {
    match error {
        McpError::Timeout { .. } | McpError::Cancelled | McpError::Io | McpError::Unhealthy => {
            handle.unhealthy = true;
            if let TransportSession::Stdio(session) = &mut handle.session {
                session.kill_now().await;
            }
        }
        McpError::UnsafeUrl | McpError::Http => {
            // HTTP faults isolate the server without process kill.
            handle.unhealthy = true;
        }
        _ => {}
    }
}

/// True when a previously observed stdio pid (or its process group) is still present.
pub fn stdio_residue_alive(pid: u32) -> bool {
    pid_alive(pid) || process_group_alive(pid)
}
