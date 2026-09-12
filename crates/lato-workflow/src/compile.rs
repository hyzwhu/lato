//! Compile 7B2 JSON workflow descriptors into sequential Rhai agent scripts.

use crate::{WorkflowDescriptor, WorkflowProfile, WorkflowStep};

/// Compile a declarative JSON workflow descriptor into a sequential Rhai script.
///
/// `meta.name` replaces `_` with `-`. Prompt/description string literals escape
/// `\`, `"`, and newlines as `\n`.
pub fn compile_declarative_workflow(descriptor: &WorkflowDescriptor) -> String {
    let name = descriptor.name.replace('_', "-");
    let description = escape_rhai_string(&descriptor.description);

    let mut script = String::new();
    script.push_str("let meta = #{\n");
    script.push_str("    name: \"");
    script.push_str(&name);
    script.push_str("\",\n");
    script.push_str("    description: \"");
    script.push_str(&description);
    script.push_str("\",\n");
    script.push_str("};\n");
    script.push_str("let last = ();\n");

    for step in &descriptor.steps {
        append_step(&mut script, step);
    }

    script.push_str("complete(last.output);\n");
    script
}

fn append_step(script: &mut String, step: &WorkflowStep) {
    let prompt = escape_rhai_string(&step.prompt);
    let (agent_type, capability_mode) = profile_opts(step.profile);
    script.push_str("last = agent(\"");
    script.push_str(&prompt);
    script.push_str("\\n\\ninput: \" + json_encode(args), #{\n");
    script.push_str("    agent_type: \"");
    script.push_str(agent_type);
    script.push_str("\",\n");
    script.push_str("    capability_mode: \"");
    script.push_str(capability_mode);
    script.push_str("\",\n");
    script.push_str("});\n");
}

fn profile_opts(profile: WorkflowProfile) -> (&'static str, &'static str) {
    match profile {
        WorkflowProfile::Explorer => ("explorer", "read-only"),
        WorkflowProfile::Reviewer => ("reviewer", "read-only"),
        WorkflowProfile::Worker => ("worker", "read-write"),
    }
}

fn escape_rhai_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{extract_meta, validate_script};

    #[test]
    fn compiles_explorer_step_to_agent_call() {
        let descriptor = WorkflowDescriptor {
            id: "demo/review-changes".into(),
            plugin_name: "demo".into(),
            name: "review_changes".into(),
            description: "Review a diff".into(),
            when_to_use: String::new(),
            agent_budget: 128,
            steps: vec![WorkflowStep {
                prompt: "Inspect the patch".into(),
                profile: WorkflowProfile::Explorer,
            }],
            source_dir: PathBuf::from("."),
            generation: 1,
        };
        let script = compile_declarative_workflow(&descriptor);
        assert!(script.contains("name: \"review-changes\""));
        assert!(script.contains("agent_type: \"explorer\""));
        assert!(script.contains("capability_mode: \"read-only\""));
        extract_meta(&script).unwrap();
        let report = validate_script(&script, None).unwrap();
        assert!(report.outcome_ok);
    }

    #[test]
    fn escapes_rhai_literals_and_maps_worker_profile() {
        let descriptor = WorkflowDescriptor {
            id: "demo/quote".into(),
            plugin_name: "demo".into(),
            name: "quote_me".into(),
            description: "Say \"hi\"\\now".into(),
            when_to_use: String::new(),
            agent_budget: 128,
            steps: vec![
                WorkflowStep {
                    prompt: "line1\nline2 \"x\"".into(),
                    profile: WorkflowProfile::Worker,
                },
                WorkflowStep {
                    prompt: "review".into(),
                    profile: WorkflowProfile::Reviewer,
                },
            ],
            source_dir: PathBuf::from("."),
            generation: 1,
        };
        let script = compile_declarative_workflow(&descriptor);
        assert!(script.contains("name: \"quote-me\""));
        assert!(script.contains("description: \"Say \\\"hi\\\"\\\\now\""));
        assert!(script.contains("agent(\"line1\\nline2 \\\"x\\\"\\n\\ninput: \""));
        assert!(script.contains("agent_type: \"worker\""));
        assert!(script.contains("capability_mode: \"read-write\""));
        assert!(script.contains("agent_type: \"reviewer\""));
        assert!(script.contains("capability_mode: \"read-only\""));
        extract_meta(&script).unwrap();
        assert!(validate_script(&script, None).unwrap().outcome_ok);
    }
}
