// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/skills/discovery.rs
// License: Apache-2.0
// Lato changes: consumes only active immutable plugin snapshots and fail-closes escaped or structurally invalid candidates

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use serde_yaml::Value;

use crate::{LoadedPlugin, PluginSnapshot};

use super::{
    DiscoveredSkill, MAX_BODY_PEEK_BYTES, MAX_DESCRIPTION_CHARS, MAX_FRONTMATTER_BYTES,
    MAX_SKILL_FILE_BYTES, MAX_SKILL_WALK_DEPTH, SkillDiagnostic, SkillDiscovery,
};

const MAX_DIAGNOSTICS: usize = 128;
const MAX_DIAGNOSTIC_BYTES: usize = 512;
const MAX_ARGUMENT_HINT_BYTES: usize = 512;
const MAX_ALLOWED_TOOLS: usize = 128;
const MAX_ALLOWED_TOOL_BYTES: usize = 128;
const MAX_SKILL_NAME_BYTES: usize = 64;

pub fn discover_skills(snapshot: &PluginSnapshot) -> SkillDiscovery {
    let mut discovery = SkillDiscovery {
        generation: snapshot.generation(),
        ..SkillDiscovery::default()
    };
    let mut candidates = Vec::new();

    for plugin in snapshot.active_plugins() {
        let mut roots = plugin.skill_dirs.clone();
        roots.sort();
        for skill_root in roots {
            collect_candidates(plugin, &skill_root, &mut candidates);
        }
    }

    let mut candidates = candidates
        .into_iter()
        .filter_map(|candidate| prepare_candidate(candidate, &mut discovery.diagnostics))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.plugin
            .name
            .cmp(&right.plugin.name)
            .then_with(|| left.canonical_path.cmp(&right.canonical_path))
    });
    let mut seen = HashSet::new();
    for candidate in candidates {
        materialize_candidate(candidate, &mut seen, &mut discovery);
    }
    discovery
}

struct Candidate<'a> {
    plugin: &'a LoadedPlugin,
    skill_root: PathBuf,
    path: PathBuf,
}

struct PreparedCandidate<'a> {
    plugin: &'a LoadedPlugin,
    skill_root: PathBuf,
    canonical_path: PathBuf,
}

fn collect_candidates<'a>(
    plugin: &'a LoadedPlugin,
    skill_root: &Path,
    out: &mut Vec<Candidate<'a>>,
) {
    let root_skill = skill_root.join("SKILL.md");
    if root_skill.is_file() {
        out.push(Candidate {
            plugin,
            skill_root: skill_root.to_owned(),
            path: root_skill,
        });
    }
    walk_for_skill_md(plugin, skill_root, skill_root, out, 0);
}

fn walk_for_skill_md<'a>(
    plugin: &'a LoadedPlugin,
    skill_root: &Path,
    dir: &Path,
    out: &mut Vec<Candidate<'a>>,
    depth: usize,
) {
    if depth > MAX_SKILL_WALK_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut dirs = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    for child in dirs {
        let skill = child.join("SKILL.md");
        if skill.is_file() {
            out.push(Candidate {
                plugin,
                skill_root: skill_root.to_owned(),
                path: skill,
            });
        }
        walk_for_skill_md(plugin, skill_root, &child, out, depth + 1);
    }
}

fn prepare_candidate<'a>(
    candidate: Candidate<'a>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<PreparedCandidate<'a>> {
    let canonical_path = match dunce::canonicalize(&candidate.path) {
        Ok(path) => path,
        Err(error) => {
            push_diagnostic(diagnostics, "skill.canonicalize", &candidate.path, error);
            return None;
        }
    };
    if !canonical_path.starts_with(&candidate.plugin.canonical_root) {
        push_diagnostic(
            diagnostics,
            "skill.path_escape",
            &candidate.path,
            "canonical skill path escapes the plugin root",
        );
        return None;
    }
    Some(PreparedCandidate {
        plugin: candidate.plugin,
        skill_root: candidate.skill_root,
        canonical_path,
    })
}

