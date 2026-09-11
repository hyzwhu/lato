use std::{
    fs,
    path::PathBuf,
    sync::Arc,
};

use lato_extensions::{
    DiscoveryConfig, PluginConfig, PluginSnapshot, build_snapshot, discover_plugins,
    materialize_mcp,
};
use lato_mcp::{McpTransportKind, qualify_tool};

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
        r#"{"name":"active","mcpServers":{"demo":{"command":"node","args":["a.js"]}}}"#,
    );
    let disabled = fixture.cli_plugin(
        "disabled",
        r#"{"name":"disabled","mcpServers":{"other":{"command":"node"}}}"#,
    );
    let snapshot = fixture.snapshot(
        true,
        vec![active, disabled],
        PluginConfig {
            enabled: vec![],
            disabled: vec!["disabled".into()],
        },
    );
    let set = materialize_mcp(&snapshot);
    assert_eq!(set.generation, 42);
    assert_eq!(set.servers.len(), 1);
    assert_eq!(set.servers[0].plugin_name, "active");
    assert_eq!(set.servers[0].server_name, "demo");
    assert_eq!(set.servers[0].id, "active/demo");
    assert_eq!(set.servers[0].transport, McpTransportKind::Stdio);
}

#[test]
fn untrusted_project_mcp_json_yields_empty_set() {
    let fixture = Fixture::new();
    let root = fixture.project_plugin("proj", r#"{"name":"proj"}"#);
    fs::write(
        root.join(".mcp.json"),
        r#"{"mcpServers":{"secret":{"command":"node","args":["leak.js"]}}}"#,
    )
    .unwrap();
    let snapshot = fixture.snapshot(
        false,
        vec![],
        PluginConfig {
            enabled: vec!["proj".into()],
            disabled: vec![],
        },
    );
    assert!(snapshot.active_plugins().next().is_none());
    let set = materialize_mcp(&snapshot);
    assert!(set.servers.is_empty());
    assert!(set.diagnostics.is_empty());
}

#[test]
fn path_escape_is_rejected() {
    let fixture = Fixture::new();
    let root = fixture.cli_plugin("escape", r#"{"name":"escape"}"#);
    fs::write(
        root.join(".mcp.json"),
        r#"{"mcpServers":{"bad":{"command":"../evil","cwd":"../outside"}}}"#,
    )
    .unwrap();
    let snapshot = fixture.snapshot(true, vec![root], PluginConfig::default());
    let set = materialize_mcp(&snapshot);
    assert!(set.servers.is_empty());
    assert!(
        set.diagnostics
            .iter()
            .any(|d| d.code == "mcp.server_invalid" && d.message.contains("escapes"))
    );
}

#[test]
fn stdio_and_http_shapes_and_inline_metadata() {
    let fixture = Fixture::new();
    let file_plugin = fixture.cli_plugin("shapes", r#"{"name":"shapes","mcpServers":".mcp.json"}"#);
    fs::write(
        file_plugin.join(".mcp.json"),
        r#"{
          "mcpServers":{
            "file-stdio":{
              "command":"node",
              "args":["server.js"],
              "env":{"OK":"1","LATO_SESSION_ID":"forged","LATO_MCP_SERVER":"x"},
              "timeout": 1200000
            }
          }
        }"#,
    )
    .unwrap();
    let inline = fixture.cli_plugin(
        "inline",
        r#"{"name":"inline","mcpServers":{"web":{"url":"http://127.0.0.1:9/mcp"}}}"#,
    );
    let snapshot = fixture.snapshot(true, vec![file_plugin, inline], PluginConfig::default());
    let set = materialize_mcp(&snapshot);
    let names: Vec<_> = set.servers.iter().map(|s| s.server_name.as_str()).collect();
    assert!(names.contains(&"file-stdio"), "{names:?}");
    assert!(names.contains(&"web"), "{names:?}");
    let stdio = set
        .servers
        .iter()
        .find(|s| s.server_name == "file-stdio")
        .unwrap();
    assert_eq!(stdio.transport, McpTransportKind::Stdio);
    assert_eq!(stdio.env.get("OK").map(String::as_str), Some("1"));
    assert!(!stdio.env.contains_key("LATO_SESSION_ID"));
    assert!(!stdio.env.contains_key("LATO_MCP_SERVER"));
    assert_eq!(stdio.timeout_ms, 600_000);
    let http = set.servers.iter().find(|s| s.server_name == "web").unwrap();
    assert_eq!(http.transport, McpTransportKind::StreamableHttp);
    assert!(http.url.is_some());
}

#[test]
fn reserved_env_stripping_on_file_config() {
    let fixture = Fixture::new();
    let root = fixture.cli_plugin("env", r#"{"name":"env"}"#);
    fs::write(
        root.join(".mcp.json"),
        r#"{"mcpServers":{"s":{"command":"node","env":{"LATO_WORKSPACE_ROOT":"/tmp","SAFE":"yes"}}}}"#,
    )
    .unwrap();
    let snapshot = fixture.snapshot(true, vec![root], PluginConfig::default());
    let set = materialize_mcp(&snapshot);
    assert_eq!(set.servers.len(), 1);
    assert_eq!(
        set.servers[0].env.get("SAFE").map(String::as_str),
        Some("yes")
    );
    assert!(!set.servers[0].env.contains_key("LATO_WORKSPACE_ROOT"));
}

#[test]
fn collision_keeps_first_and_diagnoses() {
    let fixture = Fixture::new();
    let a = fixture.cli_plugin(
        "a-plugin",
        r#"{"name":"a-plugin","mcpServers":{"shared":{"command":"node","args":["a"]}}}"#,
    );
    let b = fixture.cli_plugin(
        "b-plugin",
        r#"{"name":"b-plugin","mcpServers":{"shared":{"command":"node","args":["b"]}}}"#,
    );
    let snapshot = fixture.snapshot(true, vec![a, b], PluginConfig::default());
    let set = materialize_mcp(&snapshot);
    assert_eq!(set.servers.len(), 1);
    assert_eq!(set.servers[0].plugin_name, "a-plugin");
    assert!(
        set.diagnostics
            .iter()
            .any(|d| d.code == "mcp.server_collision")
    );
    assert_eq!(qualify_tool("shared", "tool"), "shared__tool");
}

#[test]
fn empty_and_malformed_files_are_isolated() {
    let fixture = Fixture::new();
    let empty = fixture.cli_plugin("empty", r#"{"name":"empty"}"#);
    fs::write(empty.join(".mcp.json"), "{}").unwrap();
    let bad = fixture.cli_plugin("bad", r#"{"name":"bad"}"#);
    fs::write(bad.join(".mcp.json"), "{not-json").unwrap();
    let good = fixture.cli_plugin(
        "good",
        r#"{"name":"good","mcpServers":{"ok":{"command":"node"}}}"#,
    );
    let snapshot = fixture.snapshot(true, vec![empty, bad, good], PluginConfig::default());
    let set = materialize_mcp(&snapshot);
    assert_eq!(set.servers.len(), 1);
    assert_eq!(set.servers[0].server_name, "ok");
    assert!(
        set.diagnostics
            .iter()
            .any(|d| d.code == "mcp.config_invalid" && d.plugin_name == "bad")
    );
}

#[test]
fn relative_command_under_plugin_is_accepted() {
    let fixture = Fixture::new();
    let root = fixture.cli_plugin("rel", r#"{"name":"rel"}"#);
    fs::write(root.join("bin-server"), "#!/bin/sh\n").unwrap();
    fs::write(
        root.join(".mcp.json"),
        r#"{"mcpServers":{"local":{"command":"./bin-server","cwd":"."}}}"#,
    )
    .unwrap();
    let snapshot = fixture.snapshot(true, vec![root.clone()], PluginConfig::default());
    let set = materialize_mcp(&snapshot);
    assert_eq!(set.servers.len(), 1);
    let spec = &set.servers[0];
    let command = spec.command.as_ref().unwrap();
    assert!(
        command.ends_with("bin-server"),
        "command={command:?}"
    );
    let canonical_root = dunce::canonicalize(&root).unwrap();
    assert!(
        command.starts_with(&canonical_root),
        "command={command:?} root={canonical_root:?}"
    );
    assert_eq!(spec.cwd.as_ref(), Some(&canonical_root));
}
