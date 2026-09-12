//! Workflow descriptors, coordinator engine, and Rhai script host types.
//!
//! Parsing/materialization of JSON workflow configs lives in `lato-extensions`.

pub mod completing;
pub mod config;
pub mod engine;
pub mod error;
pub mod inert;
pub mod names;
pub mod script;
pub mod types;

pub use completing::CompletingTaskRunner;
pub use config::{ParseContext, parse_workflow_config};
pub use engine::WorkflowEngine;
pub use error::WorkflowError;
pub use inert::InertWorkflow;
pub use names::{
    MAX_WORKFLOW_NAME_LEN, clamp_agent_budget, normalize_workflow_name, qualify_workflow,
};
pub use script::{
    AgentOpts, AgentResult, BudgetState, HostError, Journal, ScriptOutcome, ValidationError,
    ValidationReport, WorkflowHostRequest, WorkflowRunParams, extract_meta, run::PauseKind,
    run_workflow, validate_script, validate_script_with_agent_budget,
};
pub use types::{
    DEFAULT_AGENT_BUDGET, MAX_AGENT_BUDGET, MAX_DESCRIPTION_BYTES, MAX_WORKFLOW_DIAGNOSTIC_BYTES,
    MAX_WORKFLOW_DIAGNOSTICS, MAX_WORKFLOW_STEPS, MAX_WORKFLOWS_PER_PLUGIN, MIN_AGENT_BUDGET,
    Workflow, WorkflowContext, WorkflowDescriptor, WorkflowDescriptorSet, WorkflowDiagnostic,
    WorkflowOutcome, WorkflowProfile, WorkflowStatus, WorkflowStep, push_diagnostic,
    truncate_description,
};
