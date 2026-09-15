// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/skills/skill.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-workspace/src/permission/rules.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-workspace/src/permission/policy.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-paths/src/lib.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/web_fetch/domain.rs
// License: Apache-2.0
// Lato changes: generic resolver boundary, bounded metadata, and immutable runtime scope compilation

use crate::ToolRuntime;
use async_trait::async_trait;
use lato_core::{
    Retryability, SideEffect, Tool, ToolCancellation, ToolCapability, ToolConcurrency, ToolContext,
    ToolDescriptor, ToolError, ToolIdempotency, ToolLayer, ToolName, ToolOutput, ToolSource,
};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

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
    argument_matcher: Option<ArgumentMatcher>,
    argument_field: Option<&'static str>,
}

#[derive(Clone, Debug)]
enum ArgumentMatcher {
    Bash {
        pattern: String,
        glob: glob::Pattern,
    },
    Path(Option<glob::Pattern>),
    Domain(String),
    Freeform(glob::Pattern),
}

enum MatchedArgument {
    Original,
    ResolvedPath(PathBuf),
}

impl SkillToolScope {
    pub fn compile(specs: &[String], runtime: &ToolRuntime) -> Result<Self, ToolError> {
        if specs.is_empty() {
            return Ok(Self {
                rules: vec![AllowedToolRule {
                    wire_names: runtime.registered_local_names().into_iter().collect(),
                    argument_matcher: None,
                    argument_field: None,
                }],
            });
        }
        if specs.len() > MAX_ALLOWED_TOOL_SPECS {
            return Err(tool_error(
                "skill.invalid_allowed_tool",
                "allowed-tools exceeds 128 entries",
            ));
        }
        let mut rules = Vec::new();
        for spec in specs {
            if spec.is_empty() {
                continue;
            }
            if spec.len() > MAX_ALLOWED_TOOL_SPEC_BYTES {
                return Err(tool_error(
                    "skill.invalid_allowed_tool",
                    "allowed-tools entry exceeds 128 bytes",
                ));
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

    pub fn allows_call(&self, wire_name: &str, arguments: &Value, cwd: &Path) -> bool {
        self.canonicalize_call(wire_name, arguments, cwd).is_some()
    }

    pub(crate) fn canonicalize_call(
        &self,
        wire_name: &str,
        arguments: &Value,
        cwd: &Path,
    ) -> Option<Value> {
        let name = normalized_scope_name(wire_name);
        self.rules.iter().find_map(|rule| {
            if !rule.wire_names.contains(name) {
                return None;
            }
            let Some(matcher) = &rule.argument_matcher else {
                return Some(arguments.clone());
            };
            let candidate = rule
                .argument_field
                .and_then(|field| arguments.get(field))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| serde_json::to_string(arguments).ok());
            let candidate = candidate?;
            let MatchedArgument::ResolvedPath(resolved) = matcher.matches(&candidate, cwd, name)?
            else {
                return Some(arguments.clone());
            };
            let field = rule.argument_field?;
            let mut canonical = arguments.clone();
            canonical.as_object_mut()?.insert(
                field.to_owned(),
                // dunce strips the \\?\ verbatim prefix std::fs::canonicalize
                // produces on Windows; re-resolving a verbatim path spelled
                // with forward slashes is not reliable, and the scoped
                // execute-time re-check compares against this rewrite.
                Value::String(path_match_string(dunce::simplified(&resolved))),
            );
            Some(canonical)
        })
    }
}

impl ArgumentMatcher {
    fn matches(&self, candidate: &str, cwd: &Path, tool_name: &str) -> Option<MatchedArgument> {
        match self {
            Self::Bash { pattern, glob } => {
                let command = candidate.trim_start();
                (matches_command_prefix(command, pattern)
                    || glob_matches(glob, command, MatchContext::Freeform))
                .then_some(MatchedArgument::Original)
            }
            Self::Path(pattern) => {
                let paths = resolve_scoped_path(candidate, cwd, tool_name == "write_file")?;
                let matches = pattern.as_ref().is_none_or(|pattern| {
                    paths
                        .lexical_forms
                        .iter()
                        .any(|path| glob_matches(pattern, path, MatchContext::Path))
                        && paths
                            .resolved_forms
                            .iter()
                            .any(|path| glob_matches(pattern, path, MatchContext::Path))
                });
                matches.then_some(MatchedArgument::ResolvedPath(paths.resolved))
            }
            Self::Domain(pattern) => {
                domain_matches(pattern, candidate).then_some(MatchedArgument::Original)
            }
            Self::Freeform(pattern) => glob_matches(pattern, candidate, MatchContext::Freeform)
                .then_some(MatchedArgument::Original),
        }
    }
}

#[derive(Clone, Copy)]
enum MatchContext {
    Path,
    Freeform,
}

fn compile_rule(
    spec: &str,
    runtime: &ToolRuntime,
    out: &mut Vec<AllowedToolRule>,
) -> Result<(), ToolError> {
    if spec == "*" {
        out.push(AllowedToolRule {
            wire_names: runtime.registered_local_names().into_iter().collect(),
            argument_matcher: None,
            argument_field: None,
        });
        return Ok(());
    }

    let (name, pattern) = parse_grouped_rule(spec)?;
    let targets = compatible_tool_names(name.trim(), runtime);
    if targets.is_empty() {
        return Ok(());
    }
    let field = argument_field_for(name.trim(), &targets);
    let path_rule = field == Some("path");
    let (argument_matcher, argument_field) = match pattern.as_deref().map(str::trim) {
        None | Some("") | Some("*") if path_rule => (Some(ArgumentMatcher::Path(None)), field),
        None | Some("") | Some("*") => (None, None),
        Some(pattern) => {
            let bash_empty = (matches!(name.trim(), "Bash" | "bash")
                || targets.contains("run_terminal_command"))
                && pattern
                    .strip_suffix(":*")
                    .is_some_and(|value| value.trim().is_empty());
            if bash_empty {
                (None, None)
            } else {
                (
                    Some(compile_argument_matcher(name.trim(), &targets, pattern)?),
                    field,
                )
            }
        }
    };
    out.push(AllowedToolRule {
        wire_names: targets,
        argument_matcher,
        argument_field,
    });
    Ok(())
}

fn compatible_tool_names(name: &str, runtime: &ToolRuntime) -> HashSet<String> {
    let aliases: &[&str] = match name {
        "Bash" | "bash" => &["run_terminal_command"],
        "Read" | "read" => &["read_file"],
        "Write" => &["write_file"],
        "Edit" => &["edit_file"],
        "Grep" => &["grep"],
        "Glob" => &["glob"],
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
            "read_file" | "write_file" | "edit_file" | "grep" | "glob"
        )
    }) {
        Some("path")
    } else {
        None
    }
}

