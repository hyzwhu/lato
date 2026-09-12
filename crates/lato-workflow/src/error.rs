//! Stable workflow error codes.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("workflow execution is not implemented")]
    NotImplemented,
    #[error("workflow configuration is invalid: {0}")]
    InvalidConfiguration(String),
    #[error("workflow capability ceiling denies {0}")]
    CapabilityDenied(String),
    #[error("workflow {0} was not found")]
    NotFound(String),
    #[error("workflow budget exceeded: {0}")]
    BudgetExceeded(String),
    #[error("workflow run was cancelled")]
    Cancelled,
    #[error("workflow failed: {0}")]
    Failed(String),
}

impl WorkflowError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotImplemented => "workflow.not_implemented",
            Self::InvalidConfiguration(_) => "workflow.invalid_configuration",
            Self::CapabilityDenied(_) => "workflow.capability_denied",
            Self::NotFound(_) => "workflow.not_found",
            Self::BudgetExceeded(_) => "workflow.budget_exceeded",
            Self::Cancelled => "workflow.cancelled",
            Self::Failed(_) => "workflow.failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inert_code_is_stable() {
        assert_eq!(
            WorkflowError::NotImplemented.code(),
            "workflow.not_implemented"
        );
        assert_eq!(
            WorkflowError::CapabilityDenied("x".into()).code(),
            "workflow.capability_denied"
        );
    }
}
