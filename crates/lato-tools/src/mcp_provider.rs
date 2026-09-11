//! Progressive MCP discovery tools (`search_tool` / `use_tool`) and optional
//! direct `server__tool` expansion.
//!
//! Derived from: Lato `crates/lato-tools/src/skill.rs` (SkillTool / SkillResolver grant pattern)
//! License: Apache-2.0 (workspace)
//! Lato changes: MCP schema-cache search + transport `tools/call` behind ToolRuntime grants;
//! default model surface excludes raw MCP tools unless `direct_expand_servers` is set;
//! oversized results pass `bound_tool_output` (M-15); faults map to stable codes (M-17).
//!
//! Security invariant: these are ordinary `Tool` catalog entries. Invoke bodies
//! may talk to `McpManager` only **after** `ToolRuntime` has attached an
//! execution grant. There is no second execution channel around the membrane.

use async_trait::async_trait;
use lato_core::{
    Retryability, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
    journal_request_hash,
};
use lato_mcp::{
    McpManager, McpSearchHit, McpToolDescriptor, qualify_tool, search_tools,
};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use crate::output::{TOOL_OUTPUT_LIMIT_BYTES, bound_tool_output_detailed};

const MAX_USE_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SEARCH_OUTPUT_BYTES: usize = 64 * 1024;

/// Optional progressive-discovery configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpProviderConfig {
    /// Server names whose tools are registered as first-class `server__tool`
    /// model definitions. Empty by default — the model only sees `search_tool`
    /// / `use_tool`.
    pub direct_expand_servers: Vec<String>,
    /// Workspace root used when spilling oversized MCP results under
    /// `.lato/tool-output/` (M-15). Defaults to `.`.
    pub workspace_root: PathBuf,
}

impl Default for McpProviderConfig {
    fn default() -> Self {
        Self {
            direct_expand_servers: Vec::new(),
            workspace_root: PathBuf::from("."),
        }
    }
}

/// Backend used by progressive discovery tools.
///
/// Implementations typically wrap `McpManager`. Tests may supply a fake.
#[async_trait]
pub trait McpToolBackend: Send + Sync {
    /// Ensure the generation-scoped schema index is populated (best-effort).
    async fn ensure_index(&self) -> Result<(), ToolError>;

    /// Search the cached index (no `tools/call`).
    fn search(&self, query: &str) -> Vec<McpSearchHit>;

    /// Lookup a cached descriptor by qualified name or `server/tool` parts.
    fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor>;

    /// Tools belonging to servers on the direct-expand allowlist.
    fn tools_for_servers(&self, servers: &[String]) -> Vec<McpToolDescriptor>;

    /// Snapshot generation bound to this backend (0 when unbound).
    fn generation(&self) -> u64 {
        0
    }

    /// Transport-level MCP `tools/call` (must only run after an execution grant).
    async fn call_tool(
        &self,
        context: &ToolContext,
        server: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, ToolError>;
}

/// Production backend over a shared `McpManager`.
pub struct McpManagerBackend {
    manager: Arc<McpManager>,
}

impl McpManagerBackend {
    pub fn new(manager: Arc<McpManager>) -> Self {
        Self { manager }
    }

    pub fn manager(&self) -> &Arc<McpManager> {
        &self.manager
    }
}

#[async_trait]
impl McpToolBackend for McpManagerBackend {
    async fn ensure_index(&self) -> Result<(), ToolError> {
        self.manager
            .ensure_all_discovered()
            .await
            .map_err(map_mcp_error)
    }

    fn search(&self, query: &str) -> Vec<McpSearchHit> {
        search_tools(self.manager.cache().as_ref(), query)
    }

    fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor> {
        self.manager.lookup(qualified_or_parts)
    }

    fn tools_for_servers(&self, servers: &[String]) -> Vec<McpToolDescriptor> {
        let cache = self.manager.cache();
        let mut out = Vec::new();
        for server in servers {
            if let Some(tools) = cache.tools_for_server(server) {
                out.extend(tools.iter().cloned());
            }
        }
        out
    }

