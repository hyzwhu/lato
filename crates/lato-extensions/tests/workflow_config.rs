use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

use lato_extensions::{
    CapabilityCeiling, DiscoveryConfig, PluginComponentKind, PluginConfig, PluginSnapshot,
    WorkflowCapabilityCeiling, build_snapshot, discover_plugins, materialize_workflows,
};

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
        fs::create_dir_all(cwd.join(".lato/plugins")).unwrap();
        fs::create_dir_all(home.join("plugins")).unwrap();
        fs::create_dir_all(&plugins).unwrap();
        Self {
            _temp: temp,
            cwd,
            home,
            plugins,
        }
    }

    fn cli_plugin(&self, name: &str, plugin_json: &str) -> PathBuf {
        let root = self.plugins.join(name);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("plugin.json"), plugin_json).unwrap();
        root
    }

    fn project_plugin(&self, name: &str, plugin_json: &str) -> PathBuf {
        let root = self.cwd.join(".lato/plugins").join(name);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("plugin.json"), plugin_json).unwrap();
        root
    }

    fn snapshot(
        &self,
        project_trusted: bool,
        cli_dirs: Vec<PathBuf>,
        config: PluginConfig,
    ) -> Arc<PluginSnapshot> {
        let discovery = discover_plugins(&DiscoveryConfig {
            cwd: self.cwd.clone(),
            lato_home: self.home.clone(),
            cli_plugin_dirs: cli_dirs,
            project_trusted,
        });
        build_snapshot(42, discovery, &config).unwrap()
    }
}

#[test]
fn only_active_trusted_enabled_plugins_materialize() {
    let fixture = Fixture::new();
    let active = fixture.cli_plugin(
        "active",
        r#"{"name":"active","workflows":{"review-changes":{"description":"Review","agentBudget":32}}}"#,
    );
    let disabled = fixture.cli_plugin(
        "disabled",
        r#"{"name":"disabled","workflows":{"other":{}}}"#,
    );
    let snapshot = fixture.snapshot(
        true,
        vec![active, disabled],
        PluginConfig {
            enabled: vec![],
            disabled: vec!["disabled".into()],
        },
    );
    let set = materialize_workflows(&snapshot);
    assert_eq!(set.generation, 42);
    assert_eq!(set.workflows.len(), 1);
    assert_eq!(set.workflows[0].id, "active/review-changes");
    assert_eq!(set.workflows[0].agent_budget, 32);
    assert!(snapshot.active_plugins().any(|plugin| {
        plugin
            .components()
            .any(|kind| kind == PluginComponentKind::Workflows)
    }));
}

#[test]
fn untrusted_project_workflows_yield_empty_set() {
    let fixture = Fixture::new();
    fixture.project_plugin("proj", r#"{"name":"proj","workflows":{"secret":{}}}"#);
    let snapshot = fixture.snapshot(
        false,
        vec![],
        PluginConfig {
            enabled: vec!["proj".into()],
            disabled: vec![],
        },
    );
    assert!(snapshot.active_plugins().next().is_none());
    let set = materialize_workflows(&snapshot);
    assert!(set.workflows.is_empty());
}

#[test]
fn default_budget_and_invalid_budget_are_isolated() {
    let fixture = Fixture::new();
    let root = fixture.cli_plugin(
        "budgets",
        r#"{"name":"budgets","workflows":{"ok":{},"zero":{"agentBudget":0},"huge":{"agentBudget":1025}}}"#,
    );
    let snapshot = fixture.snapshot(
        true,
        vec![root],
        PluginConfig {
            enabled: vec![],
            disabled: vec![],
        },
    );
    let set = materialize_workflows(&snapshot);
    assert_eq!(set.workflows.len(), 1);
    assert_eq!(set.workflows[0].name, "ok");
    assert_eq!(set.workflows[0].agent_budget, 128);
    assert!(
        set.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "workflow.invalid_budget")
    );
}

#[test]
fn collision_keeps_first() {
    let fixture = Fixture::new();
    let root = fixture.cli_plugin(
        "dup",
        r#"{"name":"dup","workflows":{"Review":{"description":"first"},"review":{"description":"second"}}}"#,
    );
    let snapshot = fixture.snapshot(
        true,
        vec![root],
        PluginConfig {
            enabled: vec![],
            disabled: vec![],
        },
    );
    let set = materialize_workflows(&snapshot);
    assert_eq!(set.workflows.len(), 1);
    assert_eq!(set.workflows[0].description, "first");
    assert!(
        set.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "workflow.collision")
    );
}

#[test]
fn child_ceiling_narrows_and_cannot_restore() {
    let fixture = Fixture::new();
    let root = fixture.cli_plugin(
        "pack",
        r#"{"name":"pack","workflows":{"alpha":{},"beta":{}}}"#,
    );
    let parent = fixture.snapshot(
        true,
        vec![root],
        PluginConfig {
            enabled: vec![],
            disabled: vec![],
        },
    );
    let child = parent.derive_child(&CapabilityCeiling {
        parent: vec![lato_core::ToolCapability::ExtensionInvoke],
        profile: vec![lato_core::ToolCapability::ExtensionInvoke],
        workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
        mcp: Default::default(),
        workflows: WorkflowCapabilityCeiling {
            allowed: Some(BTreeSet::from(["pack/alpha".into()])),
        },
    });
    let set = materialize_workflows(&child);
    assert_eq!(set.workflows.len(), 1);
    assert_eq!(set.workflows[0].id, "pack/alpha");

    let restored = child.derive_child(&CapabilityCeiling {
        parent: vec![lato_core::ToolCapability::ExtensionInvoke],
        profile: vec![lato_core::ToolCapability::ExtensionInvoke],
        workspace: vec![lato_core::ToolCapability::ExtensionInvoke],
        mcp: Default::default(),
        workflows: WorkflowCapabilityCeiling {
            allowed: Some(BTreeSet::from(["pack/alpha".into(), "pack/beta".into()])),
        },
    });
    let restored_set = materialize_workflows(&restored);
    assert_eq!(restored_set.workflows.len(), 1);
    assert_eq!(restored_set.workflows[0].id, "pack/alpha");
}

#[test]
fn malformed_file_is_isolated() {
    let fixture = Fixture::new();
    let root = fixture.cli_plugin("broken", r#"{"name":"broken"}"#);
    fs::write(root.join("workflows.json"), "{").unwrap();
    let snapshot = fixture.snapshot(
        true,
        vec![root],
        PluginConfig {
            enabled: vec![],
            disabled: vec![],
        },
    );
    let set = materialize_workflows(&snapshot);
    assert!(set.workflows.is_empty());
    assert!(
        set.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "workflow.config_invalid")
    );
}
