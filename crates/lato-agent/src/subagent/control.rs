use futures_util::future::BoxFuture;
use lato_core::TaskProgress;
use lato_runtime::{ActiveMessageAdmission, ActiveMessageDelivery, TaskChildControl};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::RuntimeSession;

pub(crate) struct ChildMessage {
    pub text: String,
}

pub struct ChildSessionControl {
    session: Arc<RuntimeSession>,
    cancellation: CancellationToken,
    messages: mpsc::Sender<ChildMessage>,
    progress: Arc<Mutex<TaskProgress>>,
}

impl ChildSessionControl {
    pub(crate) fn new(
        session: Arc<RuntimeSession>,
        cancellation: CancellationToken,
        messages: mpsc::Sender<ChildMessage>,
        progress: Arc<Mutex<TaskProgress>>,
    ) -> Self {
        Self {
            session,
            cancellation,
            messages,
            progress,
        }
    }
}

impl TaskChildControl for ChildSessionControl {
    fn progress(&self) -> TaskProgress {
        self.progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn send_active_message(
        &self,
        delivery: ActiveMessageDelivery,
    ) -> BoxFuture<'static, ActiveMessageAdmission> {
        let messages = self.messages.clone();
        Box::pin(async move {
            match delivery.commit_admission(|| {
                messages.try_send(ChildMessage {
                    text: delivery.message().text.to_string(),
                })
            }) {
                Some(Ok(())) => ActiveMessageAdmission::Admitted,
                Some(Err(tokio::sync::mpsc::error::TrySendError::Closed(_))) => {
                    ActiveMessageAdmission::ChannelClosed
                }
                Some(Err(tokio::sync::mpsc::error::TrySendError::Full(_))) | None => {
                    ActiveMessageAdmission::Rejected
                }
            }
        })
    }

    fn cancel(&self) {
        self.cancellation.cancel();
        let session = Arc::clone(&self.session);
        tokio::spawn(async move {
            let _ = session.cancel().await;
        });
    }
}