fn compile_argument_matcher(
    name: &str,
    targets: &HashSet<String>,
    pattern: &str,
) -> Result<ArgumentMatcher, ToolError> {
    let command = matches!(name, "Bash" | "bash") || targets.contains("run_terminal_command");
    let web_fetch = matches!(name, "WebFetch") || targets.contains("web_fetch");
    let path = targets.iter().any(|name| {
        matches!(
            name.as_str(),
            "read_file" | "write_file" | "edit_file" | "grep" | "glob"
        )
    });
    if command {
        let pattern = pattern
            .strip_suffix(":*")
            .unwrap_or(pattern)
            .trim()
            .to_owned();
        let glob = compile_glob(&pattern)?;
        return Ok(ArgumentMatcher::Bash { pattern, glob });
    }
    if web_fetch {
        if let Some(domain) = pattern.strip_prefix("domain:") {
            return Ok(ArgumentMatcher::Domain(normalize_domain(domain)));
        }
        return Ok(ArgumentMatcher::Freeform(compile_glob(pattern)?));
    }
    if path {
        return Ok(ArgumentMatcher::Path(Some(compile_glob(pattern)?)));
    }
    Ok(ArgumentMatcher::Freeform(compile_glob(pattern)?))
}

fn split_top_level_alternatives(spec: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (index, character) in spec.char_indices() {
        match character {
            '(' if is_unescaped(spec.as_bytes(), index) => depth = depth.saturating_add(1),
            ')' if is_unescaped(spec.as_bytes(), index) => depth = depth.saturating_sub(1),
            '|' if depth == 0 && is_unescaped(spec.as_bytes(), index) => {
                out.push(&spec[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&spec[start..]);
    out
}

fn parse_grouped_rule(spec: &str) -> Result<(&str, Option<String>), ToolError> {
    let Some(open) = find_first_unescaped(spec, b'(') else {
        return Ok((spec, None));
    };
    let content = &spec[open + 1..];
    let Some(close) = find_last_unescaped(content, b')') else {
        return Err(tool_error(
            "skill.invalid_allowed_tool",
            "malformed grouped tool rule: missing closing parenthesis",
        ));
    };
    let raw = content[..close].trim();
    let pattern = if raw.is_empty() || raw == "*" {
        None
    } else {
        Some(unescape_rule_content(raw))
    };
    Ok((&spec[..open], pattern))
}

fn is_unescaped(bytes: &[u8], position: usize) -> bool {
    let mut backslashes = 0usize;
    let mut index = position;
    while index > 0 && bytes[index - 1] == b'\\' {
        backslashes += 1;
        index -= 1;
    }
    backslashes.is_multiple_of(2)
}

fn find_first_unescaped(value: &str, target: u8) -> Option<usize> {
    value
        .as_bytes()
        .iter()
        .enumerate()
        .find(|(index, byte)| **byte == target && is_unescaped(value.as_bytes(), *index))
        .map(|(index, _)| index)
}

fn find_last_unescaped(value: &str, target: u8) -> Option<usize> {
    (0..value.len())
        .rev()
        .find(|index| value.as_bytes()[*index] == target && is_unescaped(value.as_bytes(), *index))
}

fn unescape_rule_content(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_owned();
    }
    value
        .replace("\\(", "(")
        .replace("\\)", ")")
        .replace("\\\\", "\\")
}

fn compile_glob(pattern: &str) -> Result<glob::Pattern, ToolError> {
    glob::Pattern::new(pattern)
        .map_err(|error| tool_error("skill.invalid_allowed_tool", error.to_string()))
}

fn glob_matches(pattern: &glob::Pattern, text: &str, context: MatchContext) -> bool {
    pattern.matches_with(
        text,
        glob::MatchOptions {
            require_literal_separator: matches!(context, MatchContext::Path),
            require_literal_leading_dot: false,
            ..Default::default()
        },
    )
}

fn matches_command_prefix(command: &str, pattern: &str) -> bool {
    // Intentional narrowing: do not let a bare `git` pattern authorize
    // `gitleaks`; the pinned Grok allow evaluator uses this word boundary even
    // though its broader generic rule matcher also has a raw-prefix path.
    command == pattern
        || (command.starts_with(pattern) && command.as_bytes().get(pattern.len()) == Some(&b' '))
}

struct ResolvedScopedPath {
    resolved: PathBuf,
    lexical_forms: Vec<String>,
    resolved_forms: Vec<String>,
}

fn resolve_scoped_path(path: &str, cwd: &Path, allow_missing: bool) -> Option<ResolvedScopedPath> {
    let lexical_cwd = absolute_lexical_cwd(cwd);
    let resolved_cwd = std::fs::canonicalize(&lexical_cwd).ok()?;
    let raw = Path::new(path);
    if is_tilde_path(raw) {
        return None;
    }
    let lexical = if raw.is_absolute() {
        normalize_lexically(raw)
    } else {
        normalize_lexically(&lexical_cwd.join(raw))
    };
    let lexical_forms = if lexical.starts_with(&lexical_cwd) {
        path_forms(&lexical, &lexical_cwd)
    } else if lexical.starts_with(&resolved_cwd) {
        let relative = lexical.strip_prefix(&resolved_cwd).ok()?;
        path_forms(&lexical, &resolved_cwd)
            .into_iter()
            .chain(std::iter::once(path_match_string(
                &lexical_cwd.join(relative),
            )))
            .collect()
    } else {
        return None;
    };

    let resolved = match std::fs::symlink_metadata(&lexical) {
        Ok(_) => std::fs::canonicalize(&lexical).ok()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => {
            canonicalize_missing_target(&lexical)?
        }
        Err(_) => return None,
    };
    if !resolved.starts_with(&resolved_cwd) {
        return None;
    }
    let relative = resolved.strip_prefix(&resolved_cwd).ok()?;
    Some(ResolvedScopedPath {
        resolved: resolved.clone(),
        lexical_forms,
        resolved_forms: path_forms(&resolved, &resolved_cwd)
            .into_iter()
            .chain(std::iter::once(path_match_string(
                &lexical_cwd.join(relative),
            )))
            .collect(),
    })
}

fn canonicalize_missing_target(path: &Path) -> Option<PathBuf> {
    // Resolve the nearest existing ancestor first. Encountering a dangling or
    // cyclic symlink makes canonicalize fail, so missing write targets never
    // reconstruct through an unresolved link.
    let mut cursor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(&cursor) {
            Ok(_) => {
                let mut resolved = std::fs::canonicalize(&cursor).ok()?;
                if !missing.is_empty() && !std::fs::metadata(&resolved).ok()?.is_dir() {
                    return None;
                }
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Some(normalize_lexically(&resolved));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor.file_name()?.to_os_string());
                cursor = cursor.parent()?.to_path_buf();
            }
            Err(_) => return None,
        }
    }
}

fn path_forms(path: &Path, root: &Path) -> Vec<String> {
    let mut forms = vec![path_match_string(path)];
    if let Ok(relative) = path.strip_prefix(root) {
        let relative = path_match_string(relative);
        if relative.is_empty() || relative == "." {
            forms.extend([".".to_owned(), "./".to_owned()]);
        } else {
            forms.push(format!("./{relative}"));
            forms.push(relative);
        }
    }
    forms
}

fn absolute_lexical_cwd(cwd: &Path) -> PathBuf {
    if cwd.is_absolute() {
        normalize_lexically(cwd)
    } else {
        std::env::current_dir()
            .map(|current| normalize_lexically(&current.join(cwd)))
            .unwrap_or_else(|_| normalize_lexically(cwd))
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match components.last() {
                Some(Component::Normal(_)) => {
                    components.pop();
                }
                Some(Component::RootDir) => {}
                _ => components.push(component),
            },
            _ => components.push(component),
        }
    }
    if components.is_empty() {
        PathBuf::from(".")
    } else {
        components.into_iter().collect()
    }
}

fn is_tilde_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Normal(first)) if first.to_string_lossy().starts_with('~')
    )
}

fn path_match_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn domain_matches(pattern: &str, url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let domain = normalize_domain(host);
    let pattern = normalize_domain(pattern);
    !pattern.is_empty() && (domain == pattern || domain.ends_with(&format!(".{pattern}")))
}

fn normalize_domain(value: &str) -> String {
    let value = value.trim().trim_end_matches('/').trim_end_matches('.');
    value.strip_prefix("www.").unwrap_or(value).to_lowercase()
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
        "Edit" => "edit_file",
        "Grep" => "grep",
        "Glob" => "glob",
        "WebFetch" => "web_fetch",
        _ => name,
    }
}

fn tool_error(code: &str, message: impl std::fmt::Display) -> ToolError {
    ToolError::new(code, message.to_string(), Retryability::Never)
}
