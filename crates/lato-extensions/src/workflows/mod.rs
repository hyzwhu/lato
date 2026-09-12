// Derived from: Lato MCP materialize_mcp (crates/lato-extensions/src/mcp/mod.rs)
// License: Apache-2.0 (workspace)
// Lato changes: workflow descriptors from PluginSnapshot; never executes or reserves budget

//! Materialize trusted+enabled workflow descriptors from a frozen [`PluginSnapshot`].

use std::{collections::BTreeSet, fs, sync::Arc};

use lato_workflow::{
    ParseContext, WorkflowDescriptor, WorkflowDescriptorSet, WorkflowDiagnostic,
    parse_workflow_config, push_diagnostic,
};
use serde_json::Value;

use crate::PluginSnapshot;

/// Build an immutable workflow descriptor set for the snapshot generation.
///
/// Consumes only [`PluginSnapshot::active_plugins`] (trusted ∧ enabled).
/// Does not execute workflows or reserve budget.
pub fn materialize_workflows(snapshot: &PluginSnapshot) -> Arc<WorkflowDescriptorSet> {
    let mut workflows: Vec<WorkflowDescriptor> = Vec::new();
    let mut diagnostics: Vec<WorkflowDiagnostic> = Vec::new();
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    let ceiling = snapshot.workflow_ceiling();

    let mut plugins = snapshot.active_plugins().collect::<Vec<_>>();
    plugins.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.canonical_root.cmp(&b.canonical_root))
    });

    for plugin in plugins {
        let mut sources = Vec::new();
        if let Some(path) = &plugin.workflow_config_path {
            match fs::read_to_string(path)
                .map_err(|error| error.to_string())
                .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()))
            {
                Ok(value) => sources.push((Some(path.clone()), value)),
                Err(message) => push_diagnostic(
                    &mut diagnostics,
                    "workflow.config_invalid",
                    &plugin.name,
                    Some(path.clone()),
                    &message,
                ),
            }
        }
        if let Some(value) = &plugin.inline_workflows {
            let wrapped = match value {
                Value::Object(map)
                    if map.contains_key("workflows") || map.values().all(Value::is_object) =>
                {
                    value.clone()
                }
                other => Value::Object(
                    [("workflows".to_owned(), other.clone())]
                        .into_iter()
                        .collect(),
                ),
            };
            sources.push((None, wrapped));
        }
        sources.sort_by(|a, b| a.0.cmp(&b.0));

        for (path, value) in sources {
            let context = ParseContext {
                plugin_name: &plugin.name,
                plugin_root: &plugin.canonical_root,
                source_path: path.as_deref(),
                generation: snapshot.generation(),
            };
            parse_workflow_config(
                &value,
                &context,
                &mut seen_ids,
                &mut workflows,
                &mut diagnostics,
            );
        }
    }

    workflows.retain(|workflow| ceiling.allows(&workflow.id));

    Arc::new(WorkflowDescriptorSet {
        generation: snapshot.generation(),
        workflows: workflows.into(),
        diagnostics: diagnostics.into(),
    })
}
