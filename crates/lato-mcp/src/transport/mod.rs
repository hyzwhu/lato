//! MCP transports: persistent stdio sessions and hardened streamable HTTP.
//!
//! Derived from: Lato hooks command/HTTP runners (process-group reap, SSRF classes,
//! redirect::Policy::none) — see docs/superpowers/reference/lato-upstream-sources.md.

pub mod http;
pub mod stdio;

pub use http::{
    HttpSession, McpDnsResolver, SystemMcpDnsResolver, build_mcp_http_client, redact_url_credentials,
    validate_mcp_url,
};
pub use stdio::{StdioSession, pid_alive, process_group_alive};
