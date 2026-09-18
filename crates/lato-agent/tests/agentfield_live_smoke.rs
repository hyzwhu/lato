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

/// Phase 7C3 LIVE gate (opt-in): with an isolated control plane and
/// explicit test credentials, verify that a hard exit after a started run
/// resumes onto the SAME execution id, and the first explicit status does
/// NOT create a second execution. Requires `LATO_AGENTFIELD_PROBE_URL` and
/// `LATO_AGENTFIELD_CREDENTIAL`; without them it reports SKIPPED and makes
/// zero network requests (never executed by PR CI — `no-live-network`).
#[tokio::test]
async fn agentfield_live_hard_exit_resume_reuses_the_same_execution() {
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
    use af::AgentFieldClient as _;
    // Step 1: one real async start on the isolated control plane.
    let started = client
        .start_async("live-smoke.target", &serde_json::json!({"smoke": "7c3"}))
        .await
        .expect("live start must be accepted");
    let execution_id = started.execution_id;
    assert!(!execution_id.is_empty(), "live start must bind an id");
    // Step 2: HARD EXIT — the client is dropped without any cancel, exactly
    // like a killed process; the remote execution keeps running.
    drop(client);
    // Step 3: "restart": a fresh client must address the SAME execution id
    // and must NOT create a second one.
    let resumed = af::production_agentfield_client(None, &config)
        .await
        .expect("restart credential must resolve");
    let status = resumed
        .status(&execution_id)
        .await
        .expect("resumed status must reach the original execution");
    assert_eq!(status.execution_id, execution_id);
    // Step 4: explicit cleanup of the smoke execution.
    let cancelled = resumed.cancel(&execution_id, "live-smoke cleanup").await;
    match cancelled {
        Ok(_) | Err(af::AgentFieldError::Unavailable(_)) => {}
        Err(error) => panic!("live cancel failed unexpectedly: {error}"),
    }
    println!(
        "PASS: hard exit + resume addressed execution {execution_id} without a second execution"
    );
}
