use std::{fs, path::PathBuf, sync::Arc};

use lato_core::ToolCapability;
use lato_extensions::{
    CapabilityCeiling, DiscoveryConfig, PluginConfig, PluginScope, build_snapshot, discover_plugins,
};

#[test]
fn active_requires_both_trust_and_enablement() {
    let fixture = RegistryFixture::new();
    let cli = fixture.plugin(Scope::Cli, "cli");
    fixture.plugin(Scope::User, "user");
    fixture.plugin(Scope::Project, "project");
    let snapshot = fixture.snapshot(
        false,
        vec![cli],
        PluginConfig {
            enabled: vec!["user".into(), "project".into()],
            disabled: vec![],
        },
    );
    assert_eq!(snapshot.active_names(), vec!["cli", "user"]);
}

#[test]
fn cli_defaults_enabled_while_project_and_user_default_disabled() {
    let fixture = RegistryFixture::new();
    let cli = fixture.plugin(Scope::Cli, "cli");
    fixture.plugin(Scope::User, "user");
    fixture.plugin(Scope::Project, "project");
    let snapshot = fixture.snapshot(true, vec![cli], PluginConfig::default());
    assert_eq!(snapshot.active_names(), vec!["cli"]);
    assert!(
        snapshot
            .plugins()
            .iter()
            .filter(|plugin| plugin.scope != PluginScope::CliOverride)
            .all(|plugin| !plugin.enabled)
    );
}

#[test]
fn explicit_disable_precedes_enable_and_unknown_names_are_diagnosed() {
    let fixture = RegistryFixture::new();
    let cli = fixture.plugin(Scope::Cli, "cli");
    let snapshot = fixture.snapshot(
        true,
        vec![cli],
        PluginConfig {
            enabled: vec!["cli".into(), "absent".into()],
            disabled: vec!["cli".into()],
        },
    );
    assert!(snapshot.active_plugins().next().is_none());
    assert!(
        snapshot
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "plugin.config_conflict")
    );
    assert!(
        snapshot
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "plugin.config_unknown")
    );
}

#[test]
fn child_derivation_can_only_remove_extension_capability() {
    let fixture = RegistryFixture::new();
    let cli = fixture.plugin_with_components(Scope::Cli, "all");
    let parent = fixture.snapshot(true, vec![cli], PluginConfig::default());
    let child = parent.derive_child(&CapabilityCeiling {
        parent: vec![ToolCapability::ExtensionInvoke, ToolCapability::FileRead],
        profile: vec![ToolCapability::FileRead],
        workspace: vec![ToolCapability::FileRead],
        mcp: Default::default(),
        workflows: Default::default(),
    });
    assert!(child.active_plugins().next().is_none());
    assert_eq!(child.parent_generation(), Some(parent.generation()));
    assert!(child.plugins()[0].components().next().is_none());

    let allowed = parent.derive_child(&CapabilityCeiling {
        parent: vec![ToolCapability::ExtensionInvoke],
        profile: vec![ToolCapability::ExtensionInvoke],
        workspace: vec![ToolCapability::ExtensionInvoke],
        mcp: Default::default(),
        workflows: Default::default(),
    });
    assert_eq!(allowed.active_names(), vec!["all"]);
}

#[test]
fn snapshot_arc_and_ids_are_stable_and_immutable() {
    let fixture = RegistryFixture::new();
    let cli = fixture.plugin(Scope::Cli, "stable");
    let first = fixture.snapshot(true, vec![cli.clone()], PluginConfig::default());
    let clone = Arc::clone(&first);
    assert!(Arc::ptr_eq(&first, &clone));
    let second = fixture.snapshot(true, vec![cli], PluginConfig::default());
    assert_eq!(first.plugins()[0].id, second.plugins()[0].id);
    assert_eq!(first.generation(), 7);
    assert!(first.built_at_ms() > 0);
}

enum Scope {
    Cli,
    Project,
    User,
}

struct RegistryFixture {
    _root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    cli: PathBuf,
}

