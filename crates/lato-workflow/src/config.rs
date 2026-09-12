// Derived from: Lato MCP config parsing (crates/lato-mcp/src/config.rs)
// License: Apache-2.0 (workspace)
// Lato changes: workflow name/budget descriptors only; no process or HTTP start

//! Workflow descriptor parsing (inline or file JSON).
//!
//! Parsing only — this module never runs workflows.

use std::{collections::BTreeSet, path::Path};

use serde_json::Value;

use crate::{
    MAX_WORKFLOWS_PER_PLUGIN, WorkflowDescriptor, WorkflowDiagnostic, clamp_agent_budget,
    normalize_workflow_name, push_diagnostic, qualify_workflow, truncate_description,
};

pub struct ParseContext<'a> {
    pub plugin_name: &'a str,
    pub plugin_root: &'a Path,
    pub source_path: Option<&'a Path>,
    pub generation: u64,
}

pub fn parse_workflow_config(
    value: &Value,
    context: &ParseContext<'_>,
    seen_ids: &mut BTreeSet<String>,
    workflows: &mut Vec<WorkflowDescriptor>,
    diagnostics: &mut Vec<WorkflowDiagnostic>,
) {
    let Some(map) = workflow_map(value) else {
        push_diagnostic(
            diagnostics,
            "workflow.config_invalid",
            context.plugin_name,
            context.source_path.map(Path::to_path_buf),
            "workflow config must be an object map of name → descriptor",
        );
        return;
    };

    let mut accepted = 0usize;
    for (raw_name, entry) in map {
        if accepted >= MAX_WORKFLOWS_PER_PLUGIN {
            push_diagnostic(
                diagnostics,
                "workflow.limit",
                context.plugin_name,
                context.source_path.map(Path::to_path_buf),
                &format!("plugin exceeds {MAX_WORKFLOWS_PER_PLUGIN} workflows; extras ignored"),
            );
            break;
        }
        let Some(name) = normalize_workflow_name(raw_name) else {
            push_diagnostic(
                diagnostics,
                "workflow.invalid_name",
                context.plugin_name,
                context.source_path.map(Path::to_path_buf),
                &format!("invalid workflow name {raw_name:?}"),
            );
            continue;
        };
        let id = qualify_workflow(context.plugin_name, &name);
        if !seen_ids.insert(id.clone()) {
            push_diagnostic(
                diagnostics,
                "workflow.collision",
                context.plugin_name,
                context.source_path.map(Path::to_path_buf),
                &format!("keeping first workflow {id}; later collision ignored"),
            );
            continue;
        }
        match parse_entry(&name, &id, entry, context) {
            Ok(descriptor) => {
                workflows.push(descriptor);
                accepted += 1;
            }
            Err((code, message)) => {
                seen_ids.remove(&id);
                push_diagnostic(
                    diagnostics,
                    code,
                    context.plugin_name,
                    context.source_path.map(Path::to_path_buf),
                    &message,
                );
            }
        }
    }
}

fn workflow_map(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    let object = value.as_object()?;
    if let Some(inner) = object.get("workflows") {
        return inner.as_object();
    }
    Some(object)
}

fn parse_entry(
    name: &str,
    id: &str,
    entry: &Value,
    context: &ParseContext<'_>,
) -> Result<WorkflowDescriptor, (&'static str, String)> {
    let object = entry.as_object().ok_or_else(|| {
        (
            "workflow.config_invalid",
            format!("workflow {name} must be an object"),
        )
    })?;
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let when_to_use = object
        .get("whenToUse")
        .or_else(|| object.get("when_to_use"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let budget_raw = object
        .get("agentBudget")
        .or_else(|| object.get("agent_budget"))
        .and_then(Value::as_u64);
    let agent_budget = match clamp_agent_budget(budget_raw) {
        Ok(value) => value,
        Err(error) => return Err(("workflow.invalid_budget", error.to_string())),
    };
    Ok(WorkflowDescriptor {
        id: id.to_owned(),
        plugin_name: context.plugin_name.to_owned(),
        name: name.to_owned(),
        description: truncate_description(&description),
        when_to_use: truncate_description(&when_to_use),
        agent_budget,
        source_dir: context.plugin_root.to_path_buf(),
        generation: context.generation,
    })
}
