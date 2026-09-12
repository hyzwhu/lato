//! Workflow naming: `plugin/workflow` qualification.

pub const MAX_WORKFLOW_NAME_LEN: usize = 64;

/// Normalize a configured workflow name to `[a-z0-9][a-z0-9_-]*`.
pub fn normalize_workflow_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_WORKFLOW_NAME_LEN {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let mut chars = lower.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphanumeric() {
        return None;
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
        return None;
    }
    Some(lower)
}

pub fn qualify_workflow(plugin: &str, workflow: &str) -> String {
    format!("{plugin}/{workflow}")
}

/// Map a JSON integer to the declared agent-budget cap.
///
/// `None` applies [`crate::DEFAULT_AGENT_BUDGET`]. Out-of-range values error
/// so the caller can drop the entry.
pub fn clamp_agent_budget(raw: Option<u64>) -> Result<u32, crate::WorkflowError> {
    let Some(value) = raw else {
        return Ok(crate::DEFAULT_AGENT_BUDGET);
    };
    if value < u64::from(crate::MIN_AGENT_BUDGET) || value > u64::from(crate::MAX_AGENT_BUDGET) {
        return Err(crate::WorkflowError::InvalidConfiguration(format!(
            "agent_budget {value} is outside {min}..={max}",
            min = crate::MIN_AGENT_BUDGET,
            max = crate::MAX_AGENT_BUDGET
        )));
    }
    Ok(value as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_with_slash() {
        assert_eq!(qualify_workflow("demo", "review"), "demo/review");
    }

    #[test]
    fn normalizes_and_rejects_invalid_names() {
        assert_eq!(
            normalize_workflow_name("Review-Changes").as_deref(),
            Some("review-changes")
        );
        assert_eq!(normalize_workflow_name("a").as_deref(), Some("a"));
        assert!(normalize_workflow_name("").is_none());
        assert!(normalize_workflow_name("-leading").is_none());
        assert!(normalize_workflow_name("has space").is_none());
        assert!(normalize_workflow_name("UPPER/slash").is_none());
    }

    #[test]
    fn clamps_agent_budget() {
        assert_eq!(
            clamp_agent_budget(None).unwrap(),
            crate::DEFAULT_AGENT_BUDGET
        );
        assert_eq!(clamp_agent_budget(Some(32)).unwrap(), 32);
        assert_eq!(clamp_agent_budget(Some(1)).unwrap(), 1);
        assert_eq!(clamp_agent_budget(Some(1024)).unwrap(), 1024);
        assert!(clamp_agent_budget(Some(0)).is_err());
        assert!(clamp_agent_budget(Some(1025)).is_err());
    }
}
