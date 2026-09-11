//! MCP runtime error types.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("MCP server configuration is invalid: {0}")]
    InvalidConfiguration(String),
    #[error("MCP server could not be started")]
    Spawn,
    #[error("MCP transport I/O failed")]
    Io,
    #[error("MCP timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    #[error("MCP operation was cancelled")]
    Cancelled,
    #[error("MCP URL is not allowed")]
    UnsafeUrl,
    #[error("MCP HTTP request failed")]
    Http,
    #[error("MCP protocol error: {message}")]
    Protocol { message: String },
    #[error("MCP JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("MCP server is unhealthy")]
    Unhealthy,
    #[error("MCP server {0} is not running")]
    NotRunning(String),
    #[error("MCP server already shut down")]
    ShutDown,
}

impl McpError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}
