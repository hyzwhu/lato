// Phase 7C1: strict wire types for the pinned AgentField `v0.1.138` contract.
//
// Every decoder follows the fixture's field classification
// (`docs/superpowers/fixtures/agentfield-v0.1.138-contract.json`):
// - required consumed fields must be present with the right type;
// - required ignored-but-type-checked fields must be present with the right
//   type even though Lato never reads their value;
// - optional ignored-but-type-checked fields are type-checked only when
//   present;
// - unknown fields are ignored.
// Any violation fails closed with `agentfield.remote_protocol`.

use serde_json::Value;

pub const PINNED_AGENTFIELD_VERSION: &str = "v0.1.138";

/// Remote execution status strings understood by the pinned version.
/// Unknown strings are a protocol violation, never silently mapped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteExecutionStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl RemoteExecutionStatus {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

fn field<'a>(envelope: &'a Value, name: &str) -> Result<&'a Value, String> {
    envelope
        .get(name)
        .ok_or_else(|| format!("missing required field `{name}`"))
}

fn as_string(envelope: &Value, name: &str) -> Result<String, String> {
    match field(envelope, name)? {
        Value::String(text) => Ok(text.clone()),
        other => Err(format!(
            "field `{name}` must be a string, got {}",
            type_name(other)
        )),
    }
}

fn as_bool(envelope: &Value, name: &str) -> Result<(), String> {
    match field(envelope, name)? {
        Value::Bool(_) => Ok(()),
        other => Err(format!(
            "field `{name}` must be a boolean, got {}",
            type_name(other)
        )),
    }
}

fn optional_string(envelope: &Value, name: &str) -> Result<Option<String>, String> {
    match envelope.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(format!(
            "field `{name}` must be a string when present, got {}",
            type_name(other)
        )),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `POST /api/v1/execute/async/{target}` 202 envelope (pinned v0.1.138).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsyncStartEnvelope {
    /// Required consumed: the remote identity Lato binds to the local run.
    pub execution_id: String,
    /// Required consumed.
    pub status: RemoteExecutionStatus,
    /// Required consumed: must equal the derived execute target Lato called.
    pub target: String,
    /// Required consumed: pinned version only accepts `"reasoner"`.
    pub kind: String,
    /// Required ignored-but-type-checked.
    pub run_id: String,
    /// Required ignored-but-type-checked.
    pub workflow_id: String,
    /// Required ignored-but-type-checked.
    pub created_at: String,
    /// Required ignored-but-type-checked.
    pub enqueued_at: String,
    /// Required ignored-but-type-checked.
    pub webhook_registered: bool,
    /// Optional ignored-but-type-checked.
    pub webhook_error: Option<String>,
}

impl AsyncStartEnvelope {
    pub fn decode(envelope: &Value) -> Result<Self, String> {
        let decoded = Self {
            execution_id: as_string(envelope, "execution_id")?,
            status: RemoteExecutionStatus::parse(&as_string(envelope, "status")?)
                .ok_or_else(|| "unknown status enum".to_string())?,
            target: as_string(envelope, "target")?,
            kind: as_string(envelope, "type")?,
            run_id: as_string(envelope, "run_id")?,
            workflow_id: as_string(envelope, "workflow_id")?,
            created_at: as_string(envelope, "created_at")?,
            enqueued_at: as_string(envelope, "enqueued_at")?,
            webhook_registered: {
                as_bool(envelope, "webhook_registered")?;
                true
            },
            webhook_error: optional_string(envelope, "webhook_error")?,
        };
        // Pinned invariants: validated, never consumed by product logic.
        if decoded.workflow_id != decoded.run_id {
            return Err("invariant workflow_id == run_id violated".to_string());
        }
        if decoded.enqueued_at != decoded.created_at {
            return Err("invariant enqueued_at == created_at violated".to_string());
        }
        Ok(decoded)
    }
}

/// `GET /api/v1/executions/{execution_id}` success envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusEnvelope {
    /// Required consumed.
    pub execution_id: String,
    /// Required consumed.
    pub status: RemoteExecutionStatus,
    /// Required ignored-but-type-checked.
    pub run_id: String,
    /// Required ignored-but-type-checked.
    pub started_at: String,
    /// Required ignored-but-type-checked.
    pub webhook_registered: bool,
    /// Optional ignored-but-type-checked (each string-typed when present;
    /// `duration_ms` is a number).
    pub optional_ignored: StatusOptionalFields,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusOptionalFields {
    pub agent_node_id: Option<String>,
    pub instance_id: Option<String>,
    pub status_reason: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub error_details: Option<String>,
    pub completed_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub webhook_events: Option<Value>,
    pub approval_request_id: Option<String>,
    pub approval_status: Option<String>,
    pub approval_request_url: Option<String>,
}

impl StatusEnvelope {
    pub fn decode(envelope: &Value) -> Result<Self, String> {
        let optional_ignored = StatusOptionalFields {
            agent_node_id: optional_string(envelope, "agent_node_id")?,
            instance_id: optional_string(envelope, "instance_id")?,
            status_reason: optional_string(envelope, "status_reason")?,
            result: optional_string(envelope, "result")?,
            error: optional_string(envelope, "error")?,
            error_details: optional_string(envelope, "error_details")?,
            completed_at: optional_string(envelope, "completed_at")?,
            duration_ms: match envelope.get("duration_ms") {
                None | Some(Value::Null) => None,
                Some(value @ Value::Number(_)) => Some(value.as_u64().ok_or_else(|| {
                    "field `duration_ms` must be a non-negative number".to_string()
                })?),
                Some(other) => {
                    return Err(format!(
                        "field `duration_ms` must be a number when present, got {}",
                        type_name(other)
                    ));
                }
            },
            webhook_events: match envelope.get("webhook_events") {
                None | Some(Value::Null) => None,
                Some(Value::Array(_)) => Some(envelope["webhook_events"].clone()),
                Some(other) => {
                    return Err(format!(
                        "field `webhook_events` must be an array when present, got {}",
                        type_name(other)
                    ));
                }
            },
            approval_request_id: optional_string(envelope, "approval_request_id")?,
            approval_status: optional_string(envelope, "approval_status")?,
            approval_request_url: optional_string(envelope, "approval_request_url")?,
        };
        Ok(Self {
            execution_id: as_string(envelope, "execution_id")?,
            status: RemoteExecutionStatus::parse(&as_string(envelope, "status")?)
                .ok_or_else(|| "unknown status enum".to_string())?,
            run_id: as_string(envelope, "run_id")?,
            started_at: as_string(envelope, "started_at")?,
            webhook_registered: {
                as_bool(envelope, "webhook_registered")?;
                true
            },
            optional_ignored,
        })
    }
}

/// `POST /api/v1/executions/{execution_id}/cancel` success envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelSuccessEnvelope {
    /// Required consumed.
    pub execution_id: String,
    /// Required consumed.
    pub status: RemoteExecutionStatus,
    /// Required ignored-but-type-checked.
    pub previous_status: String,
    /// Required ignored-but-type-checked.
    pub cancelled_at: String,
    /// Optional ignored-but-type-checked.
    pub reason: Option<String>,
}

