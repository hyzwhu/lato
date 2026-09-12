//! Inert workflow descriptors and trait (Phase 7A).
//!
//! Parsing/materialization lives in `lato-extensions`. This crate never
//! interprets scripts, spawns agents, or reserves budget.

pub mod completing;
pub mod config;
pub mod engine;
pub mod error;
pub mod inert;
pub mod names;
pub mod types;

pub use completing::CompletingTaskRunner;
pub use config::{ParseContext, parse_workflow_config};
pub use engine::WorkflowEngine;
pub use error::WorkflowError;
pub use inert::InertWorkflow;
pub use names::{
    MAX_WORKFLOW_NAME_LEN, clamp_agent_budget, normalize_workflow_name, qualify_workflow,
};
pub use types::{
    DEFAULT_AGENT_BUDGET, MAX_AGENT_BUDGET, MAX_DESCRIPTION_BYTES, MAX_WORKFLOW_DIAGNOSTIC_BYTES,
    MAX_WORKFLOW_DIAGNOSTICS, MAX_WORKFLOW_STEPS, MAX_WORKFLOWS_PER_PLUGIN, MIN_AGENT_BUDGET,
    Workflow, WorkflowContext, WorkflowDescriptor, WorkflowDescriptorSet, WorkflowDiagnostic,
    WorkflowOutcome, WorkflowProfile, WorkflowStatus, WorkflowStep, push_diagnostic,
    truncate_description,
};