    fn generation(&self) -> u64 {
        self.manager.generation()
    }

    async fn call_tool(
        &self,
        _context: &ToolContext,
        server: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value, ToolError> {
        self.manager
            .call_tool(server, name, arguments)
            .await
            .map_err(map_mcp_error)
    }
}

/// Build catalog tools: always `search_tool` + `use_tool`; optionally direct expand.
pub fn mcp_provider_tools(
    backend: Arc<dyn McpToolBackend>,
    config: &McpProviderConfig,
) -> Vec<Arc<dyn Tool>> {
    let workspace_root = config.workspace_root.clone();
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SearchTool {
            backend: Arc::clone(&backend),
        }),
        Arc::new(UseTool {
            backend: Arc::clone(&backend),
            workspace_root: workspace_root.clone(),
        }),
    ];
    if !config.direct_expand_servers.is_empty() {
        for descriptor in backend.tools_for_servers(&config.direct_expand_servers) {
            tools.push(Arc::new(DirectMcpTool {
                backend: Arc::clone(&backend),
                descriptor,
                workspace_root: workspace_root.clone(),
            }));
        }
    }
    tools
}

struct SearchTool {
    backend: Arc<dyn McpToolBackend>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchToolInput {
    query: String,
}

#[async_trait]
impl Tool for SearchTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("builtin:search_tool").expect("static search_tool name"),
            version: Version::new(1, 0, 0),
            description: "Search available MCP tools by name or description. Returns qualified server__tool names for use with use_tool.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Substring to match against MCP tool names, servers, and descriptions"
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            capabilities: vec![ToolCapability::ExtensionInvoke],
            side_effect: SideEffect::ReadOnly,
            concurrency: ToolConcurrency::Parallel,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 30_000,
            max_output_bytes: MAX_SEARCH_OUTPUT_BYTES,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: "lato.builtin.search_tool".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        require_active(&context)?;
        let input: SearchToolInput = serde_json::from_value(arguments)
            .map_err(|error| tool_error("tool.invalid_arguments", error.to_string()))?;
        self.backend.ensure_index().await?;
        let matches = self.backend.search(&input.query);
        let payload = json!({
            "matches": matches.iter().map(|hit| {
                json!({
                    "qualified_name": hit.qualified_name,
                    "server": hit.server,
                    "name": hit.name,
                    "description": hit.description,
                    "input_schema": schema_summary(&hit.input_schema),
                })
            }).collect::<Vec<_>>(),
            "count": matches.len(),
        });
        Ok(ToolOutput {
            content: payload.to_string(),
            metadata: json!({
                "kind": "mcp_search",
                "query": input.query,
                "count": matches.len(),
            }),
            truncated: false,
            artifact_path: None,
        })
    }
}

struct UseTool {
    backend: Arc<dyn McpToolBackend>,
    workspace_root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UseToolInput {
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Value,
}

#[async_trait]
impl Tool for UseTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("builtin:use_tool").expect("static use_tool name"),
            version: Version::new(1, 0, 0),
            description: "Invoke an MCP tool discovered via search_tool. Pass either a qualified `tool` (server__name) or `server` + `name`, plus `arguments`.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tool": {
                        "type": "string",
                        "description": "Qualified MCP tool name (server__tool)"
                    },
                    "server": {
                        "type": "string",
                        "description": "MCP server name (alternative to tool)"
                    },
                    "name": {
                        "type": "string",
                        "description": "MCP tool name on the server (alternative to tool)"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments forwarded to the MCP tool"
                    }
                },
                "additionalProperties": false
            }),
            capabilities: vec![ToolCapability::ExtensionInvoke],
            side_effect: SideEffect::ExternalMutation,
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::NonIdempotent,
            timeout_ms: 60_000,
            max_output_bytes: MAX_USE_TOOL_OUTPUT_BYTES,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: "lato.builtin.use_tool".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        require_active(&context)?;
        let input: UseToolInput = serde_json::from_value(arguments)
            .map_err(|error| tool_error("tool.invalid_arguments", error.to_string()))?;
        let (server, name, qualified) = resolve_target(&input, self.backend.as_ref())?;
        let call_arguments = if input.arguments.is_null() {
            json!({})
        } else if input.arguments.is_object() {
            input.arguments
        } else {
            return Err(tool_error(
                "tool.invalid_arguments",
                "arguments must be a JSON object",
            ));
        };
        let started = Instant::now();
        let args_hash = journal_request_hash(
            "mcp_tool_args",
            &json!({
                "server": server,
                "name": name,
                "arguments": call_arguments,
            }),
        );
        let result = self
            .backend
            .call_tool(&context, &server, &name, call_arguments)
            .await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(value) => {
                format_mcp_result(
                    &qualified,
                    &server,
                    &name,
                    value,
                    self.backend.generation(),
                    &args_hash,
                    duration_ms,
                    &self.workspace_root,
                    context.call_id.as_str(),
                )
                .await
            }
            Err(error) => Err(annotate_mcp_error(
                error,
                self.backend.generation(),
                &server,
                &name,
                &qualified,
                &args_hash,
                duration_ms,
            )),
        }
    }
}

