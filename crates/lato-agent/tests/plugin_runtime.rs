use std::sync::Arc;

use async_trait::async_trait;
use lato_agent::{RuntimePromptOutcome, RuntimeSession, SessionPluginSnapshots};
use lato_ai::{ModelStream, StreamPiece};
use lato_core::{ModelError, SessionId};
use lato_extensions::{
    DiscoveryConfig, PluginConfig, PluginSnapshot, build_snapshot, discover_plugins,
};
use lato_workspace::{FileLocks, SessionTrust};
use tokio::sync::{Semaphore, mpsc};

#[tokio::test]
async fn active_turn_keeps_old_snapshot_and_next_turn_adopts_pending() {
    let old = snapshot(1, "old");
    let new = snapshot(2, "new");
    let stream = Arc::new(BlockingStream::default());
    let directory = tempfile::tempdir().unwrap();
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let session = Arc::new(RuntimeSession::new(
        "plugin-turn-freeze".into(),
        stream.clone(),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(directory.path()),
        directory.path().to_path_buf(),
        updates,
        None,
    ));
    session.stage_plugin_snapshot(old).await.unwrap();

    let prompt = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.prompt("wait".into()).await }
    });
    stream.started.acquire().await.unwrap().forget();
    session.stage_plugin_snapshot(new).await.unwrap();
    assert_eq!(
        session
            .active_turn_plugin_snapshot()
            .await
            .unwrap()
            .generation(),
        1
    );
    assert_eq!(session.plugin_snapshot().await.generation(), 1);
    stream.release.add_permits(1);
    assert_eq!(
        prompt.await.unwrap().unwrap(),
        RuntimePromptOutcome::Complete {
            text: "done".into()
        }
    );
    assert_eq!(session.plugin_snapshot().await.generation(), 2);
    assert!(session.active_turn_plugin_snapshot().await.is_none());
}

#[tokio::test]
async fn snapshot_table_updates_every_live_session_without_cross_leakage() {
    let table = SessionPluginSnapshots::default();
    table.register(SessionId::from("a"), snapshot(1, "a")).await;
    table.register(SessionId::from("b"), snapshot(1, "b")).await;
    table.adopt(SessionId::from("a"), snapshot(2, "a2")).await;
    assert_eq!(
        table.get(&SessionId::from("a")).await.unwrap().generation(),
        2
    );
    assert_eq!(
        table.get(&SessionId::from("b")).await.unwrap().generation(),
        1
    );
    assert_eq!(
        table
            .session_ids()
            .await
            .iter()
            .map(SessionId::as_str)
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
}

#[tokio::test]
async fn runtime_session_rejects_snapshot_generation_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let (updates, _updates_rx) = mpsc::unbounded_channel();
    let session = RuntimeSession::new(
        "plugin-rollback".into(),
        Arc::new(BlockingStream::default()),
        Arc::new(FileLocks::new()),
        SessionTrust::for_headless_prompt(directory.path()),
        directory.path().to_path_buf(),
        updates,
        None,
    );
    session
        .stage_plugin_snapshot(snapshot(2, "new"))
        .await
        .unwrap();
    let error = session
        .stage_plugin_snapshot(snapshot(1, "old"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "plugin.snapshot_generation_rollback");
}

struct BlockingStream {
    started: Semaphore,
    release: Semaphore,
}

impl Default for BlockingStream {
    fn default() -> Self {
        Self {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}

#[async_trait]
impl ModelStream for BlockingStream {
    async fn stream(
        &self,
        _prompt_bytes: usize,
        _context: serde_json::Value,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), ModelError> {
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        tx.send(StreamPiece::Text("done".into()))
            .await
            .map_err(|_| ModelError::cancelled())
    }
}

fn snapshot(generation: u64, name: &str) -> Arc<PluginSnapshot> {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let plugin = root.path().join(name);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("plugin.json"),
        format!(r#"{{"name":"{name}"}}"#),
    )
    .unwrap();
    build_snapshot(
        generation,
        discover_plugins(&DiscoveryConfig {
            cwd: workspace,
            lato_home: home,
            cli_plugin_dirs: vec![plugin],
            project_trusted: true,
        }),
        &PluginConfig::default(),
    )
    .unwrap()
}
