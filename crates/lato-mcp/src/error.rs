//! MCP runtime error types with stable codes for ToolError mapping.
//!
//! Protocol / timeout / SSRF failures map to stable `mcp.*` codes (M-17).
//! User-visible messages never include URL credentials, headers, or env values.

use thiserror::Error;

/// Soft cap on RPC/protocol message fragments surfaced to callers.
const MAX_SAFE_MESSAGE_BYTES: usize = 512;

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
    #[error("MCP capability ceiling denies {0}")]
    CapabilityDenied(String),
}

impl McpError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: truncate_safe(message.into()),
        }
    }

    pub fn rpc(code: i64, message: impl Into<String>) -> Self {
        Self::Rpc {
            code,
            message: truncate_safe(message.into()),
        }
    }

    /// Stable error code for ToolError / journal mapping (M-17).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfiguration(_) => "mcp.invalid_configuration",
            Self::Spawn => "mcp.spawn",
            Self::Io => "mcp.io",
            Self::Timeout { .. } => "mcp.timeout",
            Self::Cancelled => "tool.cancelled",
            Self::UnsafeUrl => "mcp.unsafe_url",
            Self::Http => "mcp.http",
            Self::Protocol { .. } => "mcp.protocol",
            Self::Rpc { .. } => "mcp.rpc_error",
            Self::Unhealthy => "mcp.unhealthy",
            Self::NotRunning(_) => "mcp.not_running",
            Self::ShutDown => "mcp.shutdown",
            Self::CapabilityDenied(_) => "mcp.capability_denied",
        }
    }

    /// Whether a failed call may be retried after backoff.
    pub fn retryable_after_backoff(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. }
                | Self::NotRunning(_)
                | Self::Unhealthy
                | Self::Io
                | Self::Http
                | Self::Spawn
        )
    }

    /// User-visible message that never echoes credentials or raw secrets.
    pub fn safe_message(&self) -> String {
        match self {
            Self::InvalidConfiguration(message) => {
                format!(
                    "MCP server configuration is invalid: {}",
                    truncate_safe(message.clone())
                )
            }
            Self::Spawn => "MCP server could not be started".into(),
            Self::Io => "MCP transport I/O failed".into(),
            Self::Timeout { timeout_ms } => format!("MCP timed out after {timeout_ms}ms"),
            Self::Cancelled => "MCP operation was cancelled".into(),
            Self::UnsafeUrl => "MCP URL is not allowed".into(),
            Self::Http => "MCP HTTP request failed".into(),
            Self::Protocol { message } => {
                format!("MCP protocol error: {}", truncate_safe(message.clone()))
            }
            Self::Rpc { code, message } => {
                format!(
                    "MCP JSON-RPC error {code}: {}",
                    truncate_safe(message.clone())
                )
            }
            Self::Unhealthy => "MCP server is unhealthy".into(),
            Self::NotRunning(name) => format!("MCP server {name} is not running"),
            Self::ShutDown => "MCP server already shut down".into(),
            Self::CapabilityDenied(name) => {
                format!(
                    "MCP capability ceiling denies {}",
                    truncate_safe(name.clone())
                )
            }
        }
    }
}

fn truncate_safe(mut message: String) -> String {
    if message.len() <= MAX_SAFE_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_SAFE_MESSAGE_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push('…');
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_codes_cover_protocol_timeout_and_ssrf() {
        assert_eq!(McpError::Timeout { timeout_ms: 1 }.code(), "mcp.timeout");
        assert_eq!(McpError::protocol("x").code(), "mcp.protocol");
        assert_eq!(McpError::rpc(-32600, "bad").code(), "mcp.rpc_error");
        assert_eq!(McpError::UnsafeUrl.code(), "mcp.unsafe_url");
        assert_eq!(McpError::Cancelled.code(), "tool.cancelled");
        assert_eq!(McpError::Http.code(), "mcp.http");
    }

    #[test]
    fn safe_messages_never_echo_credentials() {
        let err = McpError::UnsafeUrl;
        let message = err.safe_message();
        assert!(!message.contains("://"));
        assert!(!message.contains("secret"));
        assert_eq!(message, "MCP URL is not allowed");
    }

    #[test]
    fn protocol_messages_are_bounded() {
        let huge = "s".repeat(2_000);
        let err = McpError::protocol(huge);
        match &err {
            McpError::Protocol { message } => assert!(message.len() <= MAX_SAFE_MESSAGE_BYTES + 4),
            other => panic!("unexpected {other:?}"),
        }
        assert!(err.safe_message().len() < 600);
    }
}