fn materialize_candidate(
    candidate: PreparedCandidate<'_>,
    seen: &mut HashSet<PathBuf>,
    discovery: &mut SkillDiscovery,
) {
    let canonical_path = candidate.canonical_path;
    if !seen.insert(canonical_path.clone()) {
        return;
    }

    let metadata = match fs::metadata(&canonical_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            push_diagnostic(
                &mut discovery.diagnostics,
                "skill.metadata",
                &canonical_path,
                error,
            );
            return;
        }
    };
    if metadata.len() > MAX_SKILL_FILE_BYTES as u64 {
        push_diagnostic(
            &mut discovery.diagnostics,
            "skill.file_too_large",
            &canonical_path,
            format!("SKILL.md exceeds {MAX_SKILL_FILE_BYTES} bytes"),
        );
        return;
    }
    let bytes = match fs::read(&canonical_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            push_diagnostic(
                &mut discovery.diagnostics,
                "skill.read",
                &canonical_path,
                error,
            );
            return;
        }
    };
    if bytes.len() > MAX_SKILL_FILE_BYTES {
        push_diagnostic(
            &mut discovery.diagnostics,
            "skill.file_too_large",
            &canonical_path,
            format!("SKILL.md exceeds {MAX_SKILL_FILE_BYTES} bytes"),
        );
        return;
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) => {
            push_diagnostic(
                &mut discovery.diagnostics,
                "skill.invalid_utf8",
                &canonical_path,
                error,
            );
            return;
        }
    };
    let fallback_name = canonical_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let parsed = match parse_skill(
        &content,
        fallback_name,
        &canonical_path,
        &mut discovery.diagnostics,
    ) {
        Ok(parsed) => parsed,
        Err(message) => {
            push_diagnostic(
                &mut discovery.diagnostics,
                "skill.parse",
                &canonical_path,
                message,
            );
            return;
        }
    };
    let skill_dir = canonical_path
        .parent()
        .unwrap_or(&candidate.skill_root)
        .to_owned();
    discovery.skills.push(DiscoveredSkill {
        plugin_name: candidate.plugin.name.clone(),
        plugin_root: candidate.plugin.canonical_root.clone(),
        skill_dir,
        source_path: canonical_path,
        name: parsed.name,
        description: parsed.description,
        has_authored_description: parsed.has_authored_description,
        when_to_use: parsed.when_to_use,
        argument_hint: parsed.argument_hint,
        allowed_tools: parsed.allowed_tools,
        user_invocable: parsed.user_invocable,
        disable_model_invocation: parsed.disable_model_invocation,
        body: parsed.body,
        paths: parsed.paths,
        license: parsed.license,
        compatibility: parsed.compatibility,
        metadata: parsed.metadata,
        model: parsed.model,
        effort: parsed.effort,
    });
}

struct ParsedSkill {
    name: String,
    description: String,
    has_authored_description: bool,
    when_to_use: Option<String>,
    argument_hint: Option<String>,
    allowed_tools: Option<Vec<String>>,
    user_invocable: bool,
    disable_model_invocation: bool,
    body: String,
    paths: Option<Vec<String>>,
    license: Option<String>,
    compatibility: Option<String>,
    metadata: Option<BTreeMap<String, String>>,
    model: Option<String>,
    effort: Option<String>,
}

fn parse_skill(
    content: &str,
    fallback_name: Option<&str>,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Result<ParsedSkill, String> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let map = match frontmatter {
        Some(yaml) => parse_frontmatter(yaml, path, diagnostics),
        None => BTreeMap::new(),
    };

    let frontmatter_name = coerce_to_string(map.get("name"));
    let name = [frontmatter_name.as_deref(), fallback_name]
        .into_iter()
        .flatten()
        .map(normalize_skill_name)
        .find(|candidate| valid_skill_name(candidate))
        .ok_or_else(|| {
            "frontmatter and directory names do not yield a valid skill name".to_owned()
        })?;

    let authored_description = coerce_to_string(map.get("description"));
    if map.contains_key("description") && authored_description.is_none() {
        push_diagnostic(
            diagnostics,
            "skill.invalid_description",
            path,
            "description must be a scalar",
        );
    }
    let has_authored_description = authored_description.is_some();
    let description = authored_description
        .map(|value| truncate_chars(value, MAX_DESCRIPTION_CHARS))
        .or_else(|| derive_body_description(&body))
        .unwrap_or_else(|| name.clone());
    let when_to_use = coerce_to_string(map.get("when-to-use").or_else(|| map.get("when_to_use")))
        .map(|value| truncate_chars(value, MAX_DESCRIPTION_CHARS));
    let argument_hint = coerce_to_string(map.get("argument-hint"))
        .map(|value| truncate_bytes(value, MAX_ARGUMENT_HINT_BYTES));
    let allowed_tools = parse_allowed_tools(map.get("allowed-tools"), path, diagnostics);
    let paths = parse_paths(map.get("paths"), path, diagnostics);
    let metadata = parse_metadata(map.get("metadata"), path, diagnostics);

    Ok(ParsedSkill {
        name,
        description,
        has_authored_description,
        when_to_use,
        argument_hint,
        allowed_tools,
        user_invocable: map.get("user-invocable").is_none_or(parse_boolean),
        disable_model_invocation: map
            .get("disable-model-invocation")
            .is_some_and(parse_boolean),
        body,
        paths,
        license: coerce_to_string(map.get("license")),
        compatibility: coerce_to_string(map.get("compatibility")),
        metadata,
        model: coerce_to_string(map.get("model")),
        effort: coerce_to_string(map.get("effort")),
    })
}