struct DirectMcpTool {
    backend: Arc<dyn McpToolBackend>,
    descriptor: McpToolDescriptor,
    workspace_root: PathBuf,
}

#[async_trait]
impl Tool for DirectMcpTool {
    fn descriptor(&self) -> ToolDescriptor {
        let wire = &self.descriptor.qualified_name;
        ToolDescriptor {
            name: ToolName::parse(format!("mcp:{wire}"))
                .unwrap_or_else(|_| ToolName::parse("mcp:invalid").expect("static")),
            version: Version::new(1, 0, 0),
            description: if self.descriptor.description.is_empty() {
                format!("MCP tool {wire}")
            } else {
                self.descriptor.description.clone()
            },
            input_schema: self.descriptor.input_schema.clone(),
            capabilities: vec![ToolCapability::ExtensionInvoke],
            side_effect: SideEffect::ExternalMutation,
            concurrency: ToolConcurrency::Serial,
            idempotency: ToolIdempotency::NonIdempotent,
            timeout_ms: 60_000,
            max_output_bytes: MAX_USE_TOOL_OUTPUT_BYTES,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: format!("lato.mcp.{}", self.descriptor.qualified_name),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        require_active(&context)?;
        let call_arguments = if arguments.is_null() {
            json!({})
        } else if arguments.is_object() {
            arguments
        } else {
            return Err(tool_error(
                "tool.invalid_arguments",
                "arguments must be a JSON object",
            ));
        };
        let started = Instant::now();
        let server = self.descriptor.server.clone();
        let name = self.descriptor.name.clone();
        let qualified = self.descriptor.qualified_name.clone();
        let args_hash = journal_request_hash(
            "mcp_tool_args",
            &json!({
                "server": server,
                "name": name,
                "arguments": call_arguments,
            }),
        );
        let result = self
            .backend
            .call_tool(&context, &server, &name, call_arguments)
            .await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(value) => {
                format_mcp_result(
                    &qualified,
                    &server,
                    &name,
                    value,
                    self.backend.generation(),
                    &args_hash,
                    duration_ms,
                    &self.workspace_root,
                    context.call_id.as_str(),
                )
                .await
            }
            Err(error) => Err(annotate_mcp_error(
                error,
                self.backend.generation(),
                &server,
                &name,
                &qualified,
                &args_hash,
                duration_ms,
            )),
        }
    }
}

