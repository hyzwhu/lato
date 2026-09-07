use std::{fs, path::PathBuf, sync::Arc};

use lato_extensions::{DiscoveryConfig, PluginConfig, ReloadRequest, SharedPluginRegistryHandle};

#[tokio::test]
async fn failed_global_reload_preserves_last_known_good_generation() {
    let fixture = ReloadFixture::new();
    let handle = fixture.handle().await;
    let before = handle.snapshot().await.unwrap();
    fs::remove_dir_all(&fixture.cli_plugin).unwrap();
    let error = handle.reload(fixture.request(true)).await.unwrap_err();
    assert_eq!(error.code(), "plugin.reload_root_unavailable");
    assert_eq!(
        handle.snapshot().await.unwrap().generation(),
        before.generation()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_reloads_publish_monotonic_generations() {
    let fixture = ReloadFixture::new();
    let handle = fixture.handle().await;
    let (a, b) = tokio::join!(
        handle.reload(fixture.request(true)),
        handle.reload(fixture.request(true)),
    );
    let mut generations = [a.unwrap().generation, b.unwrap().generation];
    generations.sort_unstable();
    assert_eq!(generations[1], generations[0] + 1);
    assert_eq!(
        handle.snapshot().await.unwrap().generation(),
        generations[1]
    );
}

#[tokio::test]
async fn forced_identical_reload_still_rediscovers_and_publishes() {
    let fixture = ReloadFixture::new();
    let handle = fixture.handle().await;
    let first = handle.snapshot().await.unwrap();
    fs::write(
        fixture.cli_plugin.join("plugin.json"),
        r#"{"name":"changed"}"#,
    )
    .unwrap();
    let outcome = handle.reload(fixture.request(true)).await.unwrap();
    let changed = handle.snapshot().await.unwrap();
    assert_eq!(outcome.generation, first.generation() + 1);
    assert_eq!(changed.active_names(), vec!["changed"]);

    let identical = handle.reload(fixture.request(true)).await.unwrap();
    assert_eq!(identical.generation, outcome.generation + 1);
}

#[tokio::test]
async fn malformed_candidate_is_isolated_from_valid_plugins() {
    let fixture = ReloadFixture::new();
    let invalid = fixture.home.join("plugins/invalid");
    fs::create_dir_all(&invalid).unwrap();
    fs::write(invalid.join("plugin.json"), "{").unwrap();
    let handle = fixture.handle().await;
    let snapshot = handle.snapshot().await.unwrap();
    assert_eq!(snapshot.active_names(), vec!["cli"]);
    assert!(
        snapshot
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "plugin.manifest_invalid")
    );
}

#[tokio::test]
async fn session_cli_roots_do_not_leak_into_shared_or_other_sessions() {
    let fixture = ReloadFixture::new();
    let handle = fixture.handle().await;
    let session_a = fixture.root.path().join("session-a");
    let session_b = fixture.root.path().join("session-b");
    plugin(&session_a, "session-a");
    plugin(&session_b, "session-b");

    let a = handle
        .build_for_session(fixture.request(false), vec![session_a])
        .await
        .unwrap();
    let b = handle
        .build_for_session(fixture.request(false), vec![session_b])
        .await
        .unwrap();
    assert_eq!(a.active_names(), vec!["cli", "session-a"]);
    assert_eq!(b.active_names(), vec!["cli", "session-b"]);
    assert_eq!(handle.snapshot().await.unwrap().active_names(), vec!["cli"]);
    assert!(Arc::ptr_eq(
        &handle.snapshot().await.unwrap(),
        &handle.snapshot().await.unwrap()
    ));
}

struct ReloadFixture {
    root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    cli_plugin: PathBuf,
}

impl ReloadFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace");
        let home = root.path().join("home");
        let cli_plugin = root.path().join("cli-plugin");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(home.join("plugins")).unwrap();
        plugin(&cli_plugin, "cli");
        Self {
            root,
            cwd,
            home,
            cli_plugin,
        }
    }

    fn request(&self, force: bool) -> ReloadRequest {
        ReloadRequest {
            discovery: DiscoveryConfig {
                cwd: self.cwd.clone(),
                lato_home: self.home.clone(),
                cli_plugin_dirs: vec![self.cli_plugin.clone()],
                project_trusted: true,
            },
            plugin_config: PluginConfig::default(),
            force,
        }
    }

    async fn handle(&self) -> SharedPluginRegistryHandle {
        let handle = SharedPluginRegistryHandle::default();
        handle.reload(self.request(false)).await.unwrap();
        handle
    }
}

fn plugin(root: &std::path::Path, name: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("plugin.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
}
