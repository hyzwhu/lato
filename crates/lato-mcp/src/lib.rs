pub mod config;
pub mod discovery;
pub mod error;
pub mod lifecycle;
pub mod manager;
pub mod names;
pub mod protocol;
pub mod registry;
pub mod transport;

pub use config::{
    DEFAULT_TIMEOUT_MS, MAX_ARGS, MAX_ENV_ENTRIES, MAX_HEADER_ENTRIES, MAX_MCP_DIAGNOSTIC_BYTES,
    MAX_MCP_DIAGNOSTICS, MAX_SERVERS_PER_PLUGIN, MAX_TIMEOUT_MS, McpDescriptorSet, McpDiagnostic,
    McpServerSpec, McpTransportKind, ParseContext, parse_mcp_config, push_diagnostic, reserved_env,
};
pub use discovery::{MAX_SEARCH_MATCHES, McpSearchHit, search_tools};
pub use error::McpError;
pub use lifecycle::{
    McpServerHandle, TransportSession, health, initialize, initialize_with_resolver, rpc,
    rpc_with_resolver, shutdown_server, start_server, start_server_with_resolver,
    stdio_residue_alive,
};
pub use manager::McpManager;
pub use names::{MAX_SERVER_NAME_LEN, normalize_server_name, qualify_tool};
pub use protocol::{InitializeResult, PROTOCOL_VERSION, ServerInfo, tools_call_params};
pub use registry::{
    MAX_TOOL_DESCRIPTION_BYTES, MAX_TOOL_NAME_LEN, MAX_TOOLS_LIST_PAGES, MAX_TOOLS_PER_SERVER,
    McpSchemaCache, McpToolDescriptor, parse_tools_list_result, tools_list_params,
};
pub use transport::{
    HttpSession, McpDnsResolver, StdioSession, SystemMcpDnsResolver, build_mcp_http_client,
    pid_alive, process_group_alive, redact_url_credentials, validate_mcp_url,
};
