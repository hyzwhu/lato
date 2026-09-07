// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/skills/discovery.rs
// License: Apache-2.0
// Lato changes: consumes only active immutable plugin snapshots and fail-closes escaped or structurally invalid candidates

use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde_yaml::Value;

use crate::{LoadedPlugin, PluginSnapshot};

use super::{
    DiscoveredSkill, MAX_BODY_PEEK_BYTES, MAX_DESCRIPTION_CHARS, MAX_FRONTMATTER_BYTES,
    MAX_SKILL_CANDIDATES, MAX_SKILL_DIRECTORIES_VISITED, MAX_SKILL_DIRECTORY_ENTRIES,
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
    let mut budget = DiscoveryBudget::default();

    for plugin in snapshot.active_plugins() {
        let mut roots = plugin.skill_dirs.clone();
        roots.sort();
        for skill_root in roots {
            collect_candidates(
                plugin,
                &skill_root,
                &mut candidates,
                &mut budget,
                &mut discovery.diagnostics,
            );
        }
    }

    let mut candidates = candidates
        .into_iter()
        .filter_map(|candidate| prepare_candidate(candidate, &mut discovery.diagnostics))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.canonical_path
            .cmp(&right.canonical_path)
            .then_with(|| left.plugin.id.0.cmp(&right.plugin.id.0))
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

#[derive(Default)]
struct DiscoveryBudget {
    directories_visited: usize,
    directory_entries: usize,
    candidate_limit_reported: bool,
    directory_limit_reported: bool,
    directory_entry_limit_reported: bool,
}

impl DiscoveryBudget {
    fn candidates_exhausted(&self, candidates: &[Candidate<'_>]) -> bool {
        candidates.len() >= MAX_SKILL_CANDIDATES && self.candidate_limit_reported
    }

    fn admit_directory(&mut self, path: &Path, diagnostics: &mut Vec<SkillDiagnostic>) -> bool {
        if self.directories_visited >= MAX_SKILL_DIRECTORIES_VISITED {
            if !self.directory_limit_reported {
                push_diagnostic(
                    diagnostics,
                    "skill.directory_limit",
                    path,
                    format!(
                        "skill discovery exceeds {MAX_SKILL_DIRECTORIES_VISITED} visited directories"
                    ),
                );
                self.directory_limit_reported = true;
            }
            return false;
        }
        self.directories_visited += 1;
        true
    }

    fn push_candidate<'a>(
        &mut self,
        candidate: Candidate<'a>,
        out: &mut Vec<Candidate<'a>>,
        diagnostics: &mut Vec<SkillDiagnostic>,
    ) -> bool {
        if out.len() >= MAX_SKILL_CANDIDATES {
            if !self.candidate_limit_reported {
                push_diagnostic(
                    diagnostics,
                    "skill.candidate_limit",
                    &candidate.path,
                    format!("skill discovery exceeds {MAX_SKILL_CANDIDATES} candidates"),
                );
                self.candidate_limit_reported = true;
            }
            return false;
        }
        out.push(candidate);
        true
    }
}

fn collect_candidates<'a>(
    plugin: &'a LoadedPlugin,
    skill_root: &Path,
    out: &mut Vec<Candidate<'a>>,
    budget: &mut DiscoveryBudget,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    walk_for_skill_md(plugin, skill_root, skill_root, out, budget, diagnostics, 0);
}

