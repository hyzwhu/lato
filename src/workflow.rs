use crate::args::{SandboxArg, WorkflowCommand};
use crate::cli::{configured_stream, lato_home};
use lato_agent::default_fake_stream;
use lato_agent::workflow::{
    WorkflowHostParams, list_workflows, resolve_workflow, spawn_workflow_host_service,
    workflow_max_concurrent_agents,
};
use lato_core::SessionId;
use lato_extensions::{DiscoveryConfig, PluginConfig, build_snapshot, discover_plugins};
use lato_workflow::{
    Journal, ScriptOutcome, WorkflowError, WorkflowRunParams, clamp_agent_budget, run_workflow,
    validate_script_with_agent_budget,
};
use lato_workspace::{FileLocks, SessionTrust};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub async fn run(command: WorkflowCommand) -> i32 {
    match command {
        WorkflowCommand::List { json, plugin_dirs } => list(json, plugin_dirs),
        WorkflowCommand::Run {
            id,
            input,
            plugin_dirs,
            model,
            sandbox,
            validate_only,
            agent_budget,
        } => {
            execute(
                id,
                input,
                plugin_dirs,
                model,
                sandbox,
                validate_only,
                agent_budget,
            )
            .await
        }
    }
}

fn snapshot(
    plugin_dirs: Vec<std::path::PathBuf>,
) -> Result<
    (
        std::path::PathBuf,
        std::sync::Arc<lato_extensions::PluginSnapshot>,
    ),
    String,
> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let discovery = discover_plugins(&DiscoveryConfig {
        cwd: cwd.clone(),
        lato_home: lato_home(),
        cli_plugin_dirs: plugin_dirs,
        project_trusted: SessionTrust::for_headless_prompt(&cwd).cwd_trusted(),
    });
    let snapshot = build_snapshot(1, discovery, &PluginConfig::default())
        .map_err(|error| error.to_string())?;
    Ok((cwd, snapshot))
}

fn list(json: bool, plugin_dirs: Vec<std::path::PathBuf>) -> i32 {
    let (cwd, snapshot) = match snapshot(plugin_dirs) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    let project_trusted = SessionTrust::for_headless_prompt(&cwd).cwd_trusted();
    let workflows = list_workflows(&cwd, &lato_home(), &snapshot, project_trusted);
    if json {
        let payload = serde_json::json!({
            "generation": snapshot.generation(),
            "workflows": workflows.iter().map(|workflow| {
                serde_json::json!({
                    "id": workflow.id,
                    "name": workflow.display_name,
                    "source": workflow.source,
                    "compiled": workflow.compiled,
                    "agentBudget": workflow.agent_budget,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return 0;
    }
    if workflows.is_empty() {
        println!("No materialized workflows. Use --plugin-dir with a trusted plugin.");
        return 0;
    }
    for workflow in workflows {
        println!(
            "{}\t{}\t{}{}",
            workflow.id,
            workflow.display_name,
            workflow.source,
            if workflow.compiled { "\tcompiled" } else { "" }
        );
    }
    0
}

async fn execute(
    id: String,
    input: Option<String>,
    plugin_dirs: Vec<std::path::PathBuf>,
    model: Option<String>,
    sandbox: Option<SandboxArg>,
    validate_only: bool,
    agent_budget: Option<u64>,
) -> i32 {
    let input = match input {
        None => serde_json::json!({}),
        Some(raw) => match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("error: invalid --input JSON: {error}");
                return 2;
            }
        },
    };
    let (cwd, snapshot) = match snapshot(plugin_dirs) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    let project_trusted = SessionTrust::for_headless_prompt(&cwd).cwd_trusted();
    let resolved = match resolve_workflow(&cwd, &lato_home(), &snapshot, project_trusted, &id) {
        Ok(resolved) => resolved,
        Err(error) => return print_workflow_error(error),
    };
    let agent_budget = match agent_budget {
        Some(raw) => match clamp_agent_budget(Some(raw)) {
            Ok(value) => u64::from(value),
            Err(error) => return print_workflow_error(error),
        },
        None => u64::from(resolved.agent_budget),
    };
    if validate_only {
        let script = resolved.script;
        let report = match tokio::task::spawn_blocking(move || {
            validate_script_with_agent_budget(&script, Some(input), agent_budget)
        })
        .await
        {
            Ok(Ok(report)) => report,
            Ok(Err(error)) => {
                eprintln!(
                    "error: {error} ({})",
                    WorkflowError::Failed(error.to_string()).code()
                );
                return 1;
            }
            Err(error) => {
                return print_workflow_error(WorkflowError::Failed(error.to_string()));
            }
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "validated",
                "name": report.name,
                "outcome": report.outcome_summary,
            }))
            .unwrap()
        );
        return 0;
    }

    let mut trust = SessionTrust::for_headless_prompt(&cwd);
    if let Some(sandbox) = sandbox {
        trust.sandbox = crate::permissions::profile(sandbox);
    }
    let model = model.or_else(|| std::env::var("LATO_MODEL").ok());
    let stream = match model {
        Some(selection) => match configured_stream(&selection).await {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("error: {error}");
                return 1;
            }
        },
        None => default_fake_stream(),
    };

    let (host_tx, host_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_cancel.cancel();
    });
    let run_id = "wf-1".to_string();
    let host = spawn_workflow_host_service(
        WorkflowHostParams {
            run_id: run_id.clone(),
            session_id: SessionId::from("cli-workflow"),
            max_concurrent_agents: workflow_max_concurrent_agents(32),
            agent_budget,
            cwd,
            stream,
            locks: Arc::new(FileLocks::new()),
            trust,
            snapshot,
            cancel: cancel.clone(),
            approval: None,
            notify: None,
            // CLI one-shot run: host-owned temp dir; no session directory to persist.
            scratch_dir: None,
        },
        host_rx,
    );
    let outcome = tokio::task::spawn_blocking(move || {
        run_workflow(WorkflowRunParams {
            script: resolved.script,
            args: input,
            journal: Journal::new(None),
            host_tx,
            cancel,
            max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
        })
    })
    .await;
    let _ = host.await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return print_workflow_error(WorkflowError::Failed(error.to_string())),
    };
    match outcome {
        ScriptOutcome::Completed { result } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "runId": run_id,
                    "status": "completed",
                    "output": result,
                }))
                .unwrap()
            );
            0
        }
        ScriptOutcome::Paused { kind, message } => {
            print_workflow_error(WorkflowError::Paused(format!("{}: {message}", kind.as_str())))
        }
        ScriptOutcome::BudgetExceeded { message } => {
            print_workflow_error(WorkflowError::BudgetExceeded(message))
        }
        ScriptOutcome::Cancelled => print_workflow_error(WorkflowError::Cancelled),
        ScriptOutcome::Failed { error } => print_workflow_error(WorkflowError::Failed(error)),
    }
}

fn print_workflow_error(error: WorkflowError) -> i32 {
    eprintln!("error: {error} ({})", error.code());
    match error {
        WorkflowError::InvalidConfiguration(_) => 2,
        _ => 1,
    }
}
