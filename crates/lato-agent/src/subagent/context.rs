// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs
// License: Apache-2.0
// Lato changes: explicit bounded context package instead of full transcript inheritance

use lato_core::{BudgetAmount, TaskError, TaskErrorCode};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ContextReference {
    pub id: String,
    pub summary: String,
    pub location: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextPackageLimits {
    pub max_bytes: usize,
    pub max_constraints: usize,
    pub max_references: usize,
    pub max_summary_bytes: usize,
}

impl Default for ContextPackageLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024,
            max_constraints: 32,
            max_references: 64,
            max_summary_bytes: 16 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ContextPackage {
    pub task: String,
    pub profile_instructions: String,
    pub constraints: Vec<String>,
    pub parent_summary: Option<String>,
    pub references: Vec<ContextReference>,
    pub workspace_root: PathBuf,
    pub remaining_budget: BudgetAmount,
}

impl ContextPackage {
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }

    pub fn render(&self) -> String {
        let mut rendered = format!(
            "Delegated task:\n{}\n\nProfile instructions:\n{}\n\nWorkspace:\n{}\n",
            self.task,
            self.profile_instructions,
            self.workspace_root.display()
        );
        if !self.constraints.is_empty() {
            rendered.push_str("\nConstraints:\n");
            for constraint in &self.constraints {
                rendered.push_str("- ");
                rendered.push_str(constraint);
                rendered.push('\n');
            }
        }
        if let Some(summary) = &self.parent_summary {
            rendered.push_str("\nRelevant parent state:\n");
            rendered.push_str(summary);
            rendered.push('\n');
        }
        if !self.references.is_empty() {
            rendered.push_str("\nSelected references:\n");
            for reference in &self.references {
                rendered.push_str("- [");
                rendered.push_str(&reference.id);
                rendered.push_str("] ");
                rendered.push_str(&reference.summary);
                if let Some(location) = &reference.location {
                    rendered.push_str(" (location: ");
                    rendered.push_str(location);
                    rendered.push(')');
                }
                rendered.push('\n');
            }
        }
        rendered.push_str("\nRemaining budget (authoritative):\n");
        rendered.push_str(&serde_json::to_string(&self.remaining_budget).unwrap_or_default());
        rendered
    }
}

pub struct ContextPackageBuilder {
    limits: ContextPackageLimits,
    task: Option<String>,
    profile_instructions: Option<String>,
    constraints: Vec<String>,
    parent_summary: Option<String>,
    references: Vec<ContextReference>,
    workspace_root: Option<PathBuf>,
    remaining_budget: BudgetAmount,
}

impl ContextPackageBuilder {
    pub fn new(limits: ContextPackageLimits) -> Self {
        Self {
            limits,
            task: None,
            profile_instructions: None,
            constraints: Vec::new(),
            parent_summary: None,
            references: Vec::new(),
            workspace_root: None,
            remaining_budget: BudgetAmount::ZERO,
        }
    }

    pub fn task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    pub fn profile_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.profile_instructions = Some(instructions.into());
        self
    }

    pub fn constraints(mut self, constraints: Vec<String>) -> Self {
        self.constraints = constraints;
        self
    }

    pub fn parent_summary(mut self, summary: impl Into<String>) -> Self {
        self.parent_summary = Some(summary.into());
        self
    }

    pub fn references(mut self, references: Vec<ContextReference>) -> Self {
        self.references = references;
        self
    }

    pub fn workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }

    pub fn remaining_budget(mut self, budget: BudgetAmount) -> Self {
        self.remaining_budget = budget;
        self
    }

    pub fn build(mut self) -> Result<ContextPackage, TaskError> {
        let task = required_text(self.task, "task")?;
        let profile_instructions =
            required_text(self.profile_instructions, "profile instructions")?;
        let workspace_root = self.workspace_root.ok_or_else(|| {
            TaskError::new(
                TaskErrorCode::RunnerInitialization,
                "context package requires a workspace root",
            )
        })?;
        self.constraints.truncate(self.limits.max_constraints);
        self.references.truncate(self.limits.max_references);
        if let Some(summary) = &mut self.parent_summary {
            truncate_utf8(summary, self.limits.max_summary_bytes);
        }
        let mut package = ContextPackage {
            task,
            profile_instructions,
            constraints: self.constraints,
            parent_summary: self.parent_summary,
            references: self.references,
            workspace_root,
            remaining_budget: self.remaining_budget,
        };
        while package.encoded_len() > self.limits.max_bytes {
            if package.references.pop().is_some() {
                continue;
            }
            if package.constraints.pop().is_some() {
                continue;
            }
            if let Some(summary) = &mut package.parent_summary
                && !summary.is_empty()
            {
                let next = summary.len().saturating_sub(1024);
                truncate_utf8(summary, next);
                continue;
            }
            return Err(TaskError::new(
                TaskErrorCode::RunnerInitialization,
                "required child context exceeds the configured byte limit",
            ));
        }
        Ok(package)
    }
}

fn required_text(value: Option<String>, field: &str) -> Result<String, TaskError> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(TaskError::new(
            TaskErrorCode::RunnerInitialization,
            format!("context package requires non-empty {field}"),
        )),
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}
