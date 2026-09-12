//! Resolve user, project, and plugin workflow scripts.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use lato_extensions::{PluginSnapshot, materialize_workflows};
use lato_workflow::{
    DEFAULT_AGENT_BUDGET, WorkflowError, compile_declarative_workflow, extract_meta,
};

const MAX_WORKFLOW_SOURCE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedWorkflow {
    pub id: String,
    pub display_name: String,
    pub script: String,
    pub agent_budget: u32,
    pub source: &'static str,
    pub compiled: bool,
}

struct CatalogEntry {
    workflow: ResolvedWorkflow,
    short_name: String,
}

struct Catalog {
    entries: Vec<CatalogEntry>,
    duplicate_short: HashSet<String>,
}

pub fn list_workflows(
    cwd: &Path,
    lato_home: &Path,
    snapshot: &PluginSnapshot,
    project_trusted: bool,
) -> Vec<ResolvedWorkflow> {
    scan(cwd, lato_home, snapshot, project_trusted).list()
}

pub fn resolve_workflow(
    cwd: &Path,
    lato_home: &Path,
    snapshot: &PluginSnapshot,
    project_trusted: bool,
    id: &str,
) -> Result<ResolvedWorkflow, WorkflowError> {
    scan(cwd, lato_home, snapshot, project_trusted).resolve(id)
}

fn scan(cwd: &Path, lato_home: &Path, snapshot: &PluginSnapshot, project_trusted: bool) -> Catalog {
    let mut entries = Vec::new();
    let mut duplicate_short = HashSet::new();
    let mut seen_file_names = HashSet::new();

    let mut user = scan_directory(&lato_home.join("workflows"), "user");
    let user_dups = scope_duplicate_names(&user);
    duplicate_short.extend(user_dups.iter().cloned());
    user.retain(|entry| !user_dups.contains(&entry.short_name));
    for entry in user {
        seen_file_names.insert(entry.short_name.clone());
        entries.push(entry);
    }

    if project_trusted {
        let mut project = scan_directory(
            &project_root(cwd).join(".lato").join("workflows"),
            "project",
        );
        let project_dups = scope_duplicate_names(&project);
        duplicate_short.extend(project_dups.iter().cloned());
        project.retain(|entry| !project_dups.contains(&entry.short_name));
        for entry in project {
            if seen_file_names.contains(&entry.short_name) {
                continue;
            }
            seen_file_names.insert(entry.short_name.clone());
            entries.push(entry);
        }
    }

    let plugins = plugin_entries(snapshot);
    duplicate_short.extend(scope_duplicate_names(&plugins));
    entries.extend(plugins);

    Catalog {
        entries,
        duplicate_short,
    }
}

impl Catalog {
    fn list(&self) -> Vec<ResolvedWorkflow> {
        let file_names = self
            .entries
            .iter()
            .filter(|entry| entry.workflow.source != "plugin")
            .map(|entry| entry.short_name.clone())
            .collect::<HashSet<_>>();
        self.entries
            .iter()
            .filter(|entry| {
                entry.workflow.source != "plugin" || !file_names.contains(&entry.short_name)
            })
            .map(|entry| entry.workflow.clone())
            .collect()
    }

    fn resolve(&self, id: &str) -> Result<ResolvedWorkflow, WorkflowError> {
        if let Some(entry) = self.entries.iter().find(|entry| entry.workflow.id == id) {
            return Ok(entry.workflow.clone());
        }

        let matches = self
            .entries
            .iter()
            .filter(|entry| entry.short_name == id)
            .collect::<Vec<_>>();
        let file_matches = matches
            .iter()
            .copied()
            .filter(|entry| entry.workflow.source != "plugin")
            .collect::<Vec<_>>();
        if file_matches.len() > 1 {
            return Err(WorkflowError::InvalidConfiguration(
                "workflow.duplicate_name".into(),
            ));
        }
        if let Some(entry) = file_matches.first() {
            return Ok(entry.workflow.clone());
        }

        let plugin_matches = matches
            .iter()
            .copied()
            .filter(|entry| entry.workflow.source == "plugin")
            .collect::<Vec<_>>();
        if plugin_matches.len() > 1
            || (plugin_matches.is_empty() && self.duplicate_short.contains(id))
        {
            return Err(WorkflowError::InvalidConfiguration(
                "workflow.duplicate_name".into(),
            ));
        }
        if let Some(entry) = plugin_matches.first() {
            return Ok(entry.workflow.clone());
        }

        Err(WorkflowError::NotFound(id.to_owned()))
    }
}

