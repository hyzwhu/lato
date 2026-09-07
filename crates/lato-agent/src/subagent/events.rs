use lato_core::{TaskProgress, TaskUsage};
use lato_runtime::TaskReporter;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

use super::ChildSessionControl;

pub(crate) async fn relay_child_events(
    mut updates: mpsc::UnboundedReceiver<serde_json::Value>,
    outward: mpsc::UnboundedSender<serde_json::Value>,
    reporter: TaskReporter<ChildSessionControl>,
    progress: Arc<Mutex<TaskProgress>>,
    usage: Arc<Mutex<TaskUsage>>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            update = updates.recv() => {
                let Some(update) = update else { break };
                relay_one(update, &outward, &reporter, &progress, &usage).await;
            }
            _ = &mut stop => {
                while let Ok(update) = updates.try_recv() {
                    relay_one(update, &outward, &reporter, &progress, &usage).await;
                }
                break;
            }
        }
    }
}

async fn relay_one(
    update: serde_json::Value,
    outward: &mpsc::UnboundedSender<serde_json::Value>,
    reporter: &TaskReporter<ChildSessionControl>,
    progress: &Arc<Mutex<TaskProgress>>,
    usage: &Arc<Mutex<TaskUsage>>,
) {
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
            let bytes = update
                .pointer("/params/delta")
                .and_then(serde_json::Value::as_str)
                .map_or(0, str::len);
            let mut usage = usage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX));
            usage.total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
        }
        Some("session/reasoning") => {
            next.phase = Some("reasoning".into());
            next.message = Some("child model is reasoning".into());
        }
        Some("lato/session/context") => {
            let estimated = update
                .pointer("/params/estimatedInputTokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_else(|| usage.lock().unwrap_or_else(|p| p.into_inner()).input_tokens);
            let snapshot = {
                let mut usage = usage
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                usage.input_tokens = usage.input_tokens.max(estimated);
                usage.total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
                usage.clone()
            };
            let _ = reporter.report_usage(snapshot).await;
        }
        Some("session/tool_result") => {
            let snapshot = {
                let mut usage = usage
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                usage.tool_calls = usage.tool_calls.saturating_add(1);
                usage.clone()
            };
            let _ = reporter.report_usage(snapshot).await;
        }
        _ => {}
    }
    *progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = next.clone();
    let _ = reporter.report_progress(next).await;
    let snapshot = usage.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let _ = reporter.report_usage(snapshot).await;
    let _ = outward.send(update);
}