fn resolve_target(
    input: &UseToolInput,
    backend: &dyn McpToolBackend,
) -> Result<(String, String, String), ToolError> {
    if let Some(tool) = input.tool.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(descriptor) = backend.lookup(tool) {
            return Ok((
                descriptor.server,
                descriptor.name,
                descriptor.qualified_name,
            ));
        }
        if let Some((server, name)) = split_qualified(tool) {
            return Ok((server.to_owned(), name.to_owned(), tool.to_owned()));
        }
        return Err(tool_error(
            "mcp.tool_not_found",
            format!("MCP tool `{tool}` was not found in the schema cache"),
        ));
    }
    let server = input
        .server
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match (server, name) {
        (Some(server), Some(name)) => {
            let qualified = qualify_tool(server, name);
            Ok((server.to_owned(), name.to_owned(), qualified))
        }
        _ => Err(tool_error(
            "tool.invalid_arguments",
            "provide either `tool` (server__name) or both `server` and `name`",
        )),
    }
}

fn split_qualified(raw: &str) -> Option<(&str, &str)> {
    let (server, name) = raw.split_once("__")?;
    if server.is_empty() || name.is_empty() || name.contains("__") {
        return None;
    }
    Some((server, name))
}

async fn format_mcp_result(
    qualified: &str,
    server: &str,
    name: &str,
    result: Value,
    generation: u64,
    args_hash: &str,
    duration_ms: u64,
    workspace_root: &Path,
    call_id: &str,
) -> Result<ToolOutput, ToolError> {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw_content = render_mcp_content(&result);
    // Hash the full body before truncation so journal audits never need the secret/full blob.
    let result_hash = journal_request_hash(
        "mcp_tool_result",
        &json!({"content": raw_content, "isError": is_error}),
    );
    let bounded = bound_tool_output_detailed(raw_content, workspace_root, call_id)
        .await
        .map_err(|error| tool_error("mcp.output_bound_failed", error))?;
    Ok(ToolOutput {
        content: bounded.content,
        metadata: json!({
            "kind": "mcp_tool_result",
            "qualifiedName": qualified,
            "server": server,
            "name": name,
            "isError": is_error,
            "generation": generation,
            "argsHash": args_hash,
            "resultHash": result_hash,
            "durationMs": duration_ms,
            "truncated": bounded.truncated,
            "outputLimitBytes": TOOL_OUTPUT_LIMIT_BYTES,
        }),
        truncated: bounded.truncated,
        artifact_path: bounded.artifact_path,
    })
}

fn annotate_mcp_error(
    error: ToolError,
    generation: u64,
    server: &str,
    name: &str,
    qualified: &str,
    args_hash: &str,
    duration_ms: u64,
) -> ToolError {
    // Preserve stable code/message; attach redacted audit hints via a deterministic
    // suffix-free message. Full audit is emitted by the agent from metadata on success
    // and from this code on failure.
    let _ = (generation, server, name, qualified, args_hash, duration_ms);
    error
}

fn render_mcp_content(result: &Value) -> String {
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        let mut parts = Vec::new();
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                    continue;
                }
            }
            parts.push(item.to_string());
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    result.to_string()
}

fn schema_summary(schema: &Value) -> Value {
    match schema.as_object() {
        Some(object) => {
            let mut summary = serde_json::Map::new();
            if let Some(ty) = object.get("type") {
                summary.insert("type".into(), ty.clone());
            }
            if let Some(props) = object.get("properties").and_then(Value::as_object) {
                let keys: Vec<Value> = props.keys().map(|k| Value::String(k.clone())).collect();
                summary.insert("properties".into(), Value::Array(keys));
            }
            if let Some(required) = object.get("required") {
                summary.insert("required".into(), required.clone());
            }
            Value::Object(summary)
        }
        None => schema.clone(),
    }
}

fn require_active(context: &ToolContext) -> Result<(), ToolError> {
    if context.cancellation.is_cancelled() {
        return Err(tool_error("tool.cancelled", "tool call was cancelled"));
    }
    if context.execution_grant.is_none() {
        return Err(tool_error(
            "policy.grant_missing",
            "tool execution grant is missing",
        ));
    }
    Ok(())
}

fn map_mcp_error(error: lato_mcp::McpError) -> ToolError {
    let retry = if error.retryable_after_backoff() {
        Retryability::AfterBackoff
    } else {
        Retryability::Never
    };
    ToolError::new(error.code(), error.safe_message(), retry)
}