fn walk_for_skill_md<'a>(
    plugin: &'a LoadedPlugin,
    skill_root: &Path,
    dir: &Path,
    out: &mut Vec<Candidate<'a>>,
    budget: &mut DiscoveryBudget,
    diagnostics: &mut Vec<SkillDiagnostic>,
    depth: usize,
) {
    // Grok discovers a SKILL.md in a child at depth five, but does not descend
    // into that child's contents. Since this function owns the self check,
    // permit that final candidate directory and stop before its read_dir.
    if depth > MAX_SKILL_WALK_DEPTH + 1
        || budget.candidates_exhausted(out)
        || !budget.admit_directory(dir, diagnostics)
    {
        return;
    }

    let canonical_dir = match dunce::canonicalize(dir) {
        Ok(path) => path,
        Err(error) => {
            push_diagnostic(diagnostics, "skill.canonicalize_directory", dir, error);
            return;
        }
    };
    if !canonical_dir.starts_with(&plugin.canonical_root) {
        push_diagnostic(
            diagnostics,
            "skill.directory_escape",
            dir,
            "canonical skill directory escapes the plugin root",
        );
        return;
    }

    let skill = dir.join("SKILL.md");
    if skill.is_file()
        && !budget.push_candidate(
            Candidate {
                plugin,
                skill_root: skill_root.to_owned(),
                path: skill,
            },
            out,
            diagnostics,
        )
    {
        return;
    }

    if depth > MAX_SKILL_WALK_DEPTH {
        return;
    }

    let mut dirs = collect_child_directories(plugin, dir, budget, diagnostics);
    dirs.sort();
    for child in dirs {
        walk_for_skill_md(
            plugin,
            skill_root,
            &child,
            out,
            budget,
            diagnostics,
            depth + 1,
        );
        if budget.candidates_exhausted(out) {
            break;
        }
    }
}

fn collect_child_directories(
    plugin: &LoadedPlugin,
    dir: &Path,
    budget: &mut DiscoveryBudget,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Vec<PathBuf> {
    // Directory entry classification is advisory: a same-authority process
    // can replace an entry before the child canonicalize/read_dir step. Such a
    // replacement can consume only the bounded traversal budget, and every
    // resulting SKILL.md still passes the handle-based secure open before any
    // content is read. Ordinary symlinks and Windows reparse points are
    // rejected here to avoid intentionally walking their target trees.
    let remaining = MAX_SKILL_DIRECTORY_ENTRIES.saturating_sub(budget.directory_entries);
    if remaining == 0 {
        report_directory_entry_limit(dir, budget, diagnostics);
        return Vec::new();
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries.take(remaining + 1).collect::<Vec<_>>(),
        Err(error) => {
            push_diagnostic(diagnostics, "skill.read_directory", dir, error);
            return Vec::new();
        }
    };
    if entries.len() > remaining {
        budget.directory_entries = MAX_SKILL_DIRECTORY_ENTRIES;
        report_directory_entry_limit(dir, budget, diagnostics);
        return Vec::new();
    }
    budget.directory_entries += entries.len();

    entries
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => match entry.file_type() {
                Ok(file_type)
                    if entry_is_directory_link_or_reparse(&entry, &file_type, diagnostics) =>
                {
                    let path = entry.path();
                    let (code, message) = match dunce::canonicalize(&path) {
                        Ok(target) if !target.starts_with(&plugin.canonical_root) => (
                            "skill.path_escape",
                            "symlink skill directory escapes the plugin root",
                        ),
                        _ => (
                            "skill.directory_symlink",
                            "symlink skill directories are not traversed",
                        ),
                    };
                    push_diagnostic(diagnostics, code, &path, message);
                    None
                }
                Ok(file_type) if file_type.is_dir() => Some(entry.path()),
                Ok(_) => None,
                Err(error) => {
                    push_diagnostic(diagnostics, "skill.directory_entry_type", dir, error);
                    None
                }
            },
            Err(error) => {
                push_diagnostic(diagnostics, "skill.read_directory_entry", dir, error);
                None
            }
        })
        .collect()
}

#[cfg(not(windows))]
fn entry_is_directory_link_or_reparse(
    _entry: &fs::DirEntry,
    file_type: &fs::FileType,
    _diagnostics: &mut Vec<SkillDiagnostic>,
) -> bool {
    file_type.is_symlink()
}

#[cfg(windows)]
fn entry_is_directory_link_or_reparse(
    entry: &fs::DirEntry,
    file_type: &fs::FileType,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    if file_type.is_symlink() {
        return true;
    }
    match fs::symlink_metadata(entry.path()) {
        Ok(metadata) => metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        Err(error) => {
            push_diagnostic(
                diagnostics,
                "skill.directory_entry_metadata",
                &entry.path(),
                error,
            );
            // A classification failure cannot safely widen traversal.
            true
        }
    }
}

