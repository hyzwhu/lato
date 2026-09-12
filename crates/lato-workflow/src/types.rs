//! Workflow descriptors, context, and the inert-capable trait.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use lato_core::SessionId;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::WorkflowError;

pub const DEFAULT_AGENT_BUDGET: u32 = 128;
pub const MIN_AGENT_BUDGET: u32 = 1;
pub const MAX_AGENT_BUDGET: u32 = 1024;
pub const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
pub const MAX_WORKFLOWS_PER_PLUGIN: usize = 32;
pub const MAX_WORKFLOW_DIAGNOSTICS: usize = 128;
pub const MAX_WORKFLOW_DIAGNOSTIC_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowDescriptor {
    pub id: String,
    pub plugin_name: String,
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    pub agent_budget: u32,
    pub source_dir: PathBuf,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowDiagnostic {
    pub code: String,
    pub plugin_name: String,
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct WorkflowDescriptorSet {
    pub generation: u64,
    pub workflows: Arc<[WorkflowDescriptor]>,
    pub diagnostics: Arc<[WorkflowDiagnostic]>,
}

impl WorkflowDescriptorSet {
    pub fn empty(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            generation,
            workflows: Arc::from([]),
            diagnostics: Arc::from([]),
        })
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowContext {
    pub run_id: String,
    pub session_id: SessionId,
    pub generation: u64,
    pub cancel: CancellationToken,
    pub agent_budget: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowStatus {
    NotImplemented,
}

#[derive(Clone, Debug)]
pub struct WorkflowOutcome {
    pub run_id: String,
    pub status: WorkflowStatus,
    pub output: Value,
}

/// Future engines create coordinator tasks with `TaskOwner::Workflow` and
/// cancel them only via `TaskCoordinator::cancel_workflow`. 7A does not start runs.
#[async_trait]
pub trait Workflow: Send + Sync {
    fn descriptor(&self) -> &WorkflowDescriptor;
    async fn run(
        &self,
        context: WorkflowContext,
        input: Value,
    ) -> Result<WorkflowOutcome, WorkflowError>;
}

pub fn truncate_description(raw: &str) -> String {
    if raw.len() <= MAX_DESCRIPTION_BYTES {
        return raw.to_owned();
    }
    let mut end = MAX_DESCRIPTION_BYTES;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_owned()
}

pub fn push_diagnostic(
    diagnostics: &mut Vec<WorkflowDiagnostic>,
    code: &str,
    plugin_name: &str,
    path: Option<PathBuf>,
    message: &str,
) {
    if diagnostics.len() >= MAX_WORKFLOW_DIAGNOSTICS {
        return;
    }
    let mut message = message.to_owned();
    if message.len() > MAX_WORKFLOW_DIAGNOSTIC_BYTES {
        let mut end = MAX_WORKFLOW_DIAGNOSTIC_BYTES;
        while end > 0 && !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    diagnostics.push(WorkflowDiagnostic {
        code: code.to_owned(),
        plugin_name: plugin_name.to_owned(),
        path,
        message,
    });
}
