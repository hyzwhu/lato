use futures_util::future::BoxFuture;
use lato_core::{TaskError, TaskErrorCode, TaskId, TaskNode, TaskResult, VerificationPolicy};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationRequest {
    pub node: TaskNode,
    pub result: TaskResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationOutcome {
    Passed,
    Failed(TaskError),
    WaitingForChild { reviewer_task_id: TaskId },
    WaitingForApproval { approval_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationDecision {
    Passed,
    Failed(TaskError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationResume {
    Child {
        reviewer_task_id: TaskId,
        decision: VerificationDecision,
    },
    Approval {
        approval_id: String,
        decision: VerificationDecision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationWait {
    Child(TaskId),
    Approval(String),
}

#[async_trait::async_trait]
pub trait TaskVerifier: Send + Sync + 'static {
    async fn verify(&self, request: VerificationRequest) -> VerificationOutcome;
}

#[derive(Default)]
pub struct AcceptVerifier;

#[async_trait::async_trait]
impl TaskVerifier for AcceptVerifier {
    async fn verify(&self, _request: VerificationRequest) -> VerificationOutcome {
        VerificationOutcome::Passed
    }
}

type ProgrammaticCallback = Arc<
    dyn Fn(VerificationRequest) -> BoxFuture<'static, VerificationOutcome> + Send + Sync + 'static,
>;

pub struct PolicyTaskVerifier {
    programmatic: ProgrammaticCallback,
}

impl Default for PolicyTaskVerifier {
    fn default() -> Self {
        Self::new(|_| Box::pin(async { VerificationOutcome::Passed }))
    }
}

impl PolicyTaskVerifier {
    pub fn new(
        programmatic: impl Fn(VerificationRequest) -> BoxFuture<'static, VerificationOutcome>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            programmatic: Arc::new(programmatic),
        }
    }
}

#[async_trait::async_trait]
impl TaskVerifier for PolicyTaskVerifier {
    async fn verify(&self, request: VerificationRequest) -> VerificationOutcome {
        match request.node.profile.verification {
            VerificationPolicy::Accept => VerificationOutcome::Passed,
            VerificationPolicy::Schema => verify_schema(&request),
            VerificationPolicy::Programmatic => (self.programmatic)(request).await,
            VerificationPolicy::IndependentReview => VerificationOutcome::WaitingForChild {
                reviewer_task_id: TaskId::from(format!("{}:reviewer", request.node.id)),
            },
            VerificationPolicy::HumanGate => VerificationOutcome::WaitingForApproval {
                approval_id: format!("{}:approval", request.node.id),
            },
        }
    }
}

fn verify_schema(request: &VerificationRequest) -> VerificationOutcome {
    let Some(schema) = request.node.result_contract.schema.as_ref() else {
        return failed("schema verification requires a result schema");
    };
    let schema = match supported_object(schema) {
        Ok(schema) => schema,
        Err(error) => return VerificationOutcome::Failed(error),
    };
    let output: Value = match serde_json::from_str(&request.result.output) {
        Ok(output) => output,
        Err(_) => return failed("task output is not valid JSON"),
    };
    let Some(output) = output.as_object() else {
        return failed("task output must be a JSON object");
    };
    if let Some(required) = schema.get("required") {
        let Some(required) = required.as_array() else {
            return unsupported("required must be an array of strings");
        };
        for property in required {
            let Some(property) = property.as_str() else {
                return unsupported("required must be an array of strings");
            };
            if !output.contains_key(property) {
                return failed(format!("required property `{property}` is missing"));
            }
        }
    }
    if let Some(properties) = schema.get("properties") {
        let Some(properties) = properties.as_object() else {
            return unsupported("properties must be an object");
        };
        for (name, property_schema) in properties {
            let property_schema = match supported_property(property_schema) {
                Ok(value) => value,
                Err(error) => return VerificationOutcome::Failed(error),
            };
            let Some(value) = output.get(name) else {
                continue;
            };
            let expected = property_schema["type"]
                .as_str()
                .expect("supported property types are strings");
            let matches = match expected {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                "boolean" => value.is_boolean(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                _ => false,
            };
            if !matches {
                return failed(format!("property `{name}` must have type `{expected}`"));
            }
        }
    }
    VerificationOutcome::Passed
}

fn supported_object(schema: &Value) -> Result<&serde_json::Map<String, Value>, TaskError> {
    let Some(schema) = schema.as_object() else {
        return Err(unsupported_error("schema must be an object"));
    };
    if let Some(keyword) = schema
        .keys()
        .find(|keyword| !matches!(keyword.as_str(), "type" | "required" | "properties"))
    {
        return Err(unsupported_error(format!(
            "unsupported top-level schema keyword `{keyword}`"
        )));
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(unsupported_error("top-level schema type must be `object`"));
    }
    Ok(schema)
}

fn supported_property(schema: &Value) -> Result<&serde_json::Map<String, Value>, TaskError> {
    let Some(schema) = schema.as_object() else {
        return Err(unsupported_error("property schema must be an object"));
    };
    if schema.len() != 1 || !schema.contains_key("type") {
        return Err(unsupported_error(
            "property schemas support only the `type` keyword",
        ));
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string" | "number" | "integer" | "boolean" | "object" | "array") => Ok(schema),
        _ => Err(unsupported_error("unsupported property type")),
    }
}

fn failed(message: impl Into<String>) -> VerificationOutcome {
    VerificationOutcome::Failed(TaskError::new(TaskErrorCode::VerificationFailed, message))
}

fn unsupported(message: impl Into<String>) -> VerificationOutcome {
    VerificationOutcome::Failed(unsupported_error(message))
}

fn unsupported_error(message: impl Into<String>) -> TaskError {
    TaskError::new(TaskErrorCode::VerificationSchemaUnsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{
        AgentProfile, ResultContract, SessionId, TaskOwner, TaskScope, TaskStatus, TurnId,
        WorkspaceIntent,
    };

    fn request(schema: Value, output: &str) -> VerificationRequest {
        let mut profile = AgentProfile::explorer();
        profile.workspace = WorkspaceIntent::SharedReadOnly;
        VerificationRequest {
            node: TaskNode {
                id: TaskId::from("schema"),
                parent_id: Some(TaskId::from("root")),
                root_id: TaskId::from("root"),
                owner: TaskOwner::Interactive {
                    session_id: SessionId::from("session"),
                    turn_id: TurnId::from("turn"),
                },
                profile,
                scope: TaskScope {
                    objective: String::new(),
                    context_refs: Vec::new(),
                },
                status: TaskStatus::Verifying,
                permissions: Vec::new(),
                workspace_intent: WorkspaceIntent::SharedReadOnly,
                result_contract: ResultContract {
                    schema: Some(schema),
                    max_output_bytes: 1024,
                },
            },
            result: TaskResult {
                success: true,
                output: output.into(),
                error: None,
                usage: Default::default(),
                duration_ms: 0,
                output_ref: None,
            },
        }
    }

    #[tokio::test]
    async fn validates_the_explicit_schema_subset() {
        let verifier = PolicyTaskVerifier::default();
        let request = request(
            serde_json::json!({
                "type": "object",
                "required": ["answer"],
                "properties": {"answer": {"type": "integer"}}
            }),
            r#"{"answer":42}"#,
        );
        assert_eq!(verifier.verify(request).await, VerificationOutcome::Passed);
    }

    #[tokio::test]
    async fn rejects_unsupported_keywords_explicitly() {
        let verifier = PolicyTaskVerifier::default();
        let outcome = verifier
            .verify(request(
                serde_json::json!({"type": "object", "additionalProperties": false}),
                "{}",
            ))
            .await;
        let VerificationOutcome::Failed(error) = outcome else {
            panic!("unsupported schema must fail");
        };
        assert_eq!(error.code, TaskErrorCode::VerificationSchemaUnsupported);
    }

    #[tokio::test]
    async fn accept_and_programmatic_verifiers_are_explicit_seams() {
        let schema = serde_json::json!({"type": "object"});
        let mut accept_request = request(schema.clone(), "{}");
        accept_request.node.profile.verification = VerificationPolicy::Accept;
        assert_eq!(
            AcceptVerifier.verify(accept_request.clone()).await,
            VerificationOutcome::Passed
        );

        accept_request.node.profile.verification = VerificationPolicy::Programmatic;
        let verifier = PolicyTaskVerifier::new(|_| {
            Box::pin(async {
                VerificationOutcome::Failed(TaskError::new(
                    TaskErrorCode::VerificationFailed,
                    "injected invariant failed",
                ))
            })
        });
        assert!(matches!(
            verifier.verify(accept_request).await,
            VerificationOutcome::Failed(_)
        ));
    }
}