fn report_directory_entry_limit(
    path: &Path,
    budget: &mut DiscoveryBudget,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    if budget.directory_entry_limit_reported {
        return;
    }
    push_diagnostic(
        diagnostics,
        "skill.directory_entry_limit",
        path,
        format!("skill discovery exceeds {MAX_SKILL_DIRECTORY_ENTRIES} directory entries"),
    );
    budget.directory_entry_limit_reported = true;
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
    seen: &mut HashSet<(crate::PluginId, PathBuf)>,
    discovery: &mut SkillDiscovery,
) {
    let canonical_path = candidate.canonical_path;
    if !seen.insert((candidate.plugin.id.clone(), canonical_path.clone())) {
        return;
    }

    let mut file = match open_contained_file(&candidate.plugin.canonical_root, &canonical_path) {
        Ok(file) => file,
        Err(error) => {
            push_diagnostic(
                &mut discovery.diagnostics,
                "skill.secure_open",
                &canonical_path,
                error,
            );
            return;
        }
    };
    let metadata = match file.metadata() {
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
    if !metadata.is_file() {
        push_diagnostic(
            &mut discovery.diagnostics,
            "skill.not_regular_file",
            &canonical_path,
            "SKILL.md is not a regular file",
        );
        return;
    }
    if metadata.len() > MAX_SKILL_FILE_BYTES as u64 {
        push_diagnostic(
            &mut discovery.diagnostics,
            "skill.file_too_large",
            &canonical_path,
            format!("SKILL.md exceeds {MAX_SKILL_FILE_BYTES} bytes"),
        );
        return;
    }
    let mut bytes = Vec::new();
    let read_result = file
        .by_ref()
        .take(MAX_SKILL_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes);
    if let Err(error) = read_result {
        push_diagnostic(
            &mut discovery.diagnostics,
            "skill.read",
            &canonical_path,
            error,
        );
        return;
    }
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

/// Opens a canonical candidate without following any path component after the
/// containment decision. On Unix this walks from `/` using directory handles,
/// so renaming an ancestor or replacing any component with a symlink cannot
/// redirect the final open outside the plugin root.
#[cfg(unix)]
fn open_contained_file(plugin_root: &Path, candidate: &Path) -> io::Result<File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };

    candidate.strip_prefix(plugin_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "skill path escapes the plugin root",
        )
    })?;
    if !candidate.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "canonical skill path is not absolute",
        ));
    }

    let mut components = candidate
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(Ok(value)),
            Component::RootDir => None,
            _ => Some(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "canonical skill path contains an invalid component",
            ))),
        })
        .peekable();
    let root = File::open("/")?;
    let mut directory: Option<OwnedFd> = None;

    while let Some(component) = components.next() {
        let component = component?;
        let name = CString::new(component.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL")
        })?;
        let parent_fd = directory
            .as_ref()
            .map_or_else(|| root.as_raw_fd(), AsRawFd::as_raw_fd);
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if components.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                libc::O_NONBLOCK
            };
        // SAFETY: `parent_fd` remains owned for the call, `name` is a valid
        // NUL-terminated component, and a successful descriptor is immediately
        // transferred into `OwnedFd`.
        let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `openat` returned a new owned descriptor.
        directory = Some(unsafe { OwnedFd::from_raw_fd(fd) });
    }

    directory.map(File::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill path has no file component",
        )
    })
}

/// Windows opens once, resolves the object name from that same handle, and
/// requires it to equal the already containment-checked canonical candidate.
/// A path replacement can therefore only fail the comparison, never redirect
/// the subsequent metadata check or read.
#[cfg(windows)]
fn open_contained_file(plugin_root: &Path, candidate: &Path) -> io::Result<File> {
    use std::{
        ffi::OsString,
        os::windows::{ffi::OsStringExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };

    candidate.strip_prefix(plugin_root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "skill path escapes the plugin root",
        )
    })?;
    let file = File::open(candidate)?;
    // Windows extended-length paths contain at most 32,767 UTF-16 code units.
    // Keeping a fixed-size buffer makes the handle-path validation bounded.
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: the file handle is live for the call and the buffer exposes its
    // complete writable range.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize >= buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resolved skill path exceeds the Windows path bound",
        ));
    }
    let opened_path = PathBuf::from(OsString::from_wide(&buffer[..length as usize]));
    let opened_path = dunce::simplified(&opened_path);
    if !windows_paths_equal(opened_path, candidate) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "opened skill object does not match the contained canonical path",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_paths_equal(left: &Path, right: &Path) -> bool {
    left.as_os_str() == right.as_os_str()
}

