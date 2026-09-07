// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/acp_session_impl/hooks_plugins.rs
// License: Apache-2.0
// Lato changes: stores immutable session snapshots without activating skills, hooks, or MCP consumers

use std::{collections::HashMap, sync::Arc};

use lato_core::SessionId;
use lato_extensions::PluginSnapshot;
use tokio::sync::RwLock;

#[derive(Clone, Default)]
pub struct SessionPluginSnapshots {
    snapshots: Arc<RwLock<HashMap<SessionId, Arc<PluginSnapshot>>>>,
}

impl SessionPluginSnapshots {
    pub async fn register(&self, session_id: SessionId, snapshot: Arc<PluginSnapshot>) {
        self.snapshots.write().await.insert(session_id, snapshot);
    }

    pub async fn adopt(&self, session_id: SessionId, snapshot: Arc<PluginSnapshot>) {
        let mut snapshots = self.snapshots.write().await;
        let should_adopt = snapshots
            .get(&session_id)
            .is_none_or(|current| snapshot.generation() >= current.generation());
        if should_adopt {
            snapshots.insert(session_id, snapshot);
        }
    }

    pub async fn get(&self, session_id: &SessionId) -> Option<Arc<PluginSnapshot>> {
        self.snapshots.read().await.get(session_id).cloned()
    }

    pub async fn remove(&self, session_id: &SessionId) -> Option<Arc<PluginSnapshot>> {
        self.snapshots.write().await.remove(session_id)
    }

    pub async fn session_ids(&self) -> Vec<SessionId> {
        let mut ids = self
            .snapshots
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        ids
    }
}
