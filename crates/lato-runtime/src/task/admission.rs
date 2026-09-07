// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/admission.rs
// License: Apache-2.0
// Lato changes: bounded provider-neutral admission decisions for real task-tree children

use crate::task::{CoordinatorConfig, LimitBehavior};
use lato_core::{TaskError, TaskErrorCode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionDecision {
    Start,
    Enqueue,
    Reject(TaskError),
}

pub(crate) fn decide(
    config: &CoordinatorConfig,
    global_running: usize,
    root_running: usize,
    queued: usize,
) -> AdmissionDecision {
    if global_running < config.max_global_running && root_running < config.max_running_per_root {
        return AdmissionDecision::Start;
    }
    match config.admission_behavior {
        LimitBehavior::Reject => AdmissionDecision::Reject(TaskError::new(
            TaskErrorCode::ConcurrencyLimit,
            "task concurrency capacity is exhausted",
        )),
        LimitBehavior::Queue if queued >= config.max_queue => AdmissionDecision::Reject(
            TaskError::new(TaskErrorCode::QueueFull, "task admission queue is full"),
        ),
        LimitBehavior::Queue => AdmissionDecision::Enqueue,
    }
}