fn split_frontmatter(content: &str) -> Result<(Option<&str>, String), String> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---")
        || !trimmed
            .get(3..)
            .is_some_and(|rest| rest.starts_with(['\n', '\r']))
    {
        return Ok((None, content.to_owned()));
    }
    let opening_len = if trimmed.starts_with("---\r\n") { 5 } else { 4 };
    let after_opening = &trimmed[opening_len..];
    let mut offset = opening_len;
    for line in after_opening.split_inclusive('\n') {
        offset += line.len();
        if offset > MAX_FRONTMATTER_BYTES {
            return Err(format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"));
        }
        if line.trim() == "---" {
            let yaml_len = offset - opening_len - line.len();
            let yaml = &trimmed[opening_len..opening_len + yaml_len];
            let body = trimmed[offset..]
                .trim_start_matches(['\r', '\n'])
                .to_owned();
            return Ok((Some(yaml), body));
        }
    }
    Err("frontmatter has no closing delimiter".to_owned())
}

fn parse_frontmatter(
    yaml: &str,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> BTreeMap<String, Value> {
    serde_yaml::from_str(yaml)
        .or_else(|_| serde_yaml::from_str(&quote_problematic_values(yaml)))
        .unwrap_or_else(|error| {
            let recovered = recover_scalar_fields(yaml);
            push_diagnostic(diagnostics, "skill.frontmatter_recovered", path, error);
            recovered
        })
}

fn coerce_to_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => nonempty(value),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_boolean(value: &Value) -> bool {
    matches!(value, Value::Bool(true)) || matches!(value, Value::String(value) if value == "true")
}

fn parse_allowed_tools(
    value: Option<&Value>,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<Vec<String>> {
    let value = value?;
    let raw = match value {
        Value::String(value) => split_top_level(value, '(', ')', true),
        Value::Sequence(values) => values
            .iter()
            .filter_map(Value::as_str)
            .filter_map(nonempty)
            .collect(),
        _ => {
            push_diagnostic(
                diagnostics,
                "skill.invalid_allowed_tools",
                path,
                "allowed-tools must be a string or list of strings",
            );
            return None;
        }
    };
    if raw.len() > MAX_ALLOWED_TOOLS {
        push_diagnostic(
            diagnostics,
            "skill.too_many_allowed_tools",
            path,
            "allowed-tools exceeds 128 entries",
        );
    }
    Some(
        raw.into_iter()
            .take(MAX_ALLOWED_TOOLS)
            .filter_map(|entry| {
                if entry.len() > MAX_ALLOWED_TOOL_BYTES {
                    push_diagnostic(
                        diagnostics,
                        "skill.allowed_tool_too_long",
                        path,
                        "allowed-tools entry exceeds 128 bytes",
                    );
                    None
                } else {
                    Some(entry)
                }
            })
            .collect(),
    )
}

fn parse_paths(
    value: Option<&Value>,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<Vec<String>> {
    let value = value?;
    let paths = match value {
        Value::String(value) => split_top_level(value, '{', '}', false),
        Value::Sequence(values) => values
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|value| split_top_level(value, '{', '}', false))
            .collect(),
        _ => {
            push_diagnostic(
                diagnostics,
                "skill.invalid_paths",
                path,
                "paths must be a string or list of strings",
            );
            return None;
        }
    };
    let paths = paths
        .into_iter()
        .map(|path| path.strip_suffix("/**").unwrap_or(&path).to_owned())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    (!paths.is_empty() && !paths.iter().all(|path| path == "**")).then_some(paths)
}

fn parse_metadata(
    value: Option<&Value>,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<BTreeMap<String, String>> {
    let value = value?;
    let Value::Mapping(values) = value else {
        push_diagnostic(
            diagnostics,
            "skill.invalid_metadata",
            path,
            "metadata must be a mapping",
        );
        return None;
    };
    let metadata = values
        .iter()
        .filter_map(|(key, value)| Some((key.as_str()?.to_owned(), value.as_str()?.to_owned())))
        .collect::<BTreeMap<_, _>>();
    (!metadata.is_empty()).then_some(metadata)
}

fn split_top_level(input: &str, open: char, close: char, split_whitespace: bool) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0_i32;
    let flush = |current: &mut String, parts: &mut Vec<String>| {
        if let Some(value) = nonempty(current) {
            parts.push(value);
        }
        current.clear();
    };
    for character in input.chars() {
        if character == open {
            depth += 1;
            current.push(character);
        } else if character == close {
            depth -= 1;
            current.push(character);
        } else if depth <= 0
            && (character == ',' || (split_whitespace && character.is_whitespace()))
        {
            flush(&mut current, &mut parts);
        } else {
            current.push(character);
        }
    }
    flush(&mut current, &mut parts);
    parts
}

fn normalize_skill_name(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    for character in value.trim().chars() {
        let character = character.to_ascii_lowercase();
        let character = if character.is_ascii_lowercase() || character.is_ascii_digit() {
            character
        } else {
            '-'
        };
        if character != '-' || !normalized.ends_with('-') {
            normalized.push(character);
        }
    }
    normalized.trim_matches('-').to_owned()
}

fn valid_skill_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SKILL_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}

fn derive_body_description(body: &str) -> Option<String> {
    let peek = truncate_bytes(body.to_owned(), MAX_BODY_PEEK_BYTES);
    first_prose_paragraph(&peek)
        .or_else(|| first_heading(&peek))
        .map(|value| truncate_chars(value, MAX_DESCRIPTION_CHARS))
}

fn first_prose_paragraph(body: &str) -> Option<String> {
    let mut paragraph = Vec::new();
    let mut fenced = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced
            || trimmed.starts_with('#')
            || trimmed.starts_with('>')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
            || is_ordered_list(trimmed)
            || trimmed.contains(" | ")
        {
            if !paragraph.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.is_empty() {
            if !paragraph.is_empty() {
                break;
            }
        } else {
            paragraph.push(trimmed);
        }
    }
    (!paragraph.is_empty()).then(|| flatten_inline(&paragraph.join(" ")))
}

fn first_heading(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let heading = line
            .trim()
            .strip_prefix('#')?
            .trim_start_matches('#')
            .trim();
        (!heading.is_empty()).then(|| flatten_inline(heading))
    })
}