#[cfg(not(any(unix, windows)))]
fn open_contained_file(_plugin_root: &Path, _candidate: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure skill traversal is not implemented on this platform",
    ))
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

    let frontmatter_name =
        parse_optional_scalar(&map, "name", "skill.invalid_name", path, diagnostics);
    let name = [frontmatter_name.as_deref(), fallback_name]
        .into_iter()
        .flatten()
        .map(normalize_skill_name)
        .find(|candidate| valid_skill_name(candidate))
        .ok_or_else(|| {
            "frontmatter and directory names do not yield a valid skill name".to_owned()
        })?;

    let authored_description = parse_optional_scalar(
        &map,
        "description",
        "skill.invalid_description",
        path,
        diagnostics,
    );
    let has_authored_description = authored_description.is_some();
    let description = authored_description
        .map(|value| truncate_chars(value, MAX_DESCRIPTION_CHARS))
        .or_else(|| derive_body_description(&body))
        .unwrap_or_else(|| name.clone());
    let when_key = if map.contains_key("when-to-use") {
        "when-to-use"
    } else {
        "when_to_use"
    };
    let when_to_use = parse_optional_scalar(
        &map,
        when_key,
        "skill.invalid_when_to_use",
        path,
        diagnostics,
    )
    .map(|value| truncate_chars(value, MAX_DESCRIPTION_CHARS));
    let argument_hint = parse_optional_scalar(
        &map,
        "argument-hint",
        "skill.invalid_argument_hint",
        path,
        diagnostics,
    )
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
        user_invocable: parse_optional_boolean(
            map.get("user-invocable"),
            true,
            "skill.invalid_user_invocable",
            path,
            diagnostics,
        ),
        disable_model_invocation: parse_optional_boolean(
            map.get("disable-model-invocation"),
            false,
            "skill.invalid_disable_model_invocation",
            path,
            diagnostics,
        ),
        body,
        paths,
        license: parse_optional_scalar(&map, "license", "skill.invalid_license", path, diagnostics),
        compatibility: parse_optional_scalar(
            &map,
            "compatibility",
            "skill.invalid_compatibility",
            path,
            diagnostics,
        ),
        metadata,
        model: parse_optional_scalar(&map, "model", "skill.invalid_model", path, diagnostics),
        effort: parse_optional_scalar(&map, "effort", "skill.invalid_effort", path, diagnostics),
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

