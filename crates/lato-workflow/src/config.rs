// Derived from: Lato MCP config parsing (crates/lato-mcp/src/config.rs)
// License: Apache-2.0 (workspace)
// Lato changes: workflow name/budget descriptors only; no process or HTTP start

//! Workflow descriptor parsing (inline or file JSON).
//!
//! Parsing only — this module never runs workflows.

use std::{collections::BTreeSet, path::Path};

use serde_json::Value;

use crate::{
    MAX_WORKFLOW_STEPS, MAX_WORKFLOWS_PER_PLUGIN, WorkflowDescriptor, WorkflowDiagnostic,
    WorkflowProfile, WorkflowStep, clamp_agent_budget, normalize_workflow_name, push_diagnostic,
    qualify_workflow, truncate_description,
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
    let default_profile = match object
        .get("profile")
        .and_then(Value::as_str)
        .map(WorkflowProfile::parse)
    {
        None => WorkflowProfile::Worker,
        Some(Some(profile)) => profile,
        Some(None) => {
            return Err((
                "workflow.invalid_profile",
                format!("workflow {name} has invalid profile"),
            ));
        }
    };
    let prompt = object
        .get("prompt")
        .or_else(|| object.get("objective"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty());
    let steps = parse_steps(
        object.get("steps"),
        prompt.as_deref(),
        &description,
        id,
        default_profile,
    )?;
    Ok(WorkflowDescriptor {
        id: id.to_owned(),
        plugin_name: context.plugin_name.to_owned(),
        name: name.to_owned(),
        description: truncate_description(&description),
        when_to_use: truncate_description(&when_to_use),
        agent_budget,
        steps,
        source_dir: context.plugin_root.to_path_buf(),
        generation: context.generation,
    })
}

fn parse_steps(
    raw: Option<&Value>,
    prompt: Option<&str>,
    description: &str,
    id: &str,
    default_profile: WorkflowProfile,
) -> Result<Vec<WorkflowStep>, (&'static str, String)> {
    if let Some(Value::Array(items)) = raw {
        if items.is_empty() {
            return Ok(vec![default_step(prompt, description, id, default_profile)]);
        }
        let mut steps = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if steps.len() >= MAX_WORKFLOW_STEPS {
                break;
            }
            let object = item.as_object().ok_or_else(|| {
                (
                    "workflow.invalid_step",
                    format!("workflow {id} step {index} must be an object"),
                )
            })?;
            let step_prompt = object
                .get("prompt")
                .or_else(|| object.get("objective"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    (
                        "workflow.invalid_step",
                        format!("workflow {id} step {index} requires prompt"),
                    )
                })?;
            let profile = match object.get("profile").and_then(Value::as_str) {
                None => default_profile,
                Some(raw) => WorkflowProfile::parse(raw).ok_or_else(|| {
                    (
                        "workflow.invalid_profile",
                        format!("workflow {id} step {index} has invalid profile {raw:?}"),
                    )
                })?,
            };
            steps.push(WorkflowStep {
                prompt: truncate_description(&step_prompt),
                profile,
            });
        }
        return Ok(steps);
    }
    if raw.is_some() {
        return Err((
            "workflow.invalid_step",
            format!("workflow {id} steps must be an array"),
        ));
    }
    Ok(vec![default_step(prompt, description, id, default_profile)])
}

fn default_step(
    prompt: Option<&str>,
    description: &str,
    id: &str,
    profile: WorkflowProfile,
) -> WorkflowStep {
    let prompt = prompt
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let trimmed = description.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
        .unwrap_or_else(|| format!("Run {id}"));
    WorkflowStep {
        prompt: truncate_description(&prompt),
        profile,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use serde_json::json;

    use super::*;
    use crate::WorkflowProfile;

    fn parse(value: serde_json::Value) -> (Vec<WorkflowDescriptor>, Vec<WorkflowDiagnostic>) {
        let mut seen = BTreeSet::new();
        let mut workflows = Vec::new();
        let mut diagnostics = Vec::new();
        parse_workflow_config(
            &value,
            &ParseContext {
                plugin_name: "demo",
                plugin_root: Path::new("."),
                source_path: None,
                generation: 1,
            },
            &mut seen,
            &mut workflows,
            &mut diagnostics,
        );
        (workflows, diagnostics)
    }

    #[test]
    fn default_step_uses_prompt_then_description() {
        let (workflows, diagnostics) = parse(json!({
            "review": { "prompt": "Inspect the diff", "profile": "explorer" }
        }));
        assert!(diagnostics.is_empty());
        assert_eq!(workflows[0].steps.len(), 1);
        assert_eq!(workflows[0].steps[0].prompt, "Inspect the diff");
        assert_eq!(workflows[0].steps[0].profile, WorkflowProfile::Explorer);
    }

    #[test]
    fn steps_array_is_parsed() {
        let (workflows, diagnostics) = parse(json!({
            "review": {
                "steps": [
                    { "prompt": "Scan", "profile": "explorer" },
                    { "prompt": "Patch", "profile": "worker" }
                ]
            }
        }));
        assert!(diagnostics.is_empty());
        assert_eq!(workflows[0].steps.len(), 2);
        assert_eq!(workflows[0].steps[1].prompt, "Patch");
        assert_eq!(workflows[0].steps[1].profile, WorkflowProfile::Worker);
    }

    #[test]
    fn invalid_profile_is_isolated() {
        let (workflows, diagnostics) = parse(json!({
            "review": { "profile": "god-mode" }
        }));
        assert!(workflows.is_empty());
        assert!(
            diagnostics
                .iter()
                .any(|item| item.code == "workflow.invalid_profile")
        );
    }
}
