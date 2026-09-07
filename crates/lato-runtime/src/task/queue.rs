// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator/queue.rs
// License: Apache-2.0
// Lato changes: stable FIFO queue that skips roots without available capacity

use lato_core::{TaskError, TaskErrorCode, TaskId};
use std::collections::VecDeque;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedTask {
    pub task_id: TaskId,
    pub root_id: TaskId,
}

#[derive(Debug)]
pub struct SpawnQueue {
    entries: VecDeque<QueuedTask>,
    capacity: usize,
}

impl SpawnQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(debug_assertions)]
    pub(crate) fn entries_for_audit(&self) -> impl Iterator<Item = &QueuedTask> {
        self.entries.iter()
    }

    pub fn push_back(&mut self, task: QueuedTask) -> Result<(), TaskError> {
        if self.entries.len() >= self.capacity {
            return Err(TaskError::new(
                TaskErrorCode::QueueFull,
                "task admission queue is full",
            ));
        }
        self.entries.push_back(task);
        Ok(())
    }

    pub fn drain_startable(
        &mut self,
        mut capacity: impl FnMut(&QueuedTask) -> bool,
    ) -> Vec<QueuedTask> {
        let mut kept = VecDeque::with_capacity(self.entries.len());
        let mut started = Vec::new();
        while let Some(task) = self.entries.pop_front() {
            if capacity(&task) {
                started.push(task);
            } else {
                kept.push_back(task);
            }
        }
        self.entries = kept;
        started
    }

    pub fn remove_matching(
        &mut self,
        mut predicate: impl FnMut(&QueuedTask) -> bool,
    ) -> Vec<QueuedTask> {
        let mut kept = VecDeque::with_capacity(self.entries.len());
        let mut removed = Vec::new();
        while let Some(task) = self.entries.pop_front() {
            if predicate(&task) {
                removed.push(task);
            } else {
                kept.push_back(task);
            }
        }
        self.entries = kept;
        removed
    }
}

impl Default for SpawnQueue {
    fn default() -> Self {
        Self::new(0)
    }
}