fn parse_optional_scalar(
    values: &BTreeMap<String, Value>,
    key: &str,
    diagnostic_code: &'static str,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<String> {
    let value = values.get(key)?;
    let parsed = coerce_to_string(Some(value));
    if parsed.is_none() {
        push_diagnostic(
            diagnostics,
            diagnostic_code,
            path,
            format!("{key} must be a non-empty scalar"),
        );
    }
    parsed
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_boolean(value: &Value) -> bool {
    matches!(value, Value::Bool(true)) || matches!(value, Value::String(value) if value == "true")
}

fn parse_optional_boolean(
    value: Option<&Value>,
    default: bool,
    diagnostic_code: &'static str,
    path: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> bool {
    let Some(value) = value else {
        return default;
    };
    if !matches!(value, Value::Bool(_) | Value::String(_) | Value::Number(_)) {
        push_diagnostic(
            diagnostics,
            diagnostic_code,
            path,
            "boolean option must be a scalar",
        );
        return default;
    }
    parse_boolean(value)
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
            .filter_map(|value| match value.as_str().and_then(nonempty) {
                Some(value) => Some(value),
                None => {
                    push_diagnostic(
                        diagnostics,
                        "skill.invalid_allowed_tool",
                        path,
                        "allowed-tools list entry must be a non-empty string",
                    );
                    None
                }
            })
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
            .filter_map(|value| match value.as_str() {
                Some(value) => Some(value),
                None => {
                    push_diagnostic(
                        diagnostics,
                        "skill.invalid_path_entry",
                        path,
                        "paths list entry must be a string",
                    );
                    None
                }
            })
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
        .filter_map(|(key, value)| match (key.as_str(), value.as_str()) {
            (Some(key), Some(value)) => Some((key.to_owned(), value.to_owned())),
            _ => {
                push_diagnostic(
                    diagnostics,
                    "skill.invalid_metadata_entry",
                    path,
                    "metadata key and value must both be strings",
                );
                None
            }
        })
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
    extract_lead_block(&peek, false).or_else(|| extract_lead_block(&peek, true))
}

/// First top-level prose paragraph (and heading when requested), using the
/// pinned Grok Build Markdown event semantics. Lists, tables, code blocks,
/// blockquotes, and image alt text never become descriptions.
fn extract_lead_block(body: &str, include_headings: bool) -> Option<String> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut skip_depth = 0_usize;
    let mut image_depth = 0_usize;
    let mut capturing = false;
    let mut buffer = String::new();

    for event in Parser::new_ext(body, options) {
        match event {
            Event::Start(Tag::List(_) | Tag::BlockQuote(_)) => skip_depth += 1,
            Event::End(TagEnd::List(_) | TagEnd::BlockQuote(_)) => {
                skip_depth = skip_depth.saturating_sub(1);
            }
            Event::Start(Tag::Paragraph) if skip_depth == 0 => {
                capturing = true;
                buffer.clear();
            }
            Event::Start(Tag::Heading { .. }) if include_headings && skip_depth == 0 => {
                capturing = true;
                buffer.clear();
            }
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_)) if capturing => {
                let text = buffer.split_whitespace().collect::<Vec<_>>().join(" ");
                if !text.is_empty() {
                    return Some(truncate_chars(text, MAX_DESCRIPTION_CHARS));
                }
                capturing = false;
            }
            Event::Start(Tag::Image { .. }) => image_depth += 1,
            Event::End(TagEnd::Image) => image_depth = image_depth.saturating_sub(1),
            Event::Text(text) | Event::Code(text) if capturing && image_depth == 0 => {
                buffer.push_str(&text);
            }
            Event::SoftBreak | Event::HardBreak if capturing => buffer.push(' '),
            _ => {}
        }
    }
    None
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

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use super::open_contained_file;

    #[test]
    fn secure_open_rejects_ancestor_replaced_after_canonicalization() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_root = temp.path().join("plugin");
        let skill_dir = plugin_root.join("skills/example");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "inside").unwrap();
        fs::write(outside.join("SKILL.md"), "outside").unwrap();

        let canonical_root = dunce::canonicalize(&plugin_root).unwrap();
        let canonical_candidate = dunce::canonicalize(skill_dir.join("SKILL.md")).unwrap();
        fs::rename(&skill_dir, plugin_root.join("skills/original")).unwrap();
        symlink(&outside, &skill_dir).unwrap();

        let error = open_contained_file(&canonical_root, &canonical_candidate).unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

    use super::windows_paths_equal;

    #[test]
    fn handle_path_comparison_is_case_sensitive_and_lossless() {
        let upper = PathBuf::from(r"C:\Plugin\SKILL.md");
        let lower = PathBuf::from(r"c:\plugin\skill.md");
        assert!(!windows_paths_equal(&upper, &lower));

        let mut surrogate_path = "C:\\Plugin\\".encode_utf16().collect::<Vec<_>>();
        surrogate_path.push(0xd800);
        surrogate_path.extend("\\SKILL.md".encode_utf16());
        let surrogate_path = PathBuf::from(OsString::from_wide(&surrogate_path));

        let replacement_path = PathBuf::from("C:\\Plugin\\�\\SKILL.md");
        assert!(!windows_paths_equal(&surrogate_path, &replacement_path));
        assert!(windows_paths_equal(&surrogate_path, &surrogate_path));
    }
}
