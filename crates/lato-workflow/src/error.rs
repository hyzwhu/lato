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
}

impl WorkflowError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotImplemented => "workflow.not_implemented",
            Self::InvalidConfiguration(_) => "workflow.invalid_configuration",
            Self::CapabilityDenied(_) => "workflow.capability_denied",
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
