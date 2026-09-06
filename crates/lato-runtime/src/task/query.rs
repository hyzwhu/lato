// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: bounded independent waiter and foreground-handoff registries

use crate::task::{CompletionDisposition, TaskInspection, TaskSnapshot, WaitOutcome};
use lato_core::TaskId;
use tokio::sync::oneshot;
use tokio::time::Instant;

pub(crate) struct BlockingWaiter {
    pub(crate) deadline: Instant,
    pub(crate) reply: oneshot::Sender<Result<WaitOutcome, lato_core::TaskError>>,
}

pub(crate) struct ForegroundWaiter {
    pub(crate) deadline: Option<Instant>,
    pub(crate) reply: oneshot::Sender<Result<CompletionDisposition, lato_core::TaskError>>,
}

pub(crate) fn inspection(snapshot: TaskSnapshot) -> TaskInspection {
    TaskInspection {
        owner: snapshot.node.owner.clone(),
        parent_id: snapshot.node.parent_id.clone(),
        root_id: snapshot.node.root_id.clone(),
        snapshot,
    }
}

pub(crate) fn caller_owns(
    state: &crate::task::state::CoordinatorState,
    caller: &crate::task::InspectCaller,
    target: &TaskId,
) -> bool {
    match caller {
        crate::task::InspectCaller::Admin => state.contains(target),
        crate::task::InspectCaller::Scoped { root_id, task_id } => {
            state.is_self_or_descendant(root_id, task_id, target)
        }
    }
}