impl CancelSuccessEnvelope {
    pub fn decode(envelope: &Value) -> Result<Self, String> {
        Ok(Self {
            execution_id: as_string(envelope, "execution_id")?,
            status: RemoteExecutionStatus::parse(&as_string(envelope, "status")?)
                .ok_or_else(|| "unknown status enum".to_string())?,
            previous_status: as_string(envelope, "previous_status")?,
            cancelled_at: as_string(envelope, "cancelled_at")?,
            reason: optional_string(envelope, "reason")?,
        })
    }
}

/// Cancel 409 conflict: only `error == "invalid_state"` is a stable contract;
/// the dynamic `message` is type-checked but never frozen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelConflictEnvelope {
    pub error: String,
    pub message: String,
}

impl CancelConflictEnvelope {
    pub fn decode(envelope: &Value) -> Result<Self, String> {
        let decoded = Self {
            error: as_string(envelope, "error")?,
            message: as_string(envelope, "message")?,
        };
        if decoded.error != "invalid_state" {
            return Err(format!(
                "unexpected cancel conflict error `{}`",
                decoded.error
            ));
        }
        Ok(decoded)
    }
}

/// One discovery reasoner entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryReasoner {
    /// Required consumed: strict ASCII atom.
    pub id: String,
    /// Required consumed: must equal `agent_id + ':' + id`.
    pub invocation_target: String,
    /// Optional ignored-but-type-checked.
    pub description: Option<String>,
    /// Optional ignored-but-type-checked.
    pub tags: Option<Value>,
    /// Optional ignored-but-type-checked.
    pub input_schema: Option<Value>,
    /// Optional ignored-but-type-checked.
    pub output_schema: Option<Value>,
    /// Optional ignored-but-type-checked.
    pub examples: Option<Value>,
}

/// One discovery agent entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryAgent {
    /// Required consumed: strict ASCII atom.
    pub agent_id: String,
    /// Required consumed: pinned version string; mismatch is a protocol error.
    pub version: String,
    /// Required consumed: `healthy` is required for target derivation.
    pub health_status: String,
    /// Required consumed.
    pub reasoners: Vec<DiscoveryReasoner>,
    /// Required ignored-but-type-checked.
    pub group_id: String,
    /// Required ignored-but-type-checked.
    pub base_url: String,
    /// Required ignored-but-type-checked.
    pub deployment_type: String,
    /// Required ignored-but-type-checked.
    pub last_heartbeat: String,
    /// Required ignored-but-type-checked.
    pub skills: Value,
}

/// `GET /api/v1/discovery/capabilities` success envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryEnvelope {
    /// Required ignored-but-type-checked.
    pub discovered_at: String,
    /// Required ignored-but-type-checked.
    pub total_agents: u64,
    /// Required ignored-but-type-checked.
    pub total_reasoners: u64,
    /// Required ignored-but-type-checked.
    pub total_skills: u64,
    /// Required consumed.
    pub capabilities: Vec<DiscoveryAgent>,
}