fn plugin_entries(snapshot: &PluginSnapshot) -> Vec<CatalogEntry> {
    materialize_workflows(snapshot)
        .workflows
        .iter()
        .map(|descriptor| {
            let script = compile_declarative_workflow(descriptor);
            let short_name = extract_meta(&script)
                .map(|meta| meta.name)
                .unwrap_or_else(|_| descriptor.name.replace('_', "-"));
            CatalogEntry {
                workflow: ResolvedWorkflow {
                    id: descriptor.id.clone(),
                    display_name: short_name.clone(),
                    script,
                    agent_budget: descriptor.agent_budget,
                    source: "plugin",
                    compiled: true,
                },
                short_name,
            }
        })
        .collect()
}

fn scan_directory(dir: &Path, source: &'static str) -> Vec<CatalogEntry> {
    let Ok(dir_meta) = std::fs::symlink_metadata(dir) else {
        return Vec::new();
    };
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return Vec::new();
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rhai"))
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    paths
        .into_iter()
        .filter_map(|path| load_file_workflow(&path, source))
        .collect()
}

fn load_file_workflow(path: &Path, source: &'static str) -> Option<CatalogEntry> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return None;
    }
    let script = std::fs::read_to_string(path).ok()?;
    if script.len() as u64 > MAX_WORKFLOW_SOURCE_BYTES {
        return None;
    }
    let workflow_meta = extract_meta(&script).ok()?;
    let filename = path.file_name()?.to_str()?;
    if filename != format!("{}.rhai", workflow_meta.name) {
        return None;
    }
    Some(CatalogEntry {
        workflow: ResolvedWorkflow {
            id: workflow_meta.name.clone(),
            display_name: workflow_meta.name.clone(),
            script,
            agent_budget: DEFAULT_AGENT_BUDGET,
            source,
            compiled: false,
        },
        short_name: workflow_meta.name,
    })
}

