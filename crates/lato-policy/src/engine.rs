use crate::{
    ApprovalError, ApprovalLedger, NoopPolicyEventSink, PolicyEvent, PolicyEventKind,
    PolicyEventSink, approval_fingerprint, validate_sandbox_obligation,
};
use lato_core::{
    ApprovalRequest, ExecutionGrant, PolicyDecision, PolicyDenial, PolicyMode, PolicyRequest,
    SideEffect, ToolCapability, plan_mode_denial,
};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

const UNTRUSTED_EXTENSION_CODE: &str = "policy.untrusted_extension";
const PLAN_MODE_READONLY_MESSAGE: &str =
    "plan mode is read-only: the model may only inspect the workspace and draft plan.md";

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PolicyError {
    #[error(transparent)]
    Approval(#[from] ApprovalError),
    #[error("approval request fingerprint does not match its policy request")]
    ApprovalFingerprintMismatch,
    #[error("approval request summary does not match its policy request")]
    ApprovalSummaryMismatch,
    #[error("the supplied request does not require approval")]
    ApprovalNotRequired,
    #[error("policy request is invalid")]
    InvalidRequest,
    #[error("policy denied the request: {0}")]
    Denied(String),
}

impl PolicyError {
    pub fn code(&self) -> &str {
        match self {
            Self::Approval(error) => error.code(),
            Self::ApprovalFingerprintMismatch => "policy.grant_mismatch",
            Self::ApprovalSummaryMismatch => "policy.approval_request_mismatch",
            Self::ApprovalNotRequired => "policy.approval_not_required",
            Self::InvalidRequest => "policy.invalid_request",
            Self::Denied(code) => code,
        }
    }
}

pub struct PolicyEngine {
    ledger: Arc<ApprovalLedger>,
    sink: Arc<dyn PolicyEventSink>,
    /// Shared Plan-mode overlay flag; flipping it re-trims every subsequent
    /// evaluation and approval path of this engine (spec §4/§6). The Arc can
    /// be adopted from a session PlanModeRuntime so a single flag drives the
    /// policy overlay and the model catalog.
    plan_mode: RwLock<Arc<AtomicBool>>,
}

impl std::fmt::Debug for PolicyEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PolicyEngine")
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl PolicyEngine {
    pub fn new(ledger: Arc<ApprovalLedger>) -> Self {
        Self::with_sink(ledger, Arc::new(NoopPolicyEventSink))
    }

    pub fn with_sink(ledger: Arc<ApprovalLedger>, sink: Arc<dyn PolicyEventSink>) -> Self {
        Self {
            ledger,
            sink,
            plan_mode: RwLock::new(Arc::new(AtomicBool::new(false))),
        }
    }

    /// Replaces the engine's Plan-mode flag with a shared one, so the session
    /// plan state machine toggles policy and catalog in one store.
    pub fn adopt_plan_mode_flag(&self, flag: Arc<AtomicBool>) {
        *self
            .plan_mode
            .write()
            .expect("plan mode flag lock poisoned") = flag;
    }

    /// Engages or disengages the Plan-mode capability overlay.
    pub fn set_plan_mode(&self, active: bool) {
        self.plan_mode
            .read()
            .expect("plan mode flag lock poisoned")
            .store(active, Ordering::Release);
    }

    /// True while the Plan-mode overlay applies to evaluations.
    pub fn plan_mode_active(&self) -> bool {
        self.plan_mode
            .read()
            .expect("plan mode flag lock poisoned")
            .load(Ordering::Acquire)
    }

    pub fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
        let decision = self.evaluate_inner(request);
        self.emit_decision(request, &decision);
        decision
    }

    pub fn approve(&self, approval: &ApprovalRequest) -> Result<ExecutionGrant, PolicyError> {
        validate_request(&approval.request)?;
        let expected = approval_fingerprint(&approval.request)
            .map_err(|_| PolicyError::ApprovalFingerprintMismatch)?;
        if approval.fingerprint != expected {
            return Err(PolicyError::ApprovalFingerprintMismatch);
        }
        if let Some(denial) = denial(&approval.request, self.plan_mode_active()) {
            return Err(PolicyError::Denied(denial.code));
        }
        if !requires_human_approval(&approval.request) {
            return Err(PolicyError::ApprovalNotRequired);
        }
        if approval.summary != approval_summary(&approval.request) {
            return Err(PolicyError::ApprovalSummaryMismatch);
        }

        self.ledger
            .issue(expected, approval.request.sandbox.clone())
            .map_err(PolicyError::Approval)
    }

    /// Issues a one-shot grant after an independent trusted gate requested
    /// explicit user approval. The request is still fingerprinted, sandboxed,
    /// and rejected by every ordinary policy denial.
    pub fn approve_external_gate(
        &self,
        approval: &ApprovalRequest,
    ) -> Result<ExecutionGrant, PolicyError> {
        validate_request(&approval.request)?;
        let expected = approval_fingerprint(&approval.request)
            .map_err(|_| PolicyError::ApprovalFingerprintMismatch)?;
        if approval.fingerprint != expected {
            return Err(PolicyError::ApprovalFingerprintMismatch);
        }
        if let Some(denial) = denial(&approval.request, self.plan_mode_active()) {
            return Err(PolicyError::Denied(denial.code));
        }
        self.ledger
            .issue(expected, approval.request.sandbox.clone())
            .map_err(PolicyError::Approval)
    }

    pub fn consume(
        &self,
        grant: &ExecutionGrant,
        expected: &lato_core::ApprovalFingerprint,
        request: &PolicyRequest,
    ) -> Result<(), PolicyError> {
        self.ledger
            .consume(grant, expected)
            .map_err(PolicyError::Approval)?;
        self.sink.emit(event(
            PolicyEventKind::ApprovalConsumed,
            request,
            Some("allow".into()),
        ));
        Ok(())
    }

    fn evaluate_inner(&self, request: &PolicyRequest) -> PolicyDecision {
        if let Err(error) = validate_request(request) {
            return PolicyDecision::Deny(PolicyDenial::new(error.code(), error.to_string()));
        }
        if let Some(denial) = denial(request, self.plan_mode_active()) {
            return PolicyDecision::Deny(denial);
        }

        let fingerprint = match approval_fingerprint(request) {
            Ok(fingerprint) => fingerprint,
            Err(_) => {
                return PolicyDecision::Deny(PolicyDenial::new(
                    "policy.fingerprint_failed",
                    "could not bind authorization to the exact tool call",
                ));
            }
        };

        // In Plan mode every mutation-capable call was already denied above;
        // the sole survivor, `plan_draft`, must not stall planning behind an
        // interactive approval (headless `--plan` has no approver).
        if !request.plan_mode && requires_human_approval(request) {
            return PolicyDecision::RequireApproval(ApprovalRequest {
                request: request.clone(),
                fingerprint,
                summary: approval_summary(request),
            });
        }

        match self.ledger.issue(fingerprint, request.sandbox.clone()) {
            Ok(grant) => PolicyDecision::Allow(grant),
            Err(error) => PolicyDecision::Deny(PolicyDenial::new(error.code(), error.to_string())),
        }
    }

    fn emit_decision(&self, request: &PolicyRequest, decision: &PolicyDecision) {
        let code = match decision {
            PolicyDecision::Allow(_) => "allow",
            PolicyDecision::RequireApproval(_) => "require_approval",
            PolicyDecision::Deny(denial) => denial.code.as_str(),
        };
        self.sink.emit(event(
            PolicyEventKind::Evaluated,
            request,
            Some(code.to_owned()),
        ));
        match decision {
            PolicyDecision::RequireApproval(_) => self.sink.emit(event(
                PolicyEventKind::ApprovalRequested,
                request,
                Some("policy.approval_required".into()),
            )),
            PolicyDecision::Deny(denial) => self.sink.emit(event(
                PolicyEventKind::Denied,
                request,
                Some(denial.code.clone()),
            )),
            PolicyDecision::Allow(_) => {}
        }
    }
}

