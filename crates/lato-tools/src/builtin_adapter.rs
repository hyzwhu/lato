use crate::{ToolCall, dispatch, v1_tool_definitions};
use async_trait::async_trait;
use lato_core::{
    ExecutionGrant, Retryability, SandboxObligation, SandboxProfile, SideEffect, Tool,
    ToolCancellation, ToolCapability, ToolConcurrency, ToolContext, ToolDescriptor, ToolError,
    ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use lato_workspace::{ApprovalMode, FileLocks, SessionTrust, deny_write};
use semver::Version;
use serde_json::Value;
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct BuiltinToolEnvironment {
    pub cwd: PathBuf,
    pub locks: Arc<FileLocks>,
    pub trust: SessionTrust,
}

#[derive(Debug, thiserror::Error)]
pub enum BuiltinAdapterError {
    #[error("built-in tool definition is malformed: {0}")]
    InvalidDefinition(String),
    #[error("invalid built-in tool name: {0}")]
    InvalidName(#[from] lato_core::ToolNameError),
}

struct LegacyDispatchTool {
    descriptor: ToolDescriptor,
    legacy_name: String,
    environment: BuiltinToolEnvironment,
}

#[async_trait]
impl Tool for LegacyDispatchTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(tool_error("tool.cancelled", "tool call was cancelled"));
        }
        let content = if self.legacy_name == "write_file" {
            invoke_compat_write_file(
                &self.environment,
                context.execution_grant.as_ref(),
                &arguments,
            )
            .await
        } else {
            dispatch(
                &self.environment.locks,
                &self.environment.trust,
                &self.environment.cwd,
                ToolCall {
                    name: self.legacy_name.clone(),
                    arguments,
                },
            )
            .await
        }
        .map_err(classify_legacy_error)?;
        if context.cancellation.is_cancelled() {
            return Err(tool_error("tool.cancelled", "tool call was cancelled"));
        }
        Ok(ToolOutput {
            content,
            metadata: serde_json::json!({}),
            truncated: false,
            artifact_path: None,
        })
    }
}

async fn invoke_compat_write_file(
    environment: &BuiltinToolEnvironment,
    grant: Option<&ExecutionGrant>,
    arguments: &Value,
) -> Result<String, String> {
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .ok_or("missing path")?;
    let path = PathBuf::from(raw_path);
    let path = if path.is_absolute() {
        path
    } else {
        environment.cwd.join(path)
    };
    if deny_write(&path) {
        return Err("denied by write policy".into());
    }
    let obligation = grant
        .map(|grant| &grant.sandbox)
        .ok_or("denied by policy: execution grant missing")?;
    validate_write_obligation(obligation, &path)?;
    require_compat_mutating_approval(&environment.trust)?;
    let contents = arguments
        .get("contents")
        .or_else(|| arguments.get("content"))
        .and_then(Value::as_str)
        .ok_or("missing contents")?;
    let _guard = environment.locks.acquire(&path).await;
    validate_write_obligation(obligation, &path)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    tokio::fs::write(path, contents)
        .await
        .map_err(|error| error.to_string())?;
    Ok("ok".into())
}

fn validate_write_obligation(obligation: &SandboxObligation, path: &Path) -> Result<(), String> {
    match obligation.profile {
        SandboxProfile::Off => return Ok(()),
        SandboxProfile::ReadOnly => {
            return Err("denied by read-only sandbox policy".into());
        }
        SandboxProfile::Workspace => {}
    }

    let target = lexical_normalize(path)?;
    let workspace = canonical_existing(&obligation.workspace_root)?;
    let target_ancestor = canonical_nearest_existing(&target)?;
    if !target_ancestor.starts_with(&workspace) {
        return Err("denied by workspace sandbox policy".into());
    }

    let allowed = obligation.writable_roots.iter().any(|root| {
        let Ok(lexical_root) = lexical_normalize(root) else {
            return false;
        };
        if !target.starts_with(&lexical_root) {
            return false;
        }
        let Ok(canonical_root) = canonical_nearest_existing(&lexical_root) else {
            return false;
        };
        canonical_root.starts_with(&workspace) && target_ancestor.starts_with(&canonical_root)
    });
    if !allowed {
        return Err("denied by writable-roots sandbox policy".into());
    }
    Ok(())
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("denied by sandbox policy: path is not absolute".into());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err("denied by sandbox policy: path escapes its root".into());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn canonical_existing(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|_| "denied by sandbox policy: root unavailable".into())
}

fn canonical_nearest_existing(path: &Path) -> Result<PathBuf, String> {
    let mut current = path;
    loop {
        match std::fs::canonicalize(current) {
            Ok(canonical) => return Ok(canonical),
            Err(_) => {
                current = current.parent().ok_or_else(|| {
                    "denied by sandbox policy: no existing path ancestor".to_owned()
                })?;
            }
        }
    }
}

fn require_compat_mutating_approval(trust: &SessionTrust) -> Result<(), String> {
    match trust.mode {
        ApprovalMode::Always | ApprovalMode::Auto => Ok(()),
        ApprovalMode::Ask if trust.consume_allow_once() => Ok(()),
        ApprovalMode::Ask => Err("permission required before mutating tool execution".into()),
    }
}

