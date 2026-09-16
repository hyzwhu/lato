// Phase 7C1: version/health/discovery probing with a 30-second snapshot
// cache (spec §6.2).
//
// The probe is a client/config-layer capability only: 7C1 registers no
// model-invokable `agentfield list/start`. A successful discovery snapshot
// stays reusable for 30 seconds (probing must never become a per-turn hard
// dependency); once expired, the next snapshot re-probes, and a failed
// re-probe surfaces `agentfield.unavailable` (7C2 will map that to
// `available: false` and fail `start` closed).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::agentfield::client::{AgentFieldClient, AgentFieldError};

/// Default freshness window for a successful discovery snapshot.
pub const HEALTH_SNAPSHOT_TTL: Duration = Duration::from_secs(30);

/// Bounded snapshot of one successful discovery probe.
#[derive(Clone, Debug)]
pub struct AgentFieldHealthSnapshot {
    pub observed_at: Instant,
    /// Pinned remote version (the pinned contract is enforced during decode).
    pub version: &'static str,
    pub agent_count: usize,
    /// Agents reporting `health_status == "healthy"`.
    pub healthy_agent_count: usize,
    /// Validated derived execute targets, sorted (spec §6.2).
    pub execute_targets: Vec<String>,
    /// Targets whose agent reported `health_status == "healthy"`, sorted.
    pub healthy_execute_targets: Vec<String>,
}

impl AgentFieldHealthSnapshot {
    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.observed_at.elapsed() < ttl
    }
}

/// One-shot prober over any [`AgentFieldClient`] implementation.
#[derive(Clone)]
pub struct AgentFieldProbe {
    client: Arc<dyn AgentFieldClient>,
}

impl AgentFieldProbe {
    pub fn new(client: Arc<dyn AgentFieldClient>) -> Self {
        Self { client }
    }

    /// Probe discovery once; the pinned contract (version, target
    /// derivation) is enforced by the client's strict decoder.
    pub async fn probe(&self) -> Result<AgentFieldHealthSnapshot, AgentFieldError> {
        let envelope = self.client.discovery().await?;
        let healthy_agent_count = envelope
            .capabilities
            .iter()
            .filter(|agent| agent.health_status == "healthy")
            .count();
        let mut execute_targets: Vec<String> = Vec::new();
        let mut healthy_execute_targets: Vec<String> = Vec::new();
        for agent in &envelope.capabilities {
            for reasoner in &agent.reasoners {
                let target = crate::agentfield::config::derive_execute_target(
                    &agent.agent_id,
                    &reasoner.id,
                    &reasoner.invocation_target,
                )
                .map_err(crate::agentfield::client::AgentFieldError::RemoteProtocol)?;
                execute_targets.push(target.clone());
                if agent.health_status == "healthy" {
                    healthy_execute_targets.push(target);
                }
            }
        }
        execute_targets.sort();
        healthy_execute_targets.sort();
        Ok(AgentFieldHealthSnapshot {
            observed_at: Instant::now(),
            version: crate::agentfield::config::PINNED_AGENTFIELD_VERSION,
            agent_count: envelope.capabilities.len(),
            healthy_agent_count,
            execute_targets,
            healthy_execute_targets,
        })
    }
}

/// Probe front-end with a freshness-window snapshot cache.
pub struct AgentFieldProbeCache {
    probe: AgentFieldProbe,
    ttl: Duration,
    cached: Mutex<Option<AgentFieldHealthSnapshot>>,
}

impl AgentFieldProbeCache {
    pub fn new(probe: AgentFieldProbe) -> Self {
        Self::with_ttl(probe, HEALTH_SNAPSHOT_TTL)
    }

    pub fn with_ttl(probe: AgentFieldProbe, ttl: Duration) -> Self {
        Self {
            probe,
            ttl,
            cached: Mutex::new(None),
        }
    }

    /// Return a snapshot: a fresh one is reused with zero network requests;
    /// an expired one triggers a re-probe which refreshes the cache on
    /// success. A failed re-probe surfaces the stable error (no stale
    /// fallback — the caller marks the control plane unavailable).
    pub async fn snapshot(&self) -> Result<AgentFieldHealthSnapshot, AgentFieldError> {
        {
            let cached = self.cached.lock().await;
            if let Some(snapshot) = cached.as_ref()
                && snapshot.is_fresh(self.ttl)
            {
                return Ok(snapshot.clone());
            }
        }
        let fresh = self.probe.probe().await?;
        *self.cached.lock().await = Some(fresh.clone());
        Ok(fresh)
    }

