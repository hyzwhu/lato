use std::{collections::VecDeque, sync::Mutex};

use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::error::ApiError;

const MAX_EVENTS: usize = 512;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_HISTORY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceEvent {
    pub sequence: u64,
    pub session_id: String,
    pub kind: String,
    pub data: Value,
}

#[derive(Default)]
struct History {
    sequence: u64,
    bytes: usize,
    entries: VecDeque<(ServiceEvent, usize)>,
}

pub(crate) struct EventHub {
    history: Mutex<History>,
    sender: broadcast::Sender<ServiceEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self {
            history: Mutex::new(History::default()),
            sender,
        }
    }
}

impl EventHub {
    pub fn push(&self, session_id: &str, kind: &str, mut data: Value) {
        let bytes = serde_json::to_vec(&data)
            .map(|b| b.len())
            .unwrap_or_default();
        if bytes > MAX_EVENT_BYTES {
            let operation_id = data
                .get("operationId")
                .and_then(Value::as_str)
                .filter(|id| id.len() <= 128)
                .map(str::to_owned);
            data = json!({"eventTruncated":true,"originalBytes":bytes,
                "originalKind":kind,"operationId":operation_id,
                "message":"event payload exceeds 64 KiB; consult operation result or the canonical journal"});
        }
        let mut history = self.history.lock().unwrap_or_else(|p| p.into_inner());
        history.sequence += 1;
        let event = ServiceEvent {
            sequence: history.sequence,
            session_id: session_id.into(),
            kind: kind.into(),
            data,
        };
        let size = serde_json::to_vec(&event)
            .map(|b| b.len())
            .unwrap_or_default();
        history.bytes += size;
        history.entries.push_back((event.clone(), size));
        while history.entries.len() > MAX_EVENTS || history.bytes > MAX_HISTORY_BYTES {
            if let Some((_, size)) = history.entries.pop_front() {
                history.bytes -= size;
            }
        }
        // Subscribe and replay are taken under the same lock as send, avoiding
        // a lost event between the historical snapshot and live subscription.
        let _ = self.sender.send(event);
    }

    pub fn subscribe(
        &self,
        after: Option<u64>,
    ) -> Result<(VecDeque<ServiceEvent>, broadcast::Receiver<ServiceEvent>), ApiError> {
        let history = self.history.lock().unwrap_or_else(|p| p.into_inner());
        let oldest = history
            .entries
            .front()
            .map(|(e, _)| e.sequence)
            .unwrap_or(history.sequence + 1);
        let after = after.unwrap_or(oldest.saturating_sub(1));
        if after > history.sequence {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "service.invalid_cursor",
                "event cursor is ahead of this service process",
            ));
        }
        if after < oldest.saturating_sub(1) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "service.event_gap",
                "event cursor has expired; inspect operation status and reconnect without a cursor",
            ));
        }
        let receiver = self.sender.subscribe();
        let replay = history
            .entries
            .iter()
            .filter(|(e, _)| e.sequence > after)
            .map(|(e, _)| e.clone())
            .collect();
        Ok((replay, receiver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_bounded_and_cursor_gaps_are_explicit() {
        let hub = EventHub::default();
        for n in 0..600 {
            hub.push("s", "delta", json!({"n":n}));
        }
        assert_eq!(
            hub.subscribe(Some(0)).err().unwrap().code,
            "service.event_gap"
        );
        assert_eq!(
            hub.subscribe(Some(601)).err().unwrap().code,
            "service.invalid_cursor"
        );
        let (events, _) = hub.subscribe(Some(599)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 600);
        hub.push("s", "large", json!({"text":"x".repeat(MAX_EVENT_BYTES)}));
        let (events, _) = hub.subscribe(Some(600)).unwrap();
        assert_eq!(events[0].data["eventTruncated"], true);
    }
}
