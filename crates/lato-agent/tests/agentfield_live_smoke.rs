//! Opt-in AgentField production smoke (AC-09).
//!
//! Runs only when an operator provides both `LATO_AGENTFIELD_PROBE_URL` (the
//! configured control-plane origin) and `LATO_AGENTFIELD_CREDENTIAL`. Without
//! the environment the probe reports SKIPPED and performs zero network
//! requests and zero credential resolutions. PR CI never executes this
//! against a real control plane: the `no-live-network` job guarantees that,
//! and this file contains no test attribute on the same line as any opt-in
//! environment name.

use lato_agent::agentfield::{self as af, config::AgentFieldConfig};

#[tokio::test]
async fn agentfield_opt_in_discovery_probe() {
    let Ok(base_url) = std::env::var("LATO_AGENTFIELD_PROBE_URL") else {
        println!("SKIPPED: LATO_AGENTFIELD_PROBE_URL not provided");
        return;
    };
    let Ok(secret) = std::env::var("LATO_AGENTFIELD_CREDENTIAL") else {
        println!("SKIPPED: LATO_AGENTFIELD_CREDENTIAL not provided");
        return;
    };
    if base_url.is_empty() || secret.is_empty() {
        println!("SKIPPED: empty opt-in environment");
        return;
    }
    let config = AgentFieldConfig::parse(&serde_json::json!({
        "enabled": true,
        "baseUrl": base_url,
        "credential": "agentfield:smoke",
    }))
    .expect("valid smoke config")
    .expect("agentfield stanza");
    let client = af::production_agentfield_client(None, &config)
        .await
        .expect("credential must resolve from the environment");
    let probe = af::AgentFieldProbe::new(std::sync::Arc::new(client));
    let snapshot = probe.probe().await.expect("live discovery must succeed");
    println!(
        "PASS: discovery answered {} agent(s), {} healthy, pinned contract {}",
        snapshot.agent_count, snapshot.healthy_agent_count, snapshot.version
    );
}