impl RegistryFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        let home = root.path().join("home");
        let cli = root.path().join("cli");
        fs::create_dir_all(cwd.join(".lato/plugins")).unwrap();
        fs::create_dir_all(home.join("plugins")).unwrap();
        fs::create_dir_all(&cli).unwrap();
        Self {
            _root: root,
            cwd,
            home,
            cli,
        }
    }

    fn plugin(&self, scope: Scope, name: &str) -> PathBuf {
        let parent = match scope {
            Scope::Cli => self.cli.clone(),
            Scope::Project => self.cwd.join(".lato/plugins"),
            Scope::User => self.home.join("plugins"),
        };
        let root = parent.join(name);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("plugin.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
        root
    }

    fn plugin_with_components(&self, scope: Scope, name: &str) -> PathBuf {
        let root = self.plugin(scope, name);
        fs::create_dir_all(root.join("skills/demo")).unwrap();
        fs::write(root.join("skills/demo/SKILL.md"), "# demo").unwrap();
        fs::create_dir_all(root.join("hooks")).unwrap();
        fs::write(root.join("hooks/hooks.json"), "{}").unwrap();
        fs::write(root.join(".mcp.json"), "{}").unwrap();
        root
    }

    fn snapshot(
        &self,
        project_trusted: bool,
        cli_plugin_dirs: Vec<PathBuf>,
        config: PluginConfig,
    ) -> Arc<lato_extensions::PluginSnapshot> {
        let discovery = discover_plugins(&DiscoveryConfig {
            cwd: self.cwd.clone(),
            lato_home: self.home.clone(),
            cli_plugin_dirs,
            project_trusted,
        });
        build_snapshot(7, discovery, &config).unwrap()
    }
}

#[test]
fn child_mcp_ceiling_narrows_and_cannot_restore() {
    use std::collections::BTreeSet;

    use lato_extensions::McpCapabilityCeiling;

    let fixture = RegistryFixture::new();
    let cli = fixture.plugin_with_components(Scope::Cli, "all");
    let parent = fixture.snapshot(true, vec![cli], PluginConfig::default());
    assert!(parent.mcp_ceiling().allowed_servers.is_none());

    let child = parent.derive_child(&CapabilityCeiling {
        parent: vec![ToolCapability::ExtensionInvoke],
        profile: vec![ToolCapability::ExtensionInvoke],
        workspace: vec![ToolCapability::ExtensionInvoke],
        mcp: McpCapabilityCeiling {
            allowed_servers: Some(BTreeSet::from(["demo".into()])),
            allowed_tools: Some(BTreeSet::from(["demo__ping".into()])),
        },
        workflows: Default::default(),
    });
    assert_eq!(
        child.mcp_ceiling().allowed_servers,
        Some(BTreeSet::from(["demo".into()]))
    );
    assert_eq!(
        child.mcp_ceiling().allowed_tools,
        Some(BTreeSet::from(["demo__ping".into()]))
    );

    let grandchild = child.derive_child(&CapabilityCeiling {
        parent: vec![ToolCapability::ExtensionInvoke],
        profile: vec![ToolCapability::ExtensionInvoke],
        workspace: vec![ToolCapability::ExtensionInvoke],
        mcp: McpCapabilityCeiling {
            allowed_servers: Some(BTreeSet::from(["demo".into(), "other".into()])),
            allowed_tools: Some(BTreeSet::from(["demo__ping".into(), "demo__secret".into()])),
        },
        workflows: Default::default(),
    });
    assert_eq!(
        grandchild.mcp_ceiling().allowed_servers,
        Some(BTreeSet::from(["demo".into()])),
        "child cannot restore a server absent from parent grant"
    );
    assert_eq!(
        grandchild.mcp_ceiling().allowed_tools,
        Some(BTreeSet::from(["demo__ping".into()])),
        "child cannot restore a tool absent from parent grant"
    );
}

#[test]
fn extension_invoke_denial_clears_mcp_ceiling() {
    use lato_extensions::McpCapabilityCeiling;

    let fixture = RegistryFixture::new();
    let cli = fixture.plugin_with_components(Scope::Cli, "all");
    let parent = fixture.snapshot(true, vec![cli], PluginConfig::default());
    let child = parent.derive_child(&CapabilityCeiling {
        parent: vec![ToolCapability::FileRead],
        profile: vec![ToolCapability::FileRead],
        workspace: vec![ToolCapability::FileRead],
        mcp: McpCapabilityCeiling {
            allowed_servers: None,
            allowed_tools: None,
        },
        workflows: Default::default(),
    });
    assert_eq!(child.mcp_ceiling(), &McpCapabilityCeiling::deny_all());
    assert!(child.active_plugins().next().is_none());
}