    /// Read the current cached snapshot without probing.
    pub async fn peek(&self) -> Option<AgentFieldHealthSnapshot> {
        self.cached.lock().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentfield::types::{AsyncStartEnvelope, DiscoveryEnvelope};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;

    struct CountingClient {
        calls: AtomicUsize,
        fail_after: Option<usize>,
    }

    impl CountingClient {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail_after: None,
            }
        }

        fn failing_after(calls: usize) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail_after: Some(calls),
            }
        }
    }

    fn discovery_value() -> serde_json::Value {
        json!({
            "discovered_at": "2026-09-16T00:00:00Z",
            "total_agents": 1,
            "total_reasoners": 1,
            "total_skills": 0,
            "pagination": {"limit": 100, "offset": 0, "has_more": false},
            "capabilities": [{
                "agent_id": "legal-agent",
                "group_id": "",
                "base_url": "https://agentfield.invalid",
                "version": "v0.1.138",
                "health_status": "healthy",
                "deployment_type": "service",
                "last_heartbeat": "2026-09-16T00:00:00Z",
                "reasoners": [{
                    "id": "review_contract",
                    "invocation_target": "legal-agent:review_contract"
                }],
                "skills": []
            }]
        })
    }

    #[async_trait]
    impl AgentFieldClient for CountingClient {
        async fn discovery(&self) -> Result<DiscoveryEnvelope, AgentFieldError> {
            let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_after.is_some_and(|limit| calls > limit) {
                return Err(AgentFieldError::Unavailable("control plane down".into()));
            }
            DiscoveryEnvelope::decode(&discovery_value()).map_err(AgentFieldError::RemoteProtocol)
        }

        async fn start_async(
            &self,
            _execute_target: &str,
            _input: &serde_json::Value,
        ) -> Result<AsyncStartEnvelope, AgentFieldError> {
            unreachable!("probe tests never start executions")
        }

        async fn status(
            &self,
            _execution_id: &str,
        ) -> Result<crate::agentfield::types::StatusEnvelope, AgentFieldError> {
            unreachable!("probe tests never query status")
        }

        async fn cancel(
            &self,
            _execution_id: &str,
            _reason: &str,
        ) -> Result<Option<crate::agentfield::types::CancelSuccessEnvelope>, AgentFieldError>
        {
            unreachable!("probe tests never cancel")
        }
    }

    #[tokio::test]
    async fn probe_derives_targets_and_counts_healthy_agents() {
        let probe = AgentFieldProbe::new(Arc::new(CountingClient::new()));
        let snapshot = probe.probe().await.unwrap();
        assert_eq!(snapshot.version, "v0.1.138");
        assert_eq!(snapshot.agent_count, 1);
        assert_eq!(snapshot.healthy_agent_count, 1);
        assert_eq!(
            snapshot.execute_targets,
            vec!["legal-agent.review_contract"]
        );
        assert!(snapshot.is_fresh(Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn fresh_snapshot_reuses_the_cache_without_network_calls() {
        let client = Arc::new(CountingClient::new());
        let cache = AgentFieldProbeCache::new(AgentFieldProbe::new(client.clone()));
        let first = cache.snapshot().await.unwrap();
        let second = cache.snapshot().await.unwrap();
        assert_eq!(first.observed_at, second.observed_at);
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        assert!(cache.peek().await.is_some());
    }

    #[tokio::test]
    async fn expired_snapshot_triggers_a_reprobe() {
        let client = Arc::new(CountingClient::new());
        let cache = AgentFieldProbeCache::with_ttl(
            AgentFieldProbe::new(client.clone()),
            Duration::from_millis(30),
        );
        cache.snapshot().await.unwrap();
        sleep(Duration::from_millis(60)).await;
        cache.snapshot().await.unwrap();
        assert_eq!(client.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn expired_and_unreachable_maps_to_unavailable() {
        let client = Arc::new(CountingClient::failing_after(1));
        let cache = AgentFieldProbeCache::with_ttl(
            AgentFieldProbe::new(client.clone()),
            Duration::from_millis(30),
        );
        cache.snapshot().await.unwrap();
        sleep(Duration::from_millis(60)).await;
        let error = cache.snapshot().await.unwrap_err();
        assert!(matches!(error, AgentFieldError::Unavailable(_)));
        // The stale snapshot is retained but expired; it is never handed out
        // as fresh (7C2 maps this to `available: false`).
        let stale = cache.peek().await.unwrap();
        assert!(!stale.is_fresh(Duration::from_millis(30)));
        assert_eq!(client.calls.load(Ordering::SeqCst), 2);
    }
}
