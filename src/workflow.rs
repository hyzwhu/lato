use crate::args::WorkflowCommand;
use crate::cli::lato_home;
use lato_core::{BudgetAccount, BudgetLimits, SessionId};
use lato_extensions::{
    DiscoveryConfig, PluginConfig, build_snapshot, discover_plugins, materialize_workflows,
};
use lato_runtime::{CoordinatorConfig, NoopTaskEventSink, spawn_task_coordinator};
use lato_workflow::{CompletingTaskRunner, WorkflowEngine};
use lato_workspace::{MemoryWorkspaceAllocator, SessionTrust};
use std::sync::Arc;

pub async fn run(command: WorkflowCommand) -> i32 {
    match command {
        WorkflowCommand::List { json, plugin_dirs } => list(json, plugin_dirs),
        WorkflowCommand::Run {
            id,
            input,
            plugin_dirs,
        } => execute(id, input, plugin_dirs).await,
    }
}

fn snapshot(
    plugin_dirs: Vec<std::path::PathBuf>,
) -> Result<std::sync::Arc<lato_extensions::PluginSnapshot>, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let discovery = discover_plugins(&DiscoveryConfig {
        cwd: cwd.clone(),
        lato_home: lato_home(),
        cli_plugin_dirs: plugin_dirs,
        project_trusted: SessionTrust::for_headless_prompt(&cwd).cwd_trusted(),
    });
    build_snapshot(1, discovery, &PluginConfig::default()).map_err(|error| error.to_string())
}

fn list(json: bool, plugin_dirs: Vec<std::path::PathBuf>) -> i32 {
    let snapshot = match snapshot(plugin_dirs) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    let set = materialize_workflows(&snapshot);
    if json {
        let payload = serde_json::json!({
            "generation": set.generation,
            "workflows": set.workflows.iter().map(|workflow| {
                serde_json::json!({
                    "id": workflow.id,
                    "description": workflow.description,
                    "profile": format!("{:?}", workflow.steps.first().map(|step| step.profile)),
                    "steps": workflow.steps.iter().map(|step| step.prompt.clone()).collect::<Vec<_>>(),
                    "agentBudget": workflow.agent_budget,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return 0;
    }
    if set.workflows.is_empty() {
        println!("No materialized workflows. Use --plugin-dir with a trusted plugin.");
        return 0;
    }
    for workflow in set.workflows.iter() {
        println!(
            "{}\t{}\t{} step(s)",
            workflow.id,
            if workflow.description.is_empty() {
                "-"
            } else {
                workflow.description.as_str()
            },
            workflow.steps.len()
        );
    }
    0
}

async fn execute(id: String, input: Option<String>, plugin_dirs: Vec<std::path::PathBuf>) -> i32 {
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
    let snapshot = match snapshot(plugin_dirs) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    let set = materialize_workflows(&snapshot);
    let workspace = match tempfile::TempDir::new() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    let allocator = match MemoryWorkspaceAllocator::new(workspace.path()) {
        Ok(allocator) => Arc::new(allocator),
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    let (handle, _actor) = spawn_task_coordinator(
        CoordinatorConfig::default(),
        Arc::new(CompletingTaskRunner),
        allocator,
        Arc::new(NoopTaskEventSink),
    );
    let engine = WorkflowEngine::new(set, handle, BudgetAccount::new(BudgetLimits::unlimited()));
    match engine
        .run(&id, SessionId::from("cli-workflow"), input)
        .await
    {
        Ok(outcome) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "runId": outcome.run_id,
                    "status": format!("{:?}", outcome.status),
                    "output": outcome.output,
                }))
                .unwrap()
            );
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}