fn event(kind: PolicyEventKind, request: &PolicyRequest, code: Option<String>) -> PolicyEvent {
    PolicyEvent {
        kind,
        session_id: Some(request.session_id.clone()),
        turn_id: Some(request.turn_id.clone()),
        call_id: Some(request.call_id.clone()),
        tool_name: Some(request.tool_name.clone()),
        argument_digest: Some(request.arguments_digest.clone()),
        code,
        elapsed_ms: None,
        output_bytes: None,
    }
}

fn validate_request(request: &PolicyRequest) -> Result<(), PolicyError> {
    if request.arguments_digest.trim().is_empty() {
        return Err(PolicyError::InvalidRequest);
    }
    Ok(())
}

fn denial(request: &PolicyRequest, plan_mode: bool) -> Option<PolicyDenial> {
    if let Err(denial) = validate_sandbox_obligation(&request.sandbox) {
        return Some(denial);
    }
    if !request.project_trusted
        && request
            .capabilities
            .contains(&ToolCapability::ExtensionInvoke)
    {
        return Some(PolicyDenial::new(
            UNTRUSTED_EXTENSION_CODE,
            "untrusted project extensions cannot be invoked",
        ));
    }
    if plan_mode
        && let Some(code) = plan_mode_denial(
            &request.tool_name,
            &request.capabilities,
            request.side_effect,
            request.tool_layer,
        )
    {
        return Some(PolicyDenial::new(code, PLAN_MODE_READONLY_MESSAGE));
    }
    None
}

fn requires_human_approval(request: &PolicyRequest) -> bool {
    request.mode == PolicyMode::Ask
        && (matches!(
            request.side_effect,
            SideEffect::WorkspaceMutation | SideEffect::ExternalMutation
        ) || request.capabilities.iter().any(|capability| {
            matches!(
                capability,
                ToolCapability::FileWrite
                    | ToolCapability::ProcessSpawn
                    | ToolCapability::NetworkWrite
            )
        }))
}

fn approval_summary(request: &PolicyRequest) -> String {
    match &request.detail {
        Some(detail) => format!("{}: {}", request.tool_name, detail),
        None => format!(
            "{} requests {:?} access with {:?} side effects",
            request.tool_name, request.capabilities, request.side_effect
        ),
    }
}
