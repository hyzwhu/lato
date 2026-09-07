// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/skills/skill.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-workspace/src/permission/rules.rs
// License: Apache-2.0
// Lato changes: generic resolver boundary, bounded metadata, and immutable runtime scope compilation

use crate::ToolRuntime;
use async_trait::async_trait;
use lato_core::{
    Retryability, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use regex::Regex;
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};

const MAX_QUALIFIED_NAME_BYTES: usize = 129;
const MAX_ALLOWED_TOOL_SPECS: usize = 128;
const MAX_ALLOWED_TOOL_SPEC_BYTES: usize = 128;
const MAX_BODY_HASH_BYTES: usize = 128;
const MAX_SKILL_MESSAGE_BYTES: usize = 256 * 1024;

#[async_trait]
pub trait SkillResolver: Send + Sync {
    async fn invoke(
        &self,
        context: &ToolContext,
        skill: &str,
        args: Option<&str>,
    ) -> Result<ResolvedSkill, ToolError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSkill {
    pub qualified_name: String,
    pub message: String,
    pub allowed_tool_specs: Option<Vec<String>>,
    pub body_hash: String,
}

pub struct SkillTool {
    resolver: Arc<dyn SkillResolver>,
}

impl SkillTool {
    pub fn new(resolver: Arc<dyn SkillResolver>) -> Self {
        Self { resolver }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillInput {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}

#[async_trait]
impl Tool for SkillTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::parse("builtin:skill").expect("static skill tool name is valid"),
            version: Version::new(1, 0, 0),
            description: "Load the full instructions for an available skill.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill": {"type": "string"},
                    "args": {"type": "string"}
                },
                "required": ["skill"],
                "additionalProperties": false
            }),
            capabilities: vec![ToolCapability::ExtensionInvoke],
            side_effect: SideEffect::None,
            concurrency: ToolConcurrency::Parallel,
            idempotency: ToolIdempotency::Idempotent,
            timeout_ms: 20_000,
            max_output_bytes: MAX_SKILL_MESSAGE_BYTES,
            cancellation: ToolCancellation::Cooperative,
            source: ToolSource {
                layer: ToolLayer::Builtin,
                id: "lato.builtin.skill".into(),
                replacement: None,
            },
        }
    }

    async fn invoke(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(tool_error("tool.cancelled", "tool call was cancelled"));
        }
        if context.execution_grant.is_none() {
            return Err(tool_error(
                "policy.grant_missing",
                "tool execution grant is missing",
            ));
        }
        let input: SkillInput = serde_json::from_value(arguments)
            .map_err(|error| tool_error("tool.invalid_arguments", error))?;
        let resolved = self
            .resolver
            .invoke(&context, &input.skill, input.args.as_deref())
            .await?;
        validate_resolved_skill(&resolved)?;
        Ok(ToolOutput {
            content: resolved.message,
            metadata: json!({
                "kind": "skill_invocation",
                "qualifiedName": resolved.qualified_name,
                "allowedToolSpecs": resolved.allowed_tool_specs,
                "bodyHash": resolved.body_hash,
            }),
            truncated: false,
            artifact_path: None,
        })
    }
}

