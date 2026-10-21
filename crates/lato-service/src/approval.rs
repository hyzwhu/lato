use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use lato_agent::ToolApproval;
use lato_core::ApprovalRequest;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::events::EventHub;

struct PendingApproval {
    session_id: String,
    view: Value,
    reply: oneshot::Sender<bool>,
}

pub(crate) struct ApprovalBridge {
    pending: Mutex<BTreeMap<String, PendingApproval>>,
    next_id: AtomicU64,
    timeout: Duration,
    events: Arc<EventHub>,
    stopped: AtomicBool,
}

impl ApprovalBridge {
    pub fn new(events: Arc<EventHub>, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            timeout,
            events,
            stopped: AtomicBool::new(false),
        })
    }

    pub fn list(&self, session_id: &str) -> Vec<Value> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|p| p.session_id == session_id)
            .map(|p| p.view.clone())
            .collect()
    }

    pub fn resolve(&self, session_id: &str, id: &str, allow: bool) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if !pending.get(id).is_some_and(|p| p.session_id == session_id) {
            return false;
        }
        pending
            .remove(id)
            .is_some_and(|p| p.reply.send(allow).is_ok())
    }

    pub fn deny_session(&self, session_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, p| p.session_id != session_id);
    }

    pub fn deny_all(&self) {
        self.stopped.store(true, Ordering::Release);
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}

struct PendingGuard<'a> {
    bridge: &'a ApprovalBridge,
    id: String,
    session_id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.bridge
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
        self.bridge.events.push(
            &self.session_id,
            "approval_closed",
            json!({"approvalId":self.id}),
        );
    }
}

impl ApprovalBridge {
    async fn wait_for_decision(
        &self,
        request: &ApprovalRequest,
        arguments: Option<&Value>,
        owner: Option<&lato_core::SessionId>,
    ) -> bool {
        let id = format!("approval-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let session_id = owner.unwrap_or(&request.request.session_id).to_string();
        let argument_bytes = arguments
            .map(|a| serde_json::to_vec(a).map(|v| v.len()).unwrap_or_default())
            .unwrap_or_default();
        let view = json!({"approvalId":id,"sessionId":session_id,"executionSessionId":request.request.session_id,"callId":request.request.call_id,
            "tool":request.request.tool_name,"summary":request.summary,
            "capabilities":request.request.capabilities,"sideEffect":request.request.side_effect,
            "fingerprint":request.fingerprint,"timeoutMs":self.timeout.as_millis(),
            "arguments":arguments.filter(|_|argument_bytes<=32*1024),
            "argumentsTruncated":argument_bytes>32*1024,"argumentsBytes":argument_bytes});
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if self.stopped.load(Ordering::Acquire) || pending.len() >= 64 {
                return false;
            }
            pending.insert(
                id.clone(),
                PendingApproval {
                    session_id: session_id.clone(),
                    view: view.clone(),
                    reply: tx,
                },
            );
        }
        let _guard = PendingGuard {
            bridge: self,
            id,
            session_id: session_id.clone(),
        };
        self.events.push(&session_id, "approval_requested", view);
        tokio::time::timeout(self.timeout, rx)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
    }
}

#[async_trait]
impl ToolApproval for ApprovalBridge {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        self.wait_for_decision(request, None, None).await
    }

    async fn approve_with_arguments(&self, request: &ApprovalRequest, arguments: &Value) -> bool {
        self.wait_for_decision(request, Some(arguments), None).await
    }

    async fn approve_delegated(
        &self,
        request: &ApprovalRequest,
        arguments: &Value,
        owner: &lato_core::SessionId,
    ) -> bool {
        self.wait_for_decision(request, Some(arguments), Some(owner))
            .await
    }
}