impl DiscoveryEnvelope {
    pub fn decode(envelope: &Value) -> Result<Self, String> {
        for name in [
            "discovered_at",
            "total_agents",
            "total_reasoners",
            "total_skills",
        ] {
            field(envelope, name)?;
        }
        if !matches!(envelope.get("total_agents"), Some(Value::Number(_))) {
            return Err("field `total_agents` must be a number".to_string());
        }
        if !matches!(envelope.get("total_reasoners"), Some(Value::Number(_))) {
            return Err("field `total_reasoners` must be a number".to_string());
        }
        if !matches!(envelope.get("total_skills"), Some(Value::Number(_))) {
            return Err("field `total_skills` must be a number".to_string());
        }
        if !matches!(envelope.get("discovered_at"), Some(Value::String(_))) {
            return Err("field `discovered_at` must be a string".to_string());
        }
        let pagination = field(envelope, "pagination")?;
        for name in ["limit", "offset", "has_more"] {
            field(pagination, name)?;
        }
        if !matches!(pagination.get("has_more"), Some(Value::Bool(_))) {
            return Err("field `pagination.has_more` must be a boolean".to_string());
        }
        for name in ["limit", "offset"] {
            if !matches!(pagination.get(name), Some(Value::Number(_))) {
                return Err(format!("field `pagination.{name}` must be a number"));
            }
        }
        let mut capabilities = Vec::new();
        for agent in field(envelope, "capabilities")?
            .as_array()
            .ok_or_else(|| "field `capabilities` must be an array".to_string())?
        {
            capabilities.push(Self::decode_agent(agent)?);
        }
        Self::enforce_target_contract(&capabilities)?;
        Ok(Self {
            discovered_at: as_string(envelope, "discovered_at")?,
            total_agents: envelope["total_agents"].as_u64().unwrap_or_default(),
            total_reasoners: envelope["total_reasoners"].as_u64().unwrap_or_default(),
            total_skills: envelope["total_skills"].as_u64().unwrap_or_default(),
            capabilities,
        })
    }

    /// Target contract (spec §6.2, AC-13): every `(agent_id, reasoner.id,
    /// invocation_target)` triple must survive the colon→dot derivation, and
    /// no derived execute target may repeat. Runs inside `decode`, so an
    /// illegal atom or an inconsistent colon target never yields a decoded
    /// envelope.
    fn enforce_target_contract(capabilities: &[DiscoveryAgent]) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for agent in capabilities {
            for reasoner in &agent.reasoners {
                let execute_target = crate::agentfield::config::derive_execute_target(
                    &agent.agent_id,
                    &reasoner.id,
                    &reasoner.invocation_target,
                )?;
                if !seen.insert(execute_target.clone()) {
                    return Err(format!(
                        "duplicate discovery execute target `{execute_target}`"
                    ));
                }
            }
        }
        Ok(())
    }

    fn decode_agent(agent: &Value) -> Result<DiscoveryAgent, String> {
        for name in [
            "group_id",
            "base_url",
            "version",
            "health_status",
            "deployment_type",
            "last_heartbeat",
        ] {
            as_string(agent, name)?;
        }
        if !matches!(agent.get("skills"), Some(Value::Array(_))) {
            return Err("field `skills` must be an array".to_string());
        }
        let mut reasoners = Vec::new();
        for reasoner in field(agent, "reasoners")?
            .as_array()
            .ok_or_else(|| "field `reasoners` must be an array".to_string())?
        {
            reasoners.push(DiscoveryReasoner {
                id: as_string(reasoner, "id")?,
                invocation_target: as_string(reasoner, "invocation_target")?,
                description: optional_string(reasoner, "description")?,
                tags: match reasoner.get("tags") {
                    None | Some(Value::Null) => None,
                    Some(Value::Array(_)) => Some(reasoner["tags"].clone()),
                    Some(other) => {
                        return Err(format!(
                            "field `tags` must be an array when present, got {}",
                            type_name(other)
                        ));
                    }
                },
                input_schema: optional_value(reasoner, "input_schema")?,
                output_schema: optional_value(reasoner, "output_schema")?,
                examples: optional_value(reasoner, "examples")?,
            });
        }
        Ok(DiscoveryAgent {
            agent_id: as_string(agent, "agent_id")?,
            version: as_string(agent, "version")?,
            health_status: as_string(agent, "health_status")?,
            reasoners,
            group_id: as_string(agent, "group_id")?,
            base_url: as_string(agent, "base_url")?,
            deployment_type: as_string(agent, "deployment_type")?,
            last_heartbeat: as_string(agent, "last_heartbeat")?,
            skills: agent["skills"].clone(),
        })
    }
}

fn optional_value(envelope: &Value, name: &str) -> Result<Option<Value>, String> {
    match envelope.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(_)) => Ok(Some(envelope[name].clone())),
        Some(Value::Array(_)) => Ok(Some(envelope[name].clone())),
        Some(other) => Err(format!(
            "field `{name}` must be an object or array when present, got {}",
            type_name(other)
        )),
    }
}