fn scope_duplicate_names(entries: &[CatalogEntry]) -> HashSet<String> {
    let mut counts = HashMap::<String, usize>::new();
    for entry in entries {
        *counts.entry(entry.short_name.clone()).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect()
}

fn project_root(cwd: &Path) -> PathBuf {
    git_toplevel(cwd)
        .or_else(|| walk_git_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf())
}

fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn walk_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use lato_extensions::{
        DiscoveryConfig, PluginConfig, PluginSnapshot, build_snapshot, discover_plugins,
    };
    use lato_workflow::WorkflowError;

    use super::{list_workflows, resolve_workflow};

    struct Fixture {
        _temp: tempfile::TempDir,
        cwd: PathBuf,
        home: PathBuf,
        plugins: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let cwd = temp.path().join("workspace");
            let home = temp.path().join("home");
            let plugins = temp.path().join("plugins");
            fs::create_dir_all(home.join("workflows")).unwrap();
            fs::create_dir_all(&cwd).unwrap();
            fs::create_dir_all(&plugins).unwrap();
            Self {
                _temp: temp,
                cwd,
                home,
                plugins,
            }
        }

        fn write_rhai(dir: &std::path::Path, name: &str, description: &str) {
            fs::create_dir_all(dir).unwrap();
            fs::write(
                dir.join(format!("{name}.rhai")),
                rhai_script(name, description),
            )
            .unwrap();
        }

        fn write_user(&self, name: &str, description: &str) {
            Self::write_rhai(&self.home.join("workflows"), name, description);
        }

        fn write_project(&self, name: &str, description: &str) {
            Self::write_rhai(&self.cwd.join(".lato/workflows"), name, description);
        }

        fn cli_plugin(&self, name: &str, plugin_json: &str) -> PathBuf {
            let root = self.plugins.join(name);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("plugin.json"), plugin_json).unwrap();
            root
        }

        fn snapshot(&self, project_trusted: bool, cli_dirs: Vec<PathBuf>) -> Arc<PluginSnapshot> {
            build_snapshot(
                1,
                discover_plugins(&DiscoveryConfig {
                    cwd: self.cwd.clone(),
                    lato_home: self.home.clone(),
                    cli_plugin_dirs: cli_dirs,
                    project_trusted,
                }),
                &PluginConfig::default(),
            )
            .unwrap()
        }
    }

    fn rhai_script(name: &str, description: &str) -> String {
        format!(
            "let meta = #{{\n    name: \"{name}\",\n    description: \"{description}\",\n}};\ncomplete(\"ok\");\n"
        )
    }

    #[test]
    fn user_file_is_listed() {
        let fixture = Fixture::new();
        fixture.write_user("user-greet", "Say hello");
        let snapshot = fixture.snapshot(false, vec![]);
        let listed = list_workflows(&fixture.cwd, &fixture.home, &snapshot, false);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "user-greet");
        assert_eq!(listed[0].display_name, "user-greet");
        assert_eq!(listed[0].source, "user");
        assert!(!listed[0].compiled);
        assert_eq!(listed[0].agent_budget, 128);
        assert!(listed[0].script.contains("name: \"user-greet\""));
        let resolved =
            resolve_workflow(&fixture.cwd, &fixture.home, &snapshot, false, "user-greet").unwrap();
        assert_eq!(resolved.id, "user-greet");
        assert_eq!(resolved.source, "user");
    }

    #[test]
    fn untrusted_project_dir_is_ignored() {
        let fixture = Fixture::new();
        fixture.write_user("user-greet", "Say hello");
        fixture.write_project("proj-secret", "Hidden");
        let snapshot = fixture.snapshot(false, vec![]);
        let listed = list_workflows(&fixture.cwd, &fixture.home, &snapshot, false);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "user-greet");
        let err = resolve_workflow(&fixture.cwd, &fixture.home, &snapshot, false, "proj-secret")
            .unwrap_err();
        assert!(matches!(err, WorkflowError::NotFound(id) if id == "proj-secret"));
    }

    #[test]
    fn trusted_project_file_is_listed() {
        let fixture = Fixture::new();
        fixture.write_project("proj-greet", "Project hello");
        let snapshot = fixture.snapshot(true, vec![]);
        let listed = list_workflows(&fixture.cwd, &fixture.home, &snapshot, true);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "proj-greet");
        assert_eq!(listed[0].source, "project");
        assert!(!listed[0].compiled);
    }

    #[test]
    fn plugin_json_compiles_with_compiled_true() {
        let fixture = Fixture::new();
        let plugin = fixture.cli_plugin(
            "demo",
            r#"{"name":"demo","workflows":{"review-changes":{"description":"Review a diff","prompt":"Inspect the patch","profile":"explorer","agentBudget":32}}}"#,
        );
        let snapshot = fixture.snapshot(true, vec![plugin]);
        let listed = list_workflows(&fixture.cwd, &fixture.home, &snapshot, true);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "demo/review-changes");
        assert_eq!(listed[0].display_name, "review-changes");
        assert_eq!(listed[0].source, "plugin");
        assert!(listed[0].compiled);
        assert_eq!(listed[0].agent_budget, 32);
        assert!(listed[0].script.contains("name: \"review-changes\""));
        assert!(listed[0].script.contains("agent_type: \"explorer\""));
        let by_qualified = resolve_workflow(
            &fixture.cwd,
            &fixture.home,
            &snapshot,
            true,
            "demo/review-changes",
        )
        .unwrap();
        assert_eq!(by_qualified.id, "demo/review-changes");
        let by_short = resolve_workflow(
            &fixture.cwd,
            &fixture.home,
            &snapshot,
            true,
            "review-changes",
        )
        .unwrap();
        assert_eq!(by_short.id, "demo/review-changes");
    }

    #[test]
    fn unknown_id_is_not_found() {
        let fixture = Fixture::new();
        let snapshot = fixture.snapshot(false, vec![]);
        let err = resolve_workflow(
            &fixture.cwd,
            &fixture.home,
            &snapshot,
            false,
            "missing/none",
        )
        .unwrap_err();
        assert!(matches!(err, WorkflowError::NotFound(id) if id == "missing/none"));
    }

    #[test]
    fn user_short_name_shadows_project_and_plugin() {
        let fixture = Fixture::new();
        fixture.write_user("review-changes", "User copy");
        fixture.write_project("review-changes", "Project copy");
        let plugin = fixture.cli_plugin(
            "demo",
            r#"{"name":"demo","workflows":{"review-changes":{"description":"Plugin copy"}}}"#,
        );
        let snapshot = fixture.snapshot(true, vec![plugin]);
        let listed = list_workflows(&fixture.cwd, &fixture.home, &snapshot, true);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source, "user");
        let resolved = resolve_workflow(
            &fixture.cwd,
            &fixture.home,
            &snapshot,
            true,
            "review-changes",
        )
        .unwrap();
        assert_eq!(resolved.source, "user");
        let qualified = resolve_workflow(
            &fixture.cwd,
            &fixture.home,
            &snapshot,
            true,
            "demo/review-changes",
        )
        .unwrap();
        assert_eq!(qualified.source, "plugin");
        assert!(qualified.compiled);
    }

    #[test]
    fn same_scope_duplicate_short_name_is_invalid() {
        let fixture = Fixture::new();
        let alpha = fixture.cli_plugin(
            "alpha",
            r#"{"name":"alpha","workflows":{"shared":{"description":"Alpha"}}}"#,
        );
        let beta = fixture.cli_plugin(
            "beta",
            r#"{"name":"beta","workflows":{"shared":{"description":"Beta"}}}"#,
        );
        let snapshot = fixture.snapshot(true, vec![alpha, beta]);
        let listed = list_workflows(&fixture.cwd, &fixture.home, &snapshot, true);
        assert_eq!(listed.len(), 2);
        let err =
            resolve_workflow(&fixture.cwd, &fixture.home, &snapshot, true, "shared").unwrap_err();
        assert!(
            matches!(err, WorkflowError::InvalidConfiguration(code) if code == "workflow.duplicate_name")
        );
        assert_eq!(
            resolve_workflow(&fixture.cwd, &fixture.home, &snapshot, true, "alpha/shared")
                .unwrap()
                .id,
            "alpha/shared"
        );
    }

    #[test]
    fn skips_symlink_oversized_and_filename_mismatch() {
        let fixture = Fixture::new();
        let dir = fixture.home.join("workflows");
        Fixture::write_rhai(&dir, "ok", "Keep me");
        fs::write(dir.join("mismatch.rhai"), rhai_script("other-name", "Nope")).unwrap();
        fs::write(dir.join("huge.rhai"), "x".repeat(1024 * 1024 + 1)).unwrap();
        let link = dir.join("link.rhai");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("ok.rhai"), &link).unwrap();
        let snapshot = fixture.snapshot(false, vec![]);
        let listed = list_workflows(&fixture.cwd, &fixture.home, &snapshot, false);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "ok");
    }

    #[test]
    fn project_workflows_use_git_root() {
        let fixture = Fixture::new();
        let nested = fixture.cwd.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let git = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&fixture.cwd)
            .output()
            .unwrap();
        assert!(
            git.status.success(),
            "{}",
            String::from_utf8_lossy(&git.stderr)
        );
        fixture.write_project("from-root", "At git root");
        let snapshot = fixture.snapshot(true, vec![]);
        let listed = list_workflows(&nested, &fixture.home, &snapshot, true);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "from-root");
        assert_eq!(listed[0].source, "project");
    }
}