pub fn builtin_tools(
    environment: BuiltinToolEnvironment,
) -> Result<Vec<Arc<dyn Tool>>, BuiltinAdapterError> {
    let definitions = v1_tool_definitions();
    let definitions = definitions
        .as_array()
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition("root must be an array".into()))?;
    let mut definitions = definitions.clone();
    if !definitions.iter().any(|definition| {
        definition.pointer("/function/name").and_then(Value::as_str) == Some("write_file")
    }) {
        definitions.push(write_file_definition());
    }
    definitions
        .iter()
        .map(|definition| adapter_from_definition(definition, environment.clone()))
        .collect()
}

fn write_file_definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Create or overwrite a UTF-8 file in the workspace. Use this to write new files such as hello.go. Prefer this over printing file contents in chat.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path relative to the workspace or absolute"
                    },
                    "contents": {
                        "type": "string",
                        "description": "Full file contents"
                    }
                },
                "required": ["path", "contents"]
            }
        }
    })
}

fn adapter_from_definition(
    definition: &Value,
    environment: BuiltinToolEnvironment,
) -> Result<Arc<dyn Tool>, BuiltinAdapterError> {
    let function = definition
        .get("function")
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition("missing function".into()))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| BuiltinAdapterError::InvalidDefinition("missing function.name".into()))?;
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            BuiltinAdapterError::InvalidDefinition(format!("missing description for {name}"))
        })?;
    let input_schema = function.get("parameters").cloned().ok_or_else(|| {
        BuiltinAdapterError::InvalidDefinition(format!("missing parameters for {name}"))
    })?;
    let (capabilities, side_effect, concurrency, idempotency, cancellation) = metadata(name)?;
    let descriptor = ToolDescriptor {
        name: ToolName::parse(format!("builtin:{name}"))?,
        version: Version::new(1, 0, 0),
        description: description.to_owned(),
        input_schema,
        capabilities,
        side_effect,
        concurrency,
        idempotency,
        timeout_ms: 20_000,
        max_output_bytes: 256 * 1024,
        cancellation,
        source: ToolSource {
            layer: ToolLayer::Builtin,
            id: format!("lato.builtin.{name}"),
            replacement: None,
        },
    };
    descriptor.validate().map_err(|error| {
        BuiltinAdapterError::InvalidDefinition(format!("invalid descriptor for {name}: {error}"))
    })?;
    Ok(Arc::new(LegacyDispatchTool {
        descriptor,
        legacy_name: name.to_owned(),
        environment,
    }))
}

type ToolMetadata = (
    Vec<ToolCapability>,
    SideEffect,
    ToolConcurrency,
    ToolIdempotency,
    ToolCancellation,
);

fn metadata(name: &str) -> Result<ToolMetadata, BuiltinAdapterError> {
    let value = match name {
        "read_file" | "list_dir" | "grep" => (
            vec![ToolCapability::FileRead],
            SideEffect::ReadOnly,
            ToolConcurrency::Parallel,
            ToolIdempotency::Idempotent,
            ToolCancellation::Cooperative,
        ),
        "write_file" | "search_replace" => (
            vec![ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
            ToolConcurrency::ResourceKeyed,
            ToolIdempotency::NonIdempotent,
            ToolCancellation::Cooperative,
        ),
        "run_terminal_command" => (
            vec![ToolCapability::ProcessSpawn],
            SideEffect::ExternalMutation,
            ToolConcurrency::Serial,
            ToolIdempotency::NonIdempotent,
            ToolCancellation::Unsupported,
        ),
        "web_fetch" => (
            vec![ToolCapability::NetworkRead],
            SideEffect::ReadOnly,
            ToolConcurrency::Parallel,
            ToolIdempotency::Idempotent,
            ToolCancellation::Cooperative,
        ),
        "spawn_subagent" => (
            vec![ToolCapability::TaskControl, ToolCapability::FileWrite],
            SideEffect::WorkspaceMutation,
            ToolConcurrency::Serial,
            ToolIdempotency::NonIdempotent,
            ToolCancellation::Unsupported,
        ),
        "todo_write" => (
            vec![ToolCapability::TaskControl],
            SideEffect::None,
            ToolConcurrency::Serial,
            ToolIdempotency::Idempotent,
            ToolCancellation::Cooperative,
        ),
        other => {
            return Err(BuiltinAdapterError::InvalidDefinition(format!(
                "unknown built-in {other}"
            )));
        }
    };
    Ok(value)
}

fn classify_legacy_error(message: String) -> ToolError {
    let code = if message.starts_with("missing ") {
        "tool.invalid_arguments"
    } else if message.contains("permission required") || message.contains("denied by") {
        "tool.policy_denied"
    } else {
        "tool.execution_failed"
    };
    tool_error(code, message)
}

fn tool_error(code: &str, message: impl Into<String>) -> ToolError {
    ToolError::new(code, message, Retryability::Never)
}