fn is_ordered_list(value: &str) -> bool {
    value.split_once(". ").is_some_and(|(prefix, _)| {
        !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn flatten_inline(value: &str) -> String {
    value
        .chars()
        .filter(|character| !matches!(character, '`' | '*' | '_'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_problematic_values(frontmatter: &str) -> String {
    frontmatter
        .lines()
        .map(|line| {
            let Some((key, after)) = line.split_once(':') else {
                return line.to_owned();
            };
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphabetic() || byte == b'_' || byte == b'-')
            {
                return line.to_owned();
            }
            let value = after.trim();
            let already_quoted = (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''));
            let problematic = value.contains(": ")
                || value.contains([
                    '{', '}', '[', ']', '*', '&', '#', '!', '|', '>', '%', '@', '`',
                ]);
            if value.is_empty() || already_quoted || !problematic {
                line.to_owned()
            } else {
                format!(
                    "{key}: \"{}\"",
                    value.replace('\\', "\\\\").replace('"', "\\\"")
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn recover_scalar_fields(yaml: &str) -> BTreeMap<String, Value> {
    const KEYS: &[&str] = &["name", "description", "when-to-use", "when_to_use"];
    let mut recovered = BTreeMap::new();
    for line in yaml.lines().filter(|line| !line.starts_with([' ', '\t'])) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if !KEYS.contains(&key) {
            continue;
        }
        let value = value.trim();
        if value.is_empty() || matches!(value.as_bytes().first(), Some(b'|' | b'>')) {
            continue;
        }
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or_else(|| {
                value
                    .split_once(" #")
                    .map_or(value, |(before, _)| before.trim_end())
            });
        recovered
            .entry(key.to_owned())
            .or_insert_with(|| Value::String(value.to_owned()));
    }
    recovered
}

fn truncate_chars(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value
    } else {
        value.chars().take(max_chars).collect()
    }
}

fn truncate_bytes(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn push_diagnostic(
    out: &mut Vec<SkillDiagnostic>,
    code: &'static str,
    path: &Path,
    message: impl std::fmt::Display,
) {
    if out.len() == MAX_DIAGNOSTICS {
        return;
    }
    out.push(SkillDiagnostic::bounded(
        code,
        path,
        message.to_string(),
        MAX_DIAGNOSTIC_BYTES,
    ));
}
