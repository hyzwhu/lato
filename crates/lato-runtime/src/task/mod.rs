// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs
// License: Apache-2.0
// Lato changes: provider-neutral actor boundary with root registration only

mod coordinator;
mod protocol;
mod runner;
mod state;

pub use coordinator::{TaskCoordinator, spawn_task_coordinator};
pub use protocol::*;
pub use runner::*;
