// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/active_message.rs
// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator/active_message.rs
// License: Apache-2.0
// Lato changes: generation-bound provider-neutral admissions for the process-local task tree

use futures_util::future::BoxFuture;
use lato_core::{SessionId, TaskId};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio_util::sync::WaitForCancellationFutureOwned;

/// Maximum UTF-8 byte length of one in-memory active-task message.
pub const MAX_ACTIVE_MESSAGE_BYTES: usize = 32 * 1024;

/// Maximum wait for a runner to prove or reject one admission.
pub const ACTIVE_MESSAGE_ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum wait for selected admissions after runner completion.
pub const ACTIVE_MESSAGE_FINALIZATION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(6);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveMessageOperation {
    Queue,
    Steer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveMessageRequest {
    task_id: TaskId,
    text: Arc<str>,
    operation: ActiveMessageOperation,
}

impl ActiveMessageRequest {
    pub fn try_new(
        task_id: TaskId,
        text: impl Into<Arc<str>>,
    ) -> Result<Self, ActiveMessageOutcome> {
        Self::try_new_with_operation(task_id, text, ActiveMessageOperation::Queue)
    }

    pub fn try_new_with_operation(
        task_id: TaskId,
        text: impl Into<Arc<str>>,
        operation: ActiveMessageOperation,
    ) -> Result<Self, ActiveMessageOutcome> {
        let text = text.into();
        if text.is_empty() || text.len() > MAX_ACTIVE_MESSAGE_BYTES {
            return Err(ActiveMessageOutcome::Limit {
                max_bytes: MAX_ACTIVE_MESSAGE_BYTES,
                observed_bytes: text.len(),
            });
        }
        Ok(Self {
            task_id,
            text,
            operation,
        })
    }

    pub fn task_id(&self) -> &TaskId {
        &self.task_id
    }

    pub fn text(&self) -> &Arc<str> {
        &self.text
    }

    pub fn operation(&self) -> ActiveMessageOperation {
        self.operation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveMessage {
    pub message_id: u64,
    pub sender_session_id: SessionId,
    pub sender_root_id: TaskId,
    pub sender_task_id: TaskId,
    pub text: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct ActiveMessageDelivery {
    message: ActiveMessage,
    operation: ActiveMessageOperation,
    generation: u64,
    admission_lease: Arc<ActiveMessageAdmissionLease>,
}

impl ActiveMessageDelivery {
    pub(crate) fn new(
        message: ActiveMessage,
        operation: ActiveMessageOperation,
        generation: u64,
        admission_lease: Arc<ActiveMessageAdmissionLease>,
    ) -> Self {
        Self {
            message,
            operation,
            generation,
            admission_lease,
        }
    }

    pub fn message(&self) -> &ActiveMessage {
        &self.message
    }

    pub fn operation(&self) -> ActiveMessageOperation {
        self.operation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Runs protected insertion synchronously while this generation's lease is open.
    pub fn commit_admission<T>(&self, insert: impl FnOnce() -> T) -> Option<T> {
        self.admission_lease.commit_admission(insert)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveMessageAdmission {
    Admitted,
    Unsupported,
    ChannelClosed,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ActiveMessageOutcome {
    Accepted {
        message_id: u64,
    },
    NotFoundOrNotOwned,
    NotActiveOrFinalizing,
    Saturated {
        max_in_flight: usize,
    },
    AdmissionUncertain,
    NotAcceptedBeforeDeadline,
    Unsupported,
    Limit {
        max_bytes: usize,
        observed_bytes: usize,
    },
    ChannelClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ActiveMessageLeaseState {
    Open,
    Claimed,
    Committed,
    Revoked,
}

/// Atomic proof that protected insertion happened before admission closed.
#[derive(Debug)]
pub struct ActiveMessageAdmissionLease {
    state: AtomicU8,
}

impl ActiveMessageAdmissionLease {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(ActiveMessageLeaseState::Open as u8),
        })
    }

    #[doc(hidden)]
    pub fn new_for_test() -> Arc<Self> {
        Self::new()
    }

    pub fn commit_admission<T>(&self, insert: impl FnOnce() -> T) -> Option<T> {
        self.state
            .compare_exchange(
                ActiveMessageLeaseState::Open as u8,
                ActiveMessageLeaseState::Claimed as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        let inserted = insert();
        self.state
            .store(ActiveMessageLeaseState::Committed as u8, Ordering::Release);
        Some(inserted)
    }

    pub fn settle(&self, admission: ActiveMessageAdmission) -> bool {
        let state = self.state.load(Ordering::Acquire);
        match admission {
            ActiveMessageAdmission::Admitted => state == ActiveMessageLeaseState::Committed as u8,
            ActiveMessageAdmission::Unsupported
            | ActiveMessageAdmission::ChannelClosed
            | ActiveMessageAdmission::Rejected => match state {
                value if value == ActiveMessageLeaseState::Open as u8 => self
                    .state
                    .compare_exchange(
                        ActiveMessageLeaseState::Open as u8,
                        ActiveMessageLeaseState::Revoked as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok(),
                value if value == ActiveMessageLeaseState::Revoked as u8 => true,
                _ => false,
            },
        }
    }

    pub(crate) fn revoke(&self) -> bool {
        match self.state.compare_exchange(
            ActiveMessageLeaseState::Open as u8,
            ActiveMessageLeaseState::Revoked as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(value) => value == ActiveMessageLeaseState::Revoked as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalDrainDisposition {
    Clean,
    Uncertain,
}

#[derive(Debug)]
pub(crate) enum ActiveMessageLifecycle {
    Open {
        in_flight: usize,
        disposition: TerminalDrainDisposition,
    },
    Finalizing {
        in_flight: usize,
        disposition: TerminalDrainDisposition,
    },
}

impl Default for ActiveMessageLifecycle {
    fn default() -> Self {
        Self::Open {
            in_flight: 0,
            disposition: TerminalDrainDisposition::Clean,
        }
    }
}

impl ActiveMessageLifecycle {
    pub(crate) fn begin(&mut self, cap: usize) -> Result<(), ActiveMessageOutcome> {
        let Self::Open { in_flight, .. } = self else {
            return Err(ActiveMessageOutcome::NotActiveOrFinalizing);
        };
        if *in_flight >= cap {
            return Err(ActiveMessageOutcome::Saturated { max_in_flight: cap });
        }
        *in_flight += 1;
        Ok(())
    }

    pub(crate) fn start_finalizing(&mut self) -> Option<bool> {
        match self {
            Self::Open {
                in_flight,
                disposition,
            } => {
                let result =
                    (*in_flight == 0).then_some(*disposition == TerminalDrainDisposition::Clean);
                *self = Self::Finalizing {
                    in_flight: *in_flight,
                    disposition: *disposition,
                };
                result
            }
            Self::Finalizing {
                in_flight,
                disposition,
            } => (*in_flight == 0).then_some(*disposition == TerminalDrainDisposition::Clean),
        }
    }

    pub(crate) fn finish(&mut self, settled: bool) -> Option<bool> {
        let (in_flight, disposition, finalizing) = match self {
            Self::Open {
                in_flight,
                disposition,
            } => (in_flight, disposition, false),
            Self::Finalizing {
                in_flight,
                disposition,
            } => (in_flight, disposition, true),
        };
        if !settled {
            *disposition = TerminalDrainDisposition::Uncertain;
        }
        *in_flight = in_flight
            .checked_sub(1)
            .expect("active-message completion without selected admission");
        (finalizing && *in_flight == 0).then_some(*disposition == TerminalDrainDisposition::Clean)
    }

    pub(crate) fn force_uncertain(&mut self) {
        match self {
            Self::Open { disposition, .. } | Self::Finalizing { disposition, .. } => {
                *disposition = TerminalDrainDisposition::Uncertain;
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ActiveMessageCompletionKind {
    Admission(ActiveMessageAdmission),
    Cancelled,
    DeadlineElapsed,
}

pub(crate) struct ActiveMessageCompletion {
    pub(crate) task_id: TaskId,
    pub(crate) generation: u64,
    pub(crate) message_id: u64,
    pub(crate) outcome: ActiveMessageCompletionKind,
    pub(crate) settled: bool,
    pub(crate) reply: Option<oneshot::Sender<ActiveMessageOutcome>>,
    pub(crate) _permit: OwnedSemaphorePermit,
}

impl ActiveMessageCompletion {
    pub(crate) fn protocol_outcome(&self) -> ActiveMessageOutcome {
        completion_outcome(self.outcome, self.settled, self.message_id)
    }
}

impl Drop for ActiveMessageCompletion {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let outcome = if !self.settled
                || matches!(
                    self.outcome,
                    ActiveMessageCompletionKind::Admission(ActiveMessageAdmission::Admitted)
                ) {
                ActiveMessageOutcome::AdmissionUncertain
            } else {
                completion_outcome(self.outcome, true, self.message_id)
            };
            let _ = reply.send(outcome);
        }
    }
}

pub(crate) struct ActiveMessageFuture {
    task_id: TaskId,
    generation: u64,
    message_id: u64,
    future: BoxFuture<'static, ActiveMessageAdmission>,
    cancellation: Pin<Box<WaitForCancellationFutureOwned>>,
    deadline: Pin<Box<tokio::time::Sleep>>,
    lease: Arc<ActiveMessageAdmissionLease>,
    permit: Option<OwnedSemaphorePermit>,
    reply: Option<oneshot::Sender<ActiveMessageOutcome>>,
}

impl ActiveMessageFuture {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        task_id: TaskId,
        generation: u64,
        message_id: u64,
        future: BoxFuture<'static, ActiveMessageAdmission>,
        cancellation: tokio_util::sync::CancellationToken,
        deadline: tokio::time::Instant,
        lease: Arc<ActiveMessageAdmissionLease>,
        permit: OwnedSemaphorePermit,
        reply: oneshot::Sender<ActiveMessageOutcome>,
    ) -> Self {
        Self {
            task_id,
            generation,
            message_id,
            future,
            cancellation: Box::pin(cancellation.cancelled_owned()),
            deadline: Box::pin(tokio::time::sleep_until(deadline)),
            lease,
            permit: Some(permit),
            reply: Some(reply),
        }
    }
}

impl Future for ActiveMessageFuture {
    type Output = ActiveMessageCompletion;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let admission_poll = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            this.future.as_mut().poll(context)
        }));
        let outcome = match admission_poll {
            Err(_) => ActiveMessageCompletionKind::Admission(ActiveMessageAdmission::ChannelClosed),
            Ok(Poll::Ready(admission)) => ActiveMessageCompletionKind::Admission(admission),
            Ok(Poll::Pending) if this.cancellation.as_mut().poll(context).is_ready() => {
                ActiveMessageCompletionKind::Cancelled
            }
            Ok(Poll::Pending) if this.deadline.as_mut().poll(context).is_ready() => {
                ActiveMessageCompletionKind::DeadlineElapsed
            }
            Ok(Poll::Pending) => return Poll::Pending,
        };
        let settled = match outcome {
            ActiveMessageCompletionKind::Admission(admission) => this.lease.settle(admission),
            ActiveMessageCompletionKind::Cancelled
            | ActiveMessageCompletionKind::DeadlineElapsed => this.lease.revoke(),
        };
        Poll::Ready(ActiveMessageCompletion {
            task_id: this.task_id.clone(),
            generation: this.generation,
            message_id: this.message_id,
            outcome,
            settled,
            reply: this.reply.take(),
            _permit: this
                .permit
                .take()
                .expect("active-message permit taken once"),
        })
    }
}

impl Drop for ActiveMessageFuture {
    fn drop(&mut self) {
        let outcome = if self.lease.revoke() {
            ActiveMessageOutcome::ChannelClosed
        } else {
            ActiveMessageOutcome::AdmissionUncertain
        };
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(outcome);
        }
    }
}

fn completion_outcome(
    outcome: ActiveMessageCompletionKind,
    settled: bool,
    message_id: u64,
) -> ActiveMessageOutcome {
    if !settled {
        return ActiveMessageOutcome::AdmissionUncertain;
    }
    match outcome {
        ActiveMessageCompletionKind::Admission(ActiveMessageAdmission::Admitted) => {
            ActiveMessageOutcome::Accepted { message_id }
        }
        ActiveMessageCompletionKind::Admission(ActiveMessageAdmission::Unsupported) => {
            ActiveMessageOutcome::Unsupported
        }
        ActiveMessageCompletionKind::Admission(ActiveMessageAdmission::ChannelClosed) => {
            ActiveMessageOutcome::ChannelClosed
        }
        ActiveMessageCompletionKind::Admission(ActiveMessageAdmission::Rejected) => {
            ActiveMessageOutcome::NotActiveOrFinalizing
        }
        ActiveMessageCompletionKind::Cancelled | ActiveMessageCompletionKind::DeadlineElapsed => {
            ActiveMessageOutcome::NotAcceptedBeforeDeadline
        }
    }
}
