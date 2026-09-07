// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-agent/src/plugins/registry.rs
// License: Apache-2.0
// Lato changes: serializes async full rebuilds and publishes generation-stamped last-known-good snapshots

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::sync::{Mutex, RwLock};

use crate::{
    DiscoveryConfig, PluginConfig, PluginSnapshot, RegistryBuildError, build_snapshot,
    discover_plugins,
};

#[derive(Clone)]
pub struct SharedPluginRegistryHandle {
    state: Arc<RwLock<Option<Arc<PluginSnapshot>>>>,
    reload_gate: Arc<Mutex<()>>,
    next_generation: Arc<AtomicU64>,
}

impl Default for SharedPluginRegistryHandle {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SharedPluginRegistryHandle {
    pub fn new(initial: Option<Arc<PluginSnapshot>>) -> Self {
        let next_generation = initial
            .as_ref()
            .map_or(1, |snapshot| snapshot.generation().saturating_add(1));
        Self {
            state: Arc::new(RwLock::new(initial)),
            reload_gate: Arc::new(Mutex::new(())),
            next_generation: Arc::new(AtomicU64::new(next_generation)),
        }
    }

    pub async fn snapshot(&self) -> Option<Arc<PluginSnapshot>> {
        self.state.read().await.clone()
    }

    pub async fn reload(&self, request: ReloadRequest) -> Result<ReloadOutcome, ReloadError> {
        let _guard = self.reload_gate.lock().await;
        validate_discovery_roots(&request.discovery)?;
        let generation = self.next_generation.load(Ordering::Acquire);
        if generation == u64::MAX {
            return Err(ReloadError::GenerationExhausted);
        }
        let snapshot = build_in_worker(generation, request).await?;
        let outcome = ReloadOutcome::from_snapshot(&snapshot);
        *self.state.write().await = Some(snapshot);
        self.next_generation
            .store(generation + 1, Ordering::Release);
        Ok(outcome)
    }

    pub async fn build_for_session(
        &self,
        mut request: ReloadRequest,
        session_cli_dirs: Vec<PathBuf>,
    ) -> Result<Arc<PluginSnapshot>, ReloadError> {
        let _guard = self.reload_gate.lock().await;
        request.discovery.cli_plugin_dirs.extend(session_cli_dirs);
        validate_discovery_roots(&request.discovery)?;
        let generation = self.state.read().await.as_ref().map_or_else(
            || self.next_generation.load(Ordering::Acquire),
            |snapshot| snapshot.generation(),
        );
        build_in_worker(generation, request).await
    }
}

#[derive(Clone, Debug)]
pub struct ReloadRequest {
    pub discovery: DiscoveryConfig,
    pub plugin_config: PluginConfig,
    pub force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReloadOutcome {
    pub generation: u64,
    pub discovered: usize,
    pub active: usize,
    pub diagnostic_count: usize,
}

impl ReloadOutcome {
    fn from_snapshot(snapshot: &PluginSnapshot) -> Self {
        Self {
            generation: snapshot.generation(),
            discovered: snapshot.plugins().len(),
            active: snapshot.active_plugins().count(),
            diagnostic_count: snapshot.diagnostics().len(),
        }
    }
}

async fn build_in_worker(
    generation: u64,
    request: ReloadRequest,
) -> Result<Arc<PluginSnapshot>, ReloadError> {
    tokio::task::spawn_blocking(move || {
        // Phase 6A has no unchanged-input cache: both forced and ordinary calls
        // perform full filesystem discovery. The flag is retained in the API so
        // future local-install refresh can distinguish explicit reloads.
        let _force = request.force;
        let discovery = discover_plugins(&request.discovery);
        build_snapshot(generation, discovery, &request.plugin_config).map_err(ReloadError::Build)
    })
    .await
    .map_err(|error| ReloadError::Worker(error.to_string()))?
}

fn validate_discovery_roots(discovery: &DiscoveryConfig) -> Result<(), ReloadError> {
    if !discovery.cwd.is_dir() {
        return Err(ReloadError::RootUnavailable(discovery.cwd.clone()));
    }
    for root in &discovery.cli_plugin_dirs {
        if !canonical_directory(root) {
            return Err(ReloadError::RootUnavailable(root.clone()));
        }
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> bool {
    dunce::canonicalize(path).is_ok_and(|path| path.is_dir())
}

#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    #[error("plugin reload root is unavailable: {0}")]
    RootUnavailable(PathBuf),
    #[error("plugin snapshot generation is exhausted")]
    GenerationExhausted,
    #[error("plugin registry build failed: {0}")]
    Build(RegistryBuildError),
    #[error("plugin reload worker failed: {0}")]
    Worker(String),
}

impl ReloadError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RootUnavailable(_) => "plugin.reload_root_unavailable",
            Self::GenerationExhausted => "plugin.reload_generation_exhausted",
            Self::Build(_) => "plugin.reload_build_failed",
            Self::Worker(_) => "plugin.reload_worker_failed",
        }
    }
}