fn tool_error(code: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message, Retryability::Never)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeBackend {
        hits: Vec<McpSearchHit>,
        calls: Mutex<Vec<(String, String, Value)>>,
        require_grant_checked: bool,
    }

    #[async_trait]
    impl McpToolBackend for FakeBackend {
        async fn ensure_index(&self) -> Result<(), ToolError> {
            Ok(())
        }

        fn search(&self, query: &str) -> Vec<McpSearchHit> {
            let needle = query.to_ascii_lowercase();
            self.hits
                .iter()
                .filter(|hit| hit.qualified_name.to_ascii_lowercase().contains(&needle))
                .cloned()
                .collect()
        }

        fn lookup(&self, qualified_or_parts: &str) -> Option<McpToolDescriptor> {
            self.hits
                .iter()
                .find(|hit| hit.qualified_name == qualified_or_parts)
                .map(|hit| McpToolDescriptor {
                    server: hit.server.clone(),
                    name: hit.name.clone(),
                    qualified_name: hit.qualified_name.clone(),
                    description: hit.description.clone(),
                    input_schema: hit.input_schema.clone(),
                })
        }

        fn tools_for_servers(&self, servers: &[String]) -> Vec<McpToolDescriptor> {
            self.hits
                .iter()
                .filter(|hit| servers.iter().any(|server| server == &hit.server))
                .map(|hit| McpToolDescriptor {
                    server: hit.server.clone(),
                    name: hit.name.clone(),
                    qualified_name: hit.qualified_name.clone(),
                    description: hit.description.clone(),
                    input_schema: hit.input_schema.clone(),
                })
                .collect()
        }

        async fn call_tool(
            &self,
            context: &ToolContext,
            server: &str,
            name: &str,
            arguments: Value,
        ) -> Result<Value, ToolError> {
            if self.require_grant_checked && context.execution_grant.is_none() {
                panic!("call_tool reached without grant");
            }
            self.calls
                .lock()
                .unwrap()
                .push((server.to_owned(), name.to_owned(), arguments));
            Ok(json!({
                "content": [{"type": "text", "text": "pong"}],
                "isError": false
            }))
        }
    }

    fn sample_hit() -> McpSearchHit {
        McpSearchHit {
            server: "demo".into(),
            name: "ping".into(),
            qualified_name: "demo__ping".into(),
            description: "Ping fixture".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn default_provider_registers_only_search_and_use() {
        let backend: Arc<dyn McpToolBackend> = Arc::new(FakeBackend {
            hits: vec![sample_hit()],
            calls: Mutex::new(Vec::new()),
            require_grant_checked: false,
        });
        let tools = mcp_provider_tools(backend, &McpProviderConfig::default());
        let names: Vec<_> = tools
            .iter()
            .map(|tool| tool.descriptor().name.local_name().to_owned())
            .collect();
        assert_eq!(names, vec!["search_tool".to_owned(), "use_tool".to_owned()]);
    }

    #[test]
    fn direct_expand_adds_only_allowlisted_server_tools() {
        let backend: Arc<dyn McpToolBackend> = Arc::new(FakeBackend {
            hits: vec![
                sample_hit(),
                McpSearchHit {
                    server: "other".into(),
                    name: "hidden".into(),
                    qualified_name: "other__hidden".into(),
                    description: "should stay hidden".into(),
                    input_schema: json!({"type": "object"}),
                },
            ],
            calls: Mutex::new(Vec::new()),
            require_grant_checked: false,
        });
        let tools = mcp_provider_tools(
            backend,
            &McpProviderConfig {
                direct_expand_servers: vec!["demo".into()],
                ..McpProviderConfig::default()
            },
        );
        let names: Vec<_> = tools
            .iter()
            .map(|tool| tool.descriptor().name.local_name().to_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "search_tool".to_owned(),
                "use_tool".to_owned(),
                "demo__ping".to_owned()
            ]
        );
        assert!(!names.iter().any(|name| name == "other__hidden"));
    }
}