fn validate_resolved_skill(resolved: &ResolvedSkill) -> Result<(), ToolError> {
    if resolved.qualified_name.is_empty()
        || resolved.qualified_name.len() > MAX_QUALIFIED_NAME_BYTES
        || resolved.body_hash.is_empty()
        || resolved.body_hash.len() > MAX_BODY_HASH_BYTES
        || resolved.message.len() > MAX_SKILL_MESSAGE_BYTES
        || resolved.allowed_tool_specs.as_ref().is_some_and(|specs| {
            specs.len() > MAX_ALLOWED_TOOL_SPECS
                || specs
                    .iter()
                    .any(|spec| spec.is_empty() || spec.len() > MAX_ALLOWED_TOOL_SPEC_BYTES)
        })
    {
        return Err(tool_error(
            "skill.invalid_metadata",
            "the resolved skill exceeded a metadata or output bound",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SkillToolScope {
    rules: Vec<AllowedToolRule>,
}

#[derive(Clone, Debug)]
struct AllowedToolRule {
    wire_names: HashSet<String>,
    argument_pattern: Option<Regex>,
    argument_field: Option<&'static str>,
}

impl SkillToolScope {
    pub fn compile(specs: &[String], runtime: &ToolRuntime) -> Result<Self, ToolError> {
        if specs.is_empty() {
            return Ok(Self {
                rules: vec![AllowedToolRule {
                    wire_names: runtime.registered_local_names().into_iter().collect(),
                    argument_pattern: None,
                    argument_field: None,
                }],
            });
        }
        let mut rules = Vec::new();
        for spec in specs.iter().take(MAX_ALLOWED_TOOL_SPECS) {
            if spec.is_empty() || spec.len() > MAX_ALLOWED_TOOL_SPEC_BYTES {
                continue;
            }
            for alternative in split_top_level_alternatives(spec) {
                compile_rule(alternative.trim(), runtime, &mut rules)?;
            }
        }
        Ok(Self { rules })
    }

    pub fn allows_name(&self, wire_name: &str) -> bool {
        let name = normalized_scope_name(wire_name);
        self.rules.iter().any(|rule| rule.wire_names.contains(name))
    }

    pub fn allows_call(&self, wire_name: &str, arguments: &Value) -> bool {
        let name = normalized_scope_name(wire_name);
        self.rules.iter().any(|rule| {
            if !rule.wire_names.contains(name) {
                return false;
            }
            let Some(pattern) = &rule.argument_pattern else {
                return true;
            };
            let candidate = rule
                .argument_field
                .and_then(|field| arguments.get(field))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| serde_json::to_string(arguments).ok());
            candidate.is_some_and(|candidate| {
                let candidate = if rule.argument_field == Some("command") {
                    candidate.trim_start()
                } else {
                    candidate.as_str()
                };
                pattern.is_match(candidate)
            })
        })
    }
}

fn compile_rule(
    spec: &str,
    runtime: &ToolRuntime,
    out: &mut Vec<AllowedToolRule>,
) -> Result<(), ToolError> {
    if spec == "*" {
        out.push(AllowedToolRule {
            wire_names: runtime.registered_local_names().into_iter().collect(),
            argument_pattern: None,
            argument_field: None,
        });
        return Ok(());
    }

    let (name, pattern) = match spec.find('(') {
        Some(open) if spec.ends_with(')') => (&spec[..open], Some(&spec[open + 1..spec.len() - 1])),
        Some(_) => {
            return Err(tool_error(
                "skill.invalid_allowed_tool",
                "malformed grouped tool rule",
            ));
        }
        None => (spec, None),
    };
    let targets = compatible_tool_names(name.trim(), runtime);
    if targets.is_empty() {
        return Ok(());
    }
    let (argument_pattern, argument_field) = match pattern.map(str::trim) {
        None | Some("") | Some("*") => (None, None),
        Some(pattern) => {
            let field = argument_field_for(name.trim(), &targets);
            (
                Some(compile_argument_pattern(pattern, field == Some("command"))?),
                field,
            )
        }
    };
    out.push(AllowedToolRule {
        wire_names: targets,
        argument_pattern,
        argument_field,
    });
    Ok(())
}

fn compatible_tool_names(name: &str, runtime: &ToolRuntime) -> HashSet<String> {
    let aliases: &[&str] = match name {
        "Bash" | "bash" => &["run_terminal_command"],
        "Read" | "read" => &["read_file"],
        "Write" => &["write_file"],
        "Edit" => &["search_replace"],
        "Grep" => &["grep"],
        "Glob" => &["list_dir"],
        "WebFetch" => &["web_fetch"],
        _ => &[],
    };
    if !aliases.is_empty() {
        return aliases
            .iter()
            .filter(|candidate| runtime.has_local_name(candidate))
            .map(|candidate| (*candidate).to_owned())
            .collect();
    }
    runtime
        .resolve_registered_name(name)
        .into_iter()
        .map(|name| name.local_name().to_owned())
        .collect()
}

fn argument_field_for(name: &str, targets: &HashSet<String>) -> Option<&'static str> {
    if matches!(name, "Bash" | "bash") || targets.contains("run_terminal_command") {
        Some("command")
    } else if targets.contains("web_fetch") {
        Some("url")
    } else if targets.iter().any(|name| {
        matches!(
            name.as_str(),
            "read_file" | "list_dir" | "grep" | "write_file" | "search_replace"
        )
    }) {
        Some("path")
    } else {
        None
    }
}

fn compile_argument_pattern(pattern: &str, command: bool) -> Result<Regex, ToolError> {
    let prefix = command && pattern.ends_with(":*");
    let pattern = if prefix {
        pattern.strip_suffix(":*").unwrap().trim()
    } else {
        pattern.trim()
    };
    let mut regex = String::from("^");
    for character in pattern.chars() {
        match character {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            other => regex.push_str(&regex::escape(&other.to_string())),
        }
    }
    if prefix || (command && !pattern.contains(['*', '?'])) {
        regex.push_str("(?:$| )");
    } else {
        regex.push('$');
    }
    Regex::new(&regex).map_err(|error| tool_error("skill.invalid_allowed_tool", error))
}

fn split_top_level_alternatives(spec: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (index, character) in spec.char_indices() {
        match character {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                out.push(&spec[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&spec[start..]);
    out
}

fn normalized_scope_name(name: &str) -> &str {
    let name = name
        .strip_prefix("Lato:")
        .unwrap_or(name)
        .rsplit_once(':')
        .map_or(name.strip_prefix("Lato:").unwrap_or(name), |(_, local)| {
            local
        });
    match name {
        "Bash" | "bash" => "run_terminal_command",
        "Read" | "read" => "read_file",
        "Write" | "write" => "write_file",
        "Edit" => "search_replace",
        "Grep" => "grep",
        "Glob" => "list_dir",
        "WebFetch" => "web_fetch",
        _ => name,
    }
}

fn tool_error(code: &str, message: impl std::fmt::Display) -> ToolError {
    ToolError::new(code, message.to_string(), Retryability::Never)
}
