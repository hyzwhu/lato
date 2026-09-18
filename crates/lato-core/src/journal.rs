use crate::{
    AgentError, ApprovalFingerprint, CancelReason, CompactionId, CompactionTrigger,
    HistoryReplacementReason, JournalValidation, ModelContent, ModelMessage, ModelRole,
    ProjectionError, Retryability, SandboxObligation, SessionId, SideEffect, ToolCallId,
    ToolCapability, ToolError, ToolIdempotency, ToolName, ToolOutput, TurnId, TurnOutput,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::JournalRecordId;

pub const JOURNAL_SCHEMA_VERSION: u32 = 1;

/// Journal schema version for AgentField run events (Phase 7C3).
///
/// AgentField envelopes carry schema version 2 while every other record
/// keeps version 1, so an old (pre-7C3) binary keeps reading journals that
/// contain no AgentField events and fails closed at the FIRST AgentField
/// envelope (its `project_journal` rejects version 2 outright). The
/// reader-version gate in [`decode_journal_envelope`] rejects any future
/// version BEFORE the AgentField payload is deserialized.
pub const AGENTFIELD_JOURNAL_SCHEMA_VERSION: u32 = 2;

/// Maximum `alias` bytes in an AgentField journal event (7C2 tool cap).
pub const AGENTFIELD_MAX_ALIAS_BYTES: usize = 64;
/// Maximum local run id / remote execution id bytes in an AgentField event.
pub const AGENTFIELD_MAX_ID_BYTES: usize = 128;
/// Maximum catalog revision bytes in an AgentField event.
pub const AGENTFIELD_MAX_REVISION_BYTES: usize = 128;
/// Maximum input digest bytes (SHA-256 hex is 64).
pub const AGENTFIELD_MAX_DIGEST_BYTES: usize = 128;
/// Maximum bounded result summary bytes in an AgentField event (spec §9.3).
pub const AGENTFIELD_MAX_SUMMARY_BYTES: usize = 8 * 1024;
/// Maximum stable error code bytes in an AgentField event.
pub const AGENTFIELD_MAX_ERROR_CODE_BYTES: usize = 64;

/// Normalized AgentField run status as journaled (7C2 `RunStatus`
/// projection). `unavailable` is a transient observation, never terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFieldRunStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
    Unavailable,
}

