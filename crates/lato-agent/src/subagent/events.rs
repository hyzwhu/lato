use lato_core::{TaskProgress, TaskUsage};
use lato_runtime::TaskReporter;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use super::ChildSessionControl;

pub(crate) async fn relay_child_events(
    mut updates: mpsc::UnboundedReceiver<serde_json::Value>,
    outward: mpsc::UnboundedSender<serde_json::Value>,
    reporter: TaskReporter<ChildSessionControl>,
    progress: Arc<Mutex<TaskProgress>>,
) {
    let mut usage = TaskUsage::default();
    while let Some(update) = updates.recv().await {
        let method = update.get("method").and_then(serde_json::Value::as_str);
        let mut next = progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match method {
            Some("session/update") => {
                next.phase = Some("model".into());
                next.message = Some("child model produced output".into());
                next.completed_units = next.completed_units.saturating_add(1);
            }
            Some("session/reasoning") => {
                next.phase = Some("reasoning".into());
                next.message = Some("child model is reasoning".into());
            }
            Some("lato/session/context") => {
                let estimated = update
                    .pointer("/params/estimatedInputTokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(usage.input_tokens);
                usage.input_tokens = usage.input_tokens.max(estimated);
                usage.total_tokens = usage.total_tokens.max(estimated);
                let _ = reporter.report_usage(usage.clone()).await;
            }
            _ => {}
        }
        *progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next.clone();
        let _ = reporter.report_progress(next).await;
        let _ = outward.send(update);
    }
}
