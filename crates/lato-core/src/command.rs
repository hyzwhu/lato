use crate::{
    CompactSession, CompactionId, ExtensionAuditRecord, ModelSelection, PlanCommand, PlanPhase,
    PluginSnapshotSummary, TurnId,
};

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    StartTurn(StartTurn),
    SteerTurn(UserInput),
    CancelTurn {
        turn_id: TurnId,
    },
    CompactSession(CompactSession),
    CancelCompaction {
        compaction_id: CompactionId,
    },
    SelectModel {
        selection: ModelSelection,
        model_family: Option<String>,
        context_window: Option<u64>,
    },
    AdoptPluginSnapshot {
        summary: PluginSnapshotSummary,
    },
    RecordExtensionAudit {
        audit: ExtensionAuditRecord,
    },
    RecordPlanModeEvent {
        event: PlanModeJournalEvent,
    },
    /// Phase 7C3: append one AgentField run event through the SessionLoop —
    /// the only writer allowed to allocate a journal sequence. The manager,
    /// host helpers, and HTTP futures must never append directly.
    RecordAgentFieldEvent {
        event: crate::journal::AgentFieldJournalEvent,
    },
    Shutdown,
}

/// Durable Plan-mode trace appended to the canonical session journal as
/// non-conversation events (spec §5). Keeps `Command` `Eq`-compatible.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanModeJournalEvent {
    Transitioned {
        activation: u64,
        from: PlanPhase,
        to: PlanPhase,
        command: PlanCommand,
    },
    ApprovalRecorded {
        activation: u64,
        generation: u64,
        content_hash: String,
        approver: String,
        approved_at_ms: u64,
    },
    ApprovalRevoked {
        activation: u64,
        generation: u64,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StartTurn {
    pub input: UserInput,
    pub behavior: StartBehavior,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartBehavior {
    Reject,
    Replace,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct UserInput {
    pub text: String,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}
