use std::{fs, path::PathBuf};

use lato_extensions::{
    DiscoveryConfig, MAX_DIAGNOSTIC_MESSAGE_BYTES, MAX_DISCOVERY_DIAGNOSTICS, PluginScope,
    discover_plugins,
};

#[test]
fn source_precedence_and_project_trust_match_grok_build() {
    let fixture = DiscoveryFixture::new();
    fixture.plugin(Source::User, "user-same", r#"{"name":"same"}"#);
    fixture.plugin(Source::Project, "project-same", r#"{"name":"same"}"#);
    let cli = fixture.plugin(Source::Cli, "cli-same", r#"{"name":"same"}"#);

    let untrusted = discover_plugins(&fixture.config(false, vec![cli]));
    let winner = untrusted
        .plugins
        .iter()
        .find(|plugin| plugin.name() == "same")
        .unwrap();
    assert_eq!(winner.scope, PluginScope::CliOverride);
    assert!(winner.trusted);
    assert!(winner.conflict.is_some());

    let project_only = discover_plugins(&fixture.config(false, vec![]));
    let project = project_only
        .plugins
        .iter()
        .find(|plugin| plugin.scope == PluginScope::Project)
        .unwrap();
    assert!(!project.trusted);
}

#[test]
fn cli_paths_are_direct_roots_and_parent_sources_scan_only_children() {
    let fixture = DiscoveryFixture::new();
    let cli = fixture.plugin(Source::Cli, "direct", r#"{"name":"direct"}"#);
    fixture.plugin(Source::User, "child", r#"{"name":"child"}"#);
    let nested = fixture.user_plugins().join("child/nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("plugin.json"), r#"{"name":"nested"}"#).unwrap();

    let result = discover_plugins(&fixture.config(true, vec![cli]));
    assert_eq!(
        result
            .plugins
            .iter()
            .map(|plugin| plugin.name())
            .collect::<Vec<_>>(),
        vec!["direct", "child"]
    );
}

#[test]
fn same_scope_winner_is_stable_by_path() {
    let fixture = DiscoveryFixture::new();
    fixture.plugin(Source::User, "z-last", r#"{"name":"duplicate"}"#);
    let expected = fixture.plugin(Source::User, "a-first", r#"{"name":"duplicate"}"#);
    for _ in 0..2 {
        let result = discover_plugins(&fixture.config(true, vec![]));
        let winner = result.plugins.first().unwrap();
        assert_eq!(
            winner.canonical_root,
            dunce::canonicalize(&expected).unwrap()
        );
    }
}

#[test]
fn invalid_manifest_and_missing_cli_root_are_isolated() {
    let fixture = DiscoveryFixture::new();
    fixture.plugin(Source::User, "valid", r#"{"name":"valid"}"#);
    fixture.plugin(Source::User, "invalid", "{");
    let result =
        discover_plugins(&fixture.config(true, vec![fixture.root.path().join("does-not-exist")]));
    assert_eq!(result.plugins.len(), 1);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "plugin.manifest_invalid")
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "plugin.root_unreadable")
    );
}

#[test]
fn diagnostics_are_count_and_byte_bounded() {
    let fixture = DiscoveryFixture::new();
    let roots = (0..(MAX_DISCOVERY_DIAGNOSTICS + 50))
        .map(|index| {
            fixture
                .root
                .path()
                .join(format!("{}-{index}", "x".repeat(700)))
        })
        .collect();
    let result = discover_plugins(&fixture.config(true, roots));
    assert_eq!(result.diagnostics.len(), MAX_DISCOVERY_DIAGNOSTICS);
    assert!(result.diagnostics.iter().all(|diagnostic| {
        diagnostic.message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES
            && diagnostic
                .path
                .as_ref()
                .is_none_or(|path| path.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES)
    }));
}

#[cfg(unix)]
#[test]
fn canonical_alias_is_loaded_once_and_broken_alias_is_diagnosed() {
    let fixture = DiscoveryFixture::new();
    let plugin = fixture.plugin(Source::User, "demo", r#"{"name":"demo"}"#);
    let alias = fixture.root.path().join("alias");
    std::os::unix::fs::symlink(&plugin, &alias).unwrap();
    let broken = fixture.root.path().join("broken");
    std::os::unix::fs::symlink(fixture.root.path().join("absent"), &broken).unwrap();
    let result = discover_plugins(&fixture.config(true, vec![alias, plugin, broken]));
    assert_eq!(
        result
            .plugins
            .iter()
            .filter(|candidate| candidate.name() == "demo")
            .count(),
        1
    );
    assert!(result.diagnostics.len() >= 2);
}

enum Source {
    Cli,
    Project,
    User,
}

struct DiscoveryFixture {
    root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    cli: PathBuf,
}

impl DiscoveryFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        let home = root.path().join("home");
        let cli = root.path().join("cli");
        fs::create_dir_all(cwd.join(".lato/plugins")).unwrap();
        fs::create_dir_all(home.join("plugins")).unwrap();
        fs::create_dir_all(&cli).unwrap();
        Self {
            root,
            cwd,
            home,
            cli,
        }
    }

    fn plugin(&self, source: Source, dirname: &str, manifest: &str) -> PathBuf {
        let parent = match source {
            Source::Cli => self.cli.clone(),
            Source::Project => self.cwd.join(".lato/plugins"),
            Source::User => self.home.join("plugins"),
        };
        let root = parent.join(dirname);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("plugin.json"), manifest).unwrap();
        root
    }

    fn user_plugins(&self) -> PathBuf {
        self.home.join("plugins")
    }

    fn config(&self, project_trusted: bool, cli_plugin_dirs: Vec<PathBuf>) -> DiscoveryConfig {
        DiscoveryConfig {
            cwd: self.cwd.clone(),
            lato_home: self.home.clone(),
            cli_plugin_dirs,
            project_trusted,
        }
    }
}