impl AgentFieldRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Unavailable => "unavailable",
        }
    }

    /// Terminal statuses are monotonic; `unavailable` is explicitly NOT
    /// terminal (spec §7.3).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::OutcomeUnknown
        )
    }

    pub fn from_status_str(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "outcome_unknown" => Self::OutcomeUnknown,
            "unavailable" => Self::Unavailable,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalDurability {
    Flush,
    SyncData,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillInvocationOrigin {
    Model,
    User,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAuditPhase {
    DispatchStarted,
    Completed,
    Failed,
    TimedOut,
    ArgumentsRewritten,
    OutputReplaced,
    StopContinuation,
    StopCapped,
    GenerationAdopted,
    GenerationRetired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAuditOutcome {
    Started,
    Applied,
    Skipped,
    FailedOpen,
    Blocked,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuditOutcome {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    UnsafeUrl,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionAuditRecord {
    Hook {
        generation: u64,
        hook_id: String,
        event: String,
        phase: HookAuditPhase,
        outcome: HookAuditOutcome,
        duration_ms: Option<u64>,
        effective_timeout_ms: u64,
        input_hash: String,
        output_hash: Option<String>,
        replaced_prior_hook_id: Option<String>,
        truncated: bool,
        redacted_reason: Option<String>,
    },
    SkillCatalogMaterialized {
        generation: u64,
        visible_count: u64,
        omitted_count: u64,
        catalog_hash: String,
    },
    SkillInvoked {
        qualified_name: String,
        origin: SkillInvocationOrigin,
        body_hash: String,
        allowed_tools_hash: Option<String>,
    },
    SkillRejected {
        requested_name_hash: String,
        origin: SkillInvocationOrigin,
        error_code: String,
    },
    /// MCP tools/call audit. Stores hashes only — never raw args, results,
    /// headers, env, or URL credentials.
    McpToolCall {
        generation: u64,
        server: String,
        tool: String,
        qualified_name: String,
        duration_ms: Option<u64>,
        outcome: McpAuditOutcome,
        args_hash: String,
        result_hash: Option<String>,
        truncated: bool,
        error_code: Option<String>,
        redacted_reason: Option<String>,
    },
}

/// Phase 7C3: versioned AgentField run journal events (spec §"Journal
/// 事件与最小字段"). Every variant carries the schema-relevant fields:
/// local run id, session id, alias, catalog revision, input digest, the
/// remote execution id (ONLY when known), normalized status/timestamps,
/// a bounded (≤ 8 KiB) result summary and the last stable error code.
///
/// Secrets are structurally excluded: no token, authorization value,
/// credential, base-URL userinfo, raw input, full remote result,
/// transcript, system prompt, workspace path, or unredacted server body
/// may be placed into any field — writers truncate/redact BEFORE
/// construction and [`validate_agentfield_event`] enforces the byte caps.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentFieldJournalEvent {
    /// The durable start intent, appended BEFORE any remote execute.
    #[serde(rename = "agentfield_run_intent_recorded")]
    AgentFieldRunIntentRecorded {
        run_id: String,
        session_id: String,
        alias: String,
        execute_target: String,
        catalog_revision: String,
        input_digest: String,
        created_at_ms: u64,
    },
    /// Appended after the remote execution id was received AND strictly
    /// validated; the durable witness of "one execution was started".
    #[serde(rename = "agentfield_execution_bound")]
    AgentFieldExecutionBound {
        run_id: String,
        session_id: String,
        execution_id: String,
        alias: String,
        catalog_revision: String,
        input_digest: String,
        bound_at_ms: u64,
    },
    /// A remote status observation for a bound run. Appended BEFORE the
    /// observed state becomes externally visible (designer constraint 3).
    #[serde(rename = "agentfield_status_observed")]
    AgentFieldStatusObserved {
        run_id: String,
        session_id: String,
        execution_id: Option<String>,
        status: AgentFieldRunStatus,
        observed_at_ms: u64,
        summary: Option<String>,
        summary_truncated: bool,
        last_error: Option<String>,
    },
    /// Durable cancel intent, appended BEFORE the single cancel send
    /// (designer constraint 2: durable-before-send, same discipline as
    /// the start intent).
    #[serde(rename = "agentfield_cancel_requested")]
    AgentFieldCancelRequested {
        run_id: String,
        session_id: String,
        execution_id: Option<String>,
        requested_at_ms: u64,
    },
    /// The run reached a terminal state. Terminal is monotonic on replay.
    #[serde(rename = "agentfield_run_terminal")]
    AgentFieldRunTerminal {
        run_id: String,
        session_id: String,
        execution_id: Option<String>,
        status: AgentFieldRunStatus,
        observed_at_ms: u64,
        summary: Option<String>,
        summary_truncated: bool,
        last_error: Option<String>,
    },
}

impl AgentFieldJournalEvent {
    pub fn run_id(&self) -> &str {
        match self {
            Self::AgentFieldRunIntentRecorded { run_id, .. }
            | Self::AgentFieldExecutionBound { run_id, .. }
            | Self::AgentFieldStatusObserved { run_id, .. }
            | Self::AgentFieldCancelRequested { run_id, .. }
            | Self::AgentFieldRunTerminal { run_id, .. } => run_id,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::AgentFieldRunIntentRecorded { session_id, .. }
            | Self::AgentFieldExecutionBound { session_id, .. }
            | Self::AgentFieldStatusObserved { session_id, .. }
            | Self::AgentFieldCancelRequested { session_id, .. }
            | Self::AgentFieldRunTerminal { session_id, .. } => session_id,
        }
    }

    /// The terminal status carried by `AgentFieldRunTerminal`, if any.
    pub fn terminal_status(&self) -> Option<AgentFieldRunStatus> {
        match self {
            Self::AgentFieldRunTerminal { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// The remote execution id carried by an event, if any (intent and bind
/// carry it structurally; the bind's id is the required witness).
fn agentfield_event_execution_id(event: &AgentFieldJournalEvent) -> Option<&str> {
    match event {
        AgentFieldJournalEvent::AgentFieldRunIntentRecorded { .. } => None,
        AgentFieldJournalEvent::AgentFieldExecutionBound { execution_id, .. } => {
            Some(execution_id.as_str())
        }
        AgentFieldJournalEvent::AgentFieldStatusObserved { execution_id, .. }
        | AgentFieldJournalEvent::AgentFieldCancelRequested { execution_id, .. }
        | AgentFieldJournalEvent::AgentFieldRunTerminal { execution_id, .. } => {
            execution_id.as_deref()
        }
    }
}

/// Byte-cap and content-shape validation for AgentField journal events.
/// Replay fails closed ([`JournalError::Corrupt`]) on any violation, so
/// oversized or malformed payloads can never be restored into a manager.
pub fn validate_agentfield_event(event: &AgentFieldJournalEvent) -> Result<(), JournalError> {
    let corrupt = |message: String| JournalError::Corrupt {
        message: format!("agentfield event {}: {message}", event.run_id()),
    };
    let bounded =
        |name: &str, value: &str, cap: usize, non_empty: bool| -> Result<(), JournalError> {
            if value.len() > cap || (non_empty && value.is_empty()) {
                return Err(corrupt(format!("{name} length out of bounds")));
            }
            Ok(())
        };
    let bounded_opt =
        |name: &str, value: &Option<String>, cap: usize| -> Result<(), JournalError> {
            if let Some(value) = value {
                return bounded(name, value, cap, false);
            }
            Ok(())
        };
    bounded("run_id", event.run_id(), AGENTFIELD_MAX_ID_BYTES, true)?;
    bounded(
        "session_id",
        event.session_id(),
        AGENTFIELD_MAX_ID_BYTES,
        true,
    )?;
    if let Some(execution_id) = agentfield_event_execution_id(event) {
        bounded("execution_id", execution_id, AGENTFIELD_MAX_ID_BYTES, false)?;
    }
    match event {
        AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
            alias,
            execute_target,
            catalog_revision,
            input_digest,
            ..
        } => {
            bounded("alias", alias, AGENTFIELD_MAX_ALIAS_BYTES, true)?;
            bounded(
                "execute_target",
                execute_target,
                AGENTFIELD_MAX_ID_BYTES,
                true,
            )?;
            bounded(
                "catalog_revision",
                catalog_revision,
                AGENTFIELD_MAX_REVISION_BYTES,
                true,
            )?;
            bounded(
                "input_digest",
                input_digest,
                AGENTFIELD_MAX_DIGEST_BYTES,
                true,
            )?;
        }
        AgentFieldJournalEvent::AgentFieldExecutionBound {
            execution_id,
            alias,
            catalog_revision,
            input_digest,
            ..
        } => {
            bounded("execution_id", execution_id, AGENTFIELD_MAX_ID_BYTES, true)?;
            bounded("alias", alias, AGENTFIELD_MAX_ALIAS_BYTES, true)?;
            bounded(
                "catalog_revision",
                catalog_revision,
                AGENTFIELD_MAX_REVISION_BYTES,
                true,
            )?;
            bounded(
                "input_digest",
                input_digest,
                AGENTFIELD_MAX_DIGEST_BYTES,
                true,
            )?;
        }
        AgentFieldJournalEvent::AgentFieldStatusObserved {
            execution_id,
            summary,
            last_error,
            ..
        }
        | AgentFieldJournalEvent::AgentFieldRunTerminal {
            execution_id,
            summary,
            last_error,
            ..
        } => {
            bounded_opt("execution_id", execution_id, AGENTFIELD_MAX_ID_BYTES)?;
            bounded_opt("summary", summary, AGENTFIELD_MAX_SUMMARY_BYTES)?;
            bounded_opt("last_error", last_error, AGENTFIELD_MAX_ERROR_CODE_BYTES)?;
        }
        AgentFieldJournalEvent::AgentFieldCancelRequested { execution_id, .. } => {
            bounded_opt("execution_id", execution_id, AGENTFIELD_MAX_ID_BYTES)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct JournalEnvelope {
    pub schema_version: u32,
    pub record_id: JournalRecordId,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub journal_sequence: u64,
    pub timestamp_ms: u64,
    pub record: JournalRecord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAuditStage {
    Evaluated,
    ApprovalRequested,
    ApprovalResolved,
    GrantConsumed,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAuditDecision {
    Allowed,
    ApprovalRequired,
    Approved,
    Denied { code: String },
    Consumed,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PolicyAuditRecord {
    pub stage: PolicyAuditStage,
    pub decision: PolicyAuditDecision,
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub request_hash: String,
    pub approval_fingerprint: ApprovalFingerprint,
    pub capabilities: Vec<ToolCapability>,
    pub sandbox: SandboxObligation,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PreparedToolAudit {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub request_hash: String,
    pub approval_fingerprint: ApprovalFingerprint,
    pub idempotency: ToolIdempotency,
    pub side_effect: SideEffect,
    pub sandbox: SandboxObligation,
    pub capabilities: Vec<ToolCapability>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalRecord {
    SessionStarted,
    TurnInputAccepted {
        input: crate::UserInput,
    },
    ConversationItemCommitted {
        message: ModelMessage,
    },
    PolicyDecisionCommitted {
        audit: PolicyAuditRecord,
    },
    ToolCallRequested {
        call_id: ToolCallId,
        name: ToolName,
        arguments: Value,
        request_hash: String,
    },
    ToolCallPrepared {
        audit: PreparedToolAudit,
    },
    ToolCallCompleted {
        call_id: ToolCallId,
        request_hash: String,
        result: Result<ToolOutput, ToolError>,
    },
    ToolCallRejected {
        call_id: ToolCallId,
        request_hash: String,
        error: ToolError,
    },
    TurnCompleted {
        output: TurnOutput,
    },
    TurnFailed {
        error: AgentError,
    },
    TurnCancelled {
        reason: CancelReason,
    },
    CompactionRequested {
        compaction_id: CompactionId,
        trigger: CompactionTrigger,
        user_context: Option<String>,
    },
    CompactionFailed {
        compaction_id: CompactionId,
        error_code: String,
    },
    CompactionCancelled {
        compaction_id: CompactionId,
    },
    ModelSelected {
        selection: crate::ModelSelection,
        model_family: Option<String>,
        context_window: Option<u64>,
    },
    PluginSnapshotAdopted {
        summary: crate::PluginSnapshotSummary,
    },
    ExtensionAudit {
        audit: ExtensionAuditRecord,
    },
    SessionStopped,
    LegacyTranscriptImported {
        source_version: u32,
        item_count: u64,
        content_digest: String,
    },
    HistoryProjectionReplaced {
        checkpoint_id: String,
        checkpoint_digest: String,
        replaced_through_sequence: u64,
        replaced_through_record_id: JournalRecordId,
        replacement_entry_count: u64,
        history_digest: String,
        reason: HistoryReplacementReason,
        prior_checkpoint_id: Option<String>,
    },
    PlanModeTransitioned {
        activation: u64,
        from: crate::plan::PlanPhase,
        to: crate::plan::PlanPhase,
        command: crate::plan::PlanCommand,
    },
    PlanApprovalRecorded {
        activation: u64,
        generation: u64,
        content_hash: String,
        approver: String,
        approved_at_ms: u64,
    },
    PlanApprovalRevoked {
        activation: u64,
        generation: u64,
        reason: String,
    },
    /// Phase 7C3: AgentField run event. These envelopes always carry
    /// `schema_version == AGENTFIELD_JOURNAL_SCHEMA_VERSION` (see
    /// [`decode_journal_envelope`] for the reader-version gate).
    #[serde(rename = "agentfield")]
    AgentField {
        event: AgentFieldJournalEvent,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct UnresolvedToolCall {
    pub call_id: ToolCallId,
    pub request_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalTerminal {
    TurnCompleted,
    TurnFailed,
    TurnCancelled,
    SessionStopped,
}

/// One AgentField run recovered from a journal replay (Phase 7C3).
/// Carries exactly the safe projection surface of [`crate::state`]-level
/// runs — no tokens, no raw inputs, bounded summaries only.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentFieldRecoveredRun {
    pub run_id: String,
    pub alias: String,
    pub execute_target: String,
    pub revision: String,
    pub input_digest: String,
    pub execution_id: Option<String>,
    pub status: AgentFieldRunStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub summary: Option<String>,
    pub summary_truncated: bool,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionProjection {
    pub session_id: SessionId,
    pub messages: Vec<ModelMessage>,
    pub next_journal_sequence: u64,
    pub unresolved_tools: Vec<UnresolvedToolCall>,
    pub terminal: Option<JournalTerminal>,
    pub active_checkpoint_id: Option<String>,
    pub model_selection: Option<crate::ModelSelection>,
    pub model_family: Option<String>,
    pub model_context_window: Option<u64>,
    /// AgentField runs rebuilt from the journal (insertion order). Empty
    /// for journals without 7C3 events — old sessions resume unchanged.
    pub agentfield_runs: Vec<AgentFieldRecoveredRun>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JournalReplay {
    pub exists: bool,
    pub envelopes: Vec<JournalEnvelope>,
    pub projection: SessionProjection,
}

impl JournalReplay {
    pub fn empty(session_id: SessionId) -> Self {
        Self {
            exists: false,
            envelopes: Vec::new(),
            projection: SessionProjection {
                session_id,
                messages: Vec::new(),
                next_journal_sequence: 0,
                unresolved_tools: Vec::new(),
                terminal: None,
                active_checkpoint_id: None,
                model_selection: None,
                model_family: None,
                model_context_window: None,
                agentfield_runs: Vec::new(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum JournalError {
    #[error("journal io: {message}")]
    Io { message: String },
    #[error("journal full at sequence {sequence}: limit {limit}")]
    Full { sequence: u64, limit: u64 },
    #[error("journal parse at line {line}: {message}")]
    Parse { line: usize, message: String },
    #[error("unsafe journal restore: {message}")]
    UnsafeRestore { message: String },
    #[error("journal sequence mismatch: expected {expected}, found {actual}")]
    Sequence { expected: u64, actual: u64 },
    #[error("duplicate journal record id {record_id}")]
    DuplicateRecord { record_id: JournalRecordId },
    #[error("unsupported journal schema {actual}; expected {expected}")]
    SchemaUnsupported { expected: u32, actual: u32 },
    #[error("journal session mismatch: expected {expected}, found {actual}")]
    SessionMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    #[error("journal replay divergence for tool call {call_id}: {message}")]
    Divergence {
        call_id: ToolCallId,
        message: String,
    },
    #[error("tool call {call_id} has an unknown side-effect outcome")]
    IncompleteSideEffect { call_id: ToolCallId },
    #[error("legacy journal migration failed: {message}")]
    MigrationFailed { message: String },
    #[error("corrupt journal: {message}")]
    Corrupt { message: String },
    #[error(transparent)]
    Projection(#[from] ProjectionError),
}

impl JournalError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "journal.io",
            Self::Full { .. } => "journal.full",
            Self::Parse { .. } => "journal.parse",
            Self::UnsafeRestore { .. } => "journal.unsafe_restore",
            Self::Sequence { .. } => "journal.sequence",
            Self::DuplicateRecord { .. } => "journal.duplicate_record",
            Self::SchemaUnsupported { .. } => "journal.schema_unsupported",
            Self::SessionMismatch { .. } => "journal.session_mismatch",
            Self::Divergence { .. } => "journal.divergence",
            Self::IncompleteSideEffect { .. } => "journal.incomplete_side_effect",
            Self::MigrationFailed { .. } => "journal.migration_failed",
            Self::Corrupt { .. } => "journal.corrupt",
            Self::Projection(error) => error.code(),
        }
    }

    pub fn retryability(&self) -> Retryability {
        match self {
            Self::Io { .. } => Retryability::AfterBackoff,
            Self::Projection(error) => error.retryability(),
            _ => Retryability::Never,
        }
    }
}

/// Reader-version gate for one journal line (Phase 7C3): a two-stage
/// decode that validates the OUTER envelope header BEFORE the specific
/// record payload is deserialized.
///
/// - Future schema versions (anything above
///   [`AGENTFIELD_JOURNAL_SCHEMA_VERSION`]) are rejected with
///   [`JournalError::SchemaUnsupported`] before any AgentField payload is
///   parsed — a newer binary's journal can never be half-interpreted by
///   this reader.
/// - AgentField records are only accepted at
///   [`AGENTFIELD_JOURNAL_SCHEMA_VERSION`]; an AgentField payload wrapped
///   in any other version (e.g. hand-forged version 1) fails closed.
/// - Non-AgentField records are only accepted at
///   [`JOURNAL_SCHEMA_VERSION`], which keeps old sessions (no 7C3
///   events) fully readable by this reader.
pub fn decode_journal_envelope(line: &[u8]) -> Result<JournalEnvelope, JournalError> {
    let value: Value = serde_json::from_slice(line).map_err(|error| JournalError::Parse {
        line: 0,
        message: error.to_string(),
    })?;
    let corrupt = |message: String| JournalError::Corrupt {
        message: format!("journal envelope: {message}"),
    };
    let schema_version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| corrupt("missing schema_version".into()))?;
    let schema_version =
        u32::try_from(schema_version).map_err(|_| corrupt("schema_version out of range".into()))?;
    if schema_version > AGENTFIELD_JOURNAL_SCHEMA_VERSION {
        return Err(JournalError::SchemaUnsupported {
            expected: AGENTFIELD_JOURNAL_SCHEMA_VERSION,
            actual: schema_version,
        });
    }
    let record_type = value
        .get("record")
        .and_then(|record| record.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("missing record type".into()))?;
    if record_type == "agentfield" && schema_version != AGENTFIELD_JOURNAL_SCHEMA_VERSION {
        return Err(JournalError::SchemaUnsupported {
            expected: AGENTFIELD_JOURNAL_SCHEMA_VERSION,
            actual: schema_version,
        });
    }
    if schema_version == AGENTFIELD_JOURNAL_SCHEMA_VERSION && record_type != "agentfield" {
        return Err(corrupt(
            "only agentfield records may use the 7C3 schema version".into(),
        ));
    }
    serde_json::from_value(value).map_err(|error| JournalError::Parse {
        line: 0,
        message: error.to_string(),
    })
}

#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError>;

    async fn replay(&self, session_id: &SessionId) -> Result<JournalReplay, JournalError>;

    async fn import_if_absent(
        &self,
        session_id: &SessionId,
        envelopes: Vec<JournalEnvelope>,
    ) -> Result<JournalReplay, JournalError>;

    async fn list_sessions(&self) -> Result<Vec<SessionId>, JournalError>;

    async fn shutdown(&self, session_id: &SessionId) -> Result<(), JournalError>;
}

pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

pub fn journal_request_hash(kind: &str, payload: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"lato-journal-request-v1\0");
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    hasher.update(canonical_json(payload).to_string().as_bytes());
    format!("sha256:v1:{:x}", hasher.finalize())
}

pub fn project_journal(
    session_id: &SessionId,
    envelopes: &[JournalEnvelope],
) -> Result<SessionProjection, JournalError> {
    #[derive(Clone)]
    struct Requested {
        name: ToolName,
        arguments: Value,
        hash: String,
        prepared: bool,
    }

    let mut record_ids = BTreeSet::new();
    let mut requested = BTreeMap::<ToolCallId, Requested>::new();
    let mut messages = Vec::new();
    let mut terminal = None;
    let mut active_checkpoint_id = None;
    let mut model_selection = None;
    let mut model_family = None;
    let mut model_context_window = None;
    let mut checkpoint_ids = BTreeSet::new();
    let mut agentfield_runs = BTreeMap::<String, AgentFieldRecoveredRun>::new();
    let mut agentfield_order: Vec<String> = Vec::new();

    for (index, envelope) in envelopes.iter().enumerate() {
        // Reader-version gate: version 1 covers every pre-7C3 record;
        // version 2 is reserved for AgentField events. Anything else —
        // including future versions — fails closed here, BEFORE any
        // payload is interpreted or any manager state is installed.
        let is_agentfield = matches!(envelope.record, JournalRecord::AgentField { .. });
        match envelope.schema_version {
            JOURNAL_SCHEMA_VERSION if !is_agentfield => {}
            AGENTFIELD_JOURNAL_SCHEMA_VERSION if is_agentfield => {}
            _ => {
                return Err(JournalError::SchemaUnsupported {
                    expected: if is_agentfield {
                        AGENTFIELD_JOURNAL_SCHEMA_VERSION
                    } else {
                        JOURNAL_SCHEMA_VERSION
                    },
                    actual: envelope.schema_version,
                });
            }
        }
        if &envelope.session_id != session_id {
            return Err(JournalError::SessionMismatch {
                expected: session_id.clone(),
                actual: envelope.session_id.clone(),
            });
        }
        let expected = index as u64;
        if envelope.journal_sequence != expected {
            return Err(JournalError::Sequence {
                expected,
                actual: envelope.journal_sequence,
            });
        }
        if !record_ids.insert(envelope.record_id.clone()) {
            return Err(JournalError::DuplicateRecord {
                record_id: envelope.record_id.clone(),
            });
        }

        match &envelope.record {
            JournalRecord::SessionStarted
            | JournalRecord::PolicyDecisionCommitted { .. }
            | JournalRecord::CompactionRequested { .. }
            | JournalRecord::CompactionFailed { .. }
            | JournalRecord::CompactionCancelled { .. }
            | JournalRecord::PluginSnapshotAdopted { .. }
            | JournalRecord::ExtensionAudit { .. }
            | JournalRecord::LegacyTranscriptImported { .. }
            | JournalRecord::PlanModeTransitioned { .. }
            | JournalRecord::PlanApprovalRecorded { .. }
            | JournalRecord::PlanApprovalRevoked { .. } => {}
            JournalRecord::ModelSelected {
                selection,
                model_family: family,
                context_window,
            } => {
                model_selection = Some(selection.clone());
                model_family = family.clone();
                model_context_window = *context_window;
            }
            JournalRecord::HistoryProjectionReplaced { checkpoint_id, .. } => {
                if requested.values().any(|call| call.prepared) {
                    return Err(JournalError::Corrupt {
                        message: "history replaced with an unresolved prepared tool".into(),
                    });
                }
                if !checkpoint_ids.insert(checkpoint_id.clone()) {
                    return Err(JournalError::Corrupt {
                        message: format!("duplicate history checkpoint {checkpoint_id}"),
                    });
                }
                active_checkpoint_id = Some(checkpoint_id.clone());
            }
            JournalRecord::TurnInputAccepted { input } => messages.push(ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: input.text.clone(),
                }],
            }),
            JournalRecord::ConversationItemCommitted { message } => messages.push(message.clone()),
            JournalRecord::ToolCallRequested {
                call_id,
                name,
                arguments,
                request_hash,
            } => {
                if requested.contains_key(call_id) {
                    return Err(divergence(call_id, "tool call was requested twice"));
                }
                requested.insert(
                    call_id.clone(),
                    Requested {
                        name: name.clone(),
                        arguments: arguments.clone(),
                        hash: request_hash.clone(),
                        prepared: false,
                    },
                );
                messages.push(ModelMessage {
                    role: ModelRole::Assistant,
                    content: vec![ModelContent::ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    }],
                });
            }
            JournalRecord::ToolCallPrepared { audit } => {
                let Some(call) = requested.get_mut(&audit.call_id) else {
                    return Err(divergence(&audit.call_id, "preparation has no request"));
                };
                if call.hash != audit.request_hash || call.prepared {
                    return Err(divergence(
                        &audit.call_id,
                        "preparation does not match the requested call",
                    ));
                }
                call.prepared = true;
            }
            JournalRecord::ToolCallCompleted {
                call_id,
                request_hash,
                result,
            } => {
                let Some(call) = requested.remove(call_id) else {
                    return Err(divergence(call_id, "completion has no request"));
                };
                if !call.prepared || call.hash != *request_hash {
                    return Err(divergence(call_id, "completion does not match preparation"));
                }
                let output = match result {
                    Ok(output) => output.content.clone(),
                    Err(error) => format!("ERROR [{}]: {}", error.code, error.message),
                };
                messages.push(tool_result(call_id.clone(), output));
            }
            JournalRecord::ToolCallRejected {
                call_id,
                request_hash,
                error,
            } => {
                let Some(call) = requested.remove(call_id) else {
                    return Err(divergence(call_id, "rejection has no request"));
                };
                if call.prepared || call.hash != *request_hash {
                    return Err(divergence(call_id, "rejection does not match request"));
                }
                messages.push(tool_result(
                    call_id.clone(),
                    format!("ERROR [{}]: {}", error.code, error.message),
                ));
            }
            JournalRecord::TurnCompleted { .. } => terminal = Some(JournalTerminal::TurnCompleted),
            JournalRecord::TurnFailed { .. } => terminal = Some(JournalTerminal::TurnFailed),
            JournalRecord::TurnCancelled { .. } => terminal = Some(JournalTerminal::TurnCancelled),
            JournalRecord::SessionStopped => terminal = Some(JournalTerminal::SessionStopped),
            JournalRecord::AgentField { event } => {
                project_agentfield_event(
                    &envelope.session_id,
                    event,
                    &mut agentfield_runs,
                    &mut agentfield_order,
                )?;
            }
        }
    }

    let unresolved_tools = requested
        .into_iter()
        .filter_map(|(call_id, call)| {
            let _ = (&call.name, &call.arguments);
            call.prepared.then_some(UnresolvedToolCall {
                call_id,
                request_hash: call.hash,
            })
        })
        .collect();

    Ok(SessionProjection {
        session_id: session_id.clone(),
        messages,
        next_journal_sequence: envelopes.len() as u64,
        unresolved_tools,
        terminal,
        active_checkpoint_id,
        model_selection,
        model_family,
        model_context_window,
        agentfield_runs: agentfield_order
            .into_iter()
            .filter_map(|run_id| agentfield_runs.remove(&run_id))
            .collect(),
    })
}

/// Replay state machine for AgentField journal events (Phase 7C3,
/// "单写者、顺序与原子性合同" item 5). Idempotent for duplicated
/// observations; fail closed on illegal transitions, missing intents,
/// conflicting remote bindings, or oversized payloads. Zero network by
/// construction — this function never leaves the caller's process.
fn project_agentfield_event(
    session_id: &SessionId,
    event: &AgentFieldJournalEvent,
    runs: &mut BTreeMap<String, AgentFieldRecoveredRun>,
    order: &mut Vec<String>,
) -> Result<(), JournalError> {
    validate_agentfield_event(event)?;
    let run_id = event.run_id().to_owned();
    // The event session must match the journal session (fail closed on
    // cross-session replay; installing a foreign run is refused).
    if event.session_id() != session_id.as_str() {
        return Err(JournalError::SessionMismatch {
            expected: session_id.clone(),
            actual: SessionId::from(event.session_id().to_owned()),
        });
    }
    let corrupt = |message: String| JournalError::Corrupt {
        message: format!("agentfield run {run_id}: {message}"),
    };
    match event {
        AgentFieldJournalEvent::AgentFieldRunIntentRecorded {
            alias,
            execute_target,
            catalog_revision,
            input_digest,
            created_at_ms,
            ..
        } => {
            if runs.contains_key(&run_id) {
                // Duplicate intent after any follow-up event is an illegal
                // replay; a duplicated intent with nothing in between is
                // treated idempotently below only if byte-identical.
                return Err(corrupt("intent recorded twice".into()));
            }
            runs.insert(
                run_id.clone(),
                AgentFieldRecoveredRun {
                    run_id: run_id.clone(),
                    alias: alias.clone(),
                    execute_target: execute_target.clone(),
                    revision: catalog_revision.clone(),
                    input_digest: input_digest.clone(),
                    execution_id: None,
                    status: AgentFieldRunStatus::Queued,
                    created_at_ms: *created_at_ms,
                    updated_at_ms: *created_at_ms,
                    summary: None,
                    summary_truncated: false,
                    last_error: None,
                },
            );
            order.push(run_id);
        }
        AgentFieldJournalEvent::AgentFieldExecutionBound {
            execution_id,
            bound_at_ms,
            ..
        } => {
            let Some(run) = runs.get_mut(&run_id) else {
                return Err(corrupt("bind event has no prior intent".into()));
            };
            match &run.execution_id {
                Some(existing) if existing != execution_id => {
                    return Err(corrupt(
                        "run bound to a different remote execution id".into(),
                    ));
                }
                Some(_) => {}
                None => {
                    run.execution_id = Some(execution_id.clone());
                    run.status = AgentFieldRunStatus::Queued;
                    run.updated_at_ms = *bound_at_ms;
                }
            }
        }
        AgentFieldJournalEvent::AgentFieldStatusObserved {
            status,
            observed_at_ms,
            summary,
            summary_truncated,
            last_error,
            ..
        } => {
            let Some(run) = runs.get_mut(&run_id) else {
                return Err(corrupt("status observation has no prior intent".into()));
            };
            // Late observation never overwrites a terminal (spec item 3).
            if run.status.is_terminal() {
                return Ok(());
            }
            run.status = *status;
            run.updated_at_ms = *observed_at_ms;
            if summary.is_some() {
                run.summary = summary.clone();
                run.summary_truncated = *summary_truncated;
            }
            if let Some(error) = last_error {
                run.last_error = Some(error.clone());
            }
        }
        AgentFieldJournalEvent::AgentFieldCancelRequested { .. } => {
            // A cancel request does not change the status by itself; the
            // outcome arrives via a later observed/terminal event. Missing
            // intent or a terminal run makes it a no-op (idempotent).
            if !runs.contains_key(&run_id) {
                return Err(corrupt("cancel request has no prior intent".into()));
            }
        }
        AgentFieldJournalEvent::AgentFieldRunTerminal {
            status,
            observed_at_ms,
            summary,
            summary_truncated,
            last_error,
            ..
        } => {
            let Some(run) = runs.get_mut(&run_id) else {
                return Err(corrupt("terminal event has no prior intent".into()));
            };
            if !status.is_terminal() {
                return Err(corrupt(format!(
                    "terminal event carries non-terminal status {}",
                    status.as_str()
                )));
            }
            if run.status.is_terminal() {
                if run.status == *status {
                    // Idempotent duplicate terminal.
                    return Ok(());
                }
                return Err(corrupt(
                    "terminal state changed after being committed".into(),
                ));
            }
            run.status = *status;
            run.updated_at_ms = *observed_at_ms;
            if summary.is_some() {
                run.summary = summary.clone();
                run.summary_truncated = *summary_truncated;
            }
            if let Some(error) = last_error {
                run.last_error = Some(error.clone());
            }
        }
    }
    Ok(())
}

pub fn validate_journal(
    session_id: &SessionId,
    envelopes: &[JournalEnvelope],
) -> Result<JournalValidation, JournalError> {
    let projection = project_journal(session_id, envelopes)?;
    Ok(JournalValidation {
        session_id: session_id.clone(),
        next_journal_sequence: projection.next_journal_sequence,
        last_record_id: envelopes.last().map(|item| item.record_id.clone()),
        message_count: projection.messages.len() as u64,
        history_digest: crate::history_digest(&projection.messages)?,
        unresolved_tools: projection.unresolved_tools,
        terminal: projection.terminal,
        active_checkpoint_id: projection.active_checkpoint_id,
    })
}

pub fn projection_message(record: &JournalRecord) -> Option<ModelMessage> {
    match record {
        JournalRecord::TurnInputAccepted { input } => Some(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: input.text.clone(),
            }],
        }),
        JournalRecord::ConversationItemCommitted { message } => Some(message.clone()),
        JournalRecord::ToolCallRequested {
            call_id,
            name,
            arguments,
            ..
        } => Some(ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }],
        }),
        JournalRecord::ToolCallCompleted {
            call_id, result, ..
        } => Some(tool_result(
            call_id.clone(),
            match result {
                Ok(output) => output.content.clone(),
                Err(error) => format!("ERROR [{}]: {}", error.code, error.message),
            },
        )),
        JournalRecord::ToolCallRejected { call_id, error, .. } => Some(tool_result(
            call_id.clone(),
            format!("ERROR [{}]: {}", error.code, error.message),
        )),
        _ => None,
    }
}

fn divergence(call_id: &ToolCallId, message: impl Into<String>) -> JournalError {
    JournalError::Divergence {
        call_id: call_id.clone(),
        message: message.into(),
    }
}

fn tool_result(call_id: ToolCallId, output: String) -> ModelMessage {
    ModelMessage {
        role: ModelRole::Tool,
        content: vec![ModelContent::ToolResult { call_id, output }],
    }
}
