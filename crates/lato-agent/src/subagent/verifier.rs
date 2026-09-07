use super::{ExplorerOutput, ReviewerOutput, WorkerOutput};
use lato_core::{TaskError, TaskErrorCode};
use lato_runtime::{TaskVerifier, VerificationOutcome, VerificationRequest};
use std::{collections::HashSet, path::Path};

#[derive(Default)]
pub struct ProfileResultVerifier;

#[async_trait::async_trait]
impl TaskVerifier for ProfileResultVerifier {
    async fn verify(&self, request: VerificationRequest) -> VerificationOutcome {
        let result = match request.node.profile.name.as_str() {
            "explorer" => verify_explorer(&request),
            "worker" => verify_worker(&request),
            "reviewer" => verify_reviewer(&request),
            _ => Err("only built-in task profiles can be verified".into()),
        };
        match result {
            Ok(()) => VerificationOutcome::Passed,
            Err(message) => VerificationOutcome::Failed(TaskError::new(
                TaskErrorCode::VerificationFailed,
                message,
            )),
        }
    }
}

fn verify_explorer(request: &VerificationRequest) -> Result<(), String> {
    let output: ExplorerOutput = serde_json::from_str(&request.result.output)
        .map_err(|error| format!("explorer output does not match its schema: {error}"))?;
    if output.answer.trim().is_empty() {
        return Err("explorer answer must not be empty".into());
    }
    let evidence: HashSet<&str> = output
        .evidence
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    if output.evidence.iter().any(|item| {
        item.id.trim().is_empty()
            || item.summary.trim().is_empty()
            || item.location.trim().is_empty()
    }) {
        return Err("explorer evidence fields must not be empty".into());
    }
    if let Some(citation) = output
        .citations
        .iter()
        .find(|citation| !evidence.contains(citation.as_str()))
    {
        return Err(format!(
            "explorer citation `{citation}` does not resolve to produced evidence"
        ));
    }
    Ok(())
}

fn verify_worker(request: &VerificationRequest) -> Result<(), String> {
    let output: WorkerOutput = serde_json::from_str(&request.result.output)
        .map_err(|error| format!("worker output does not match its schema: {error}"))?;
    if output.summary.trim().is_empty() {
        return Err("worker summary must not be empty".into());
    }
    for path in output.changed_files.iter().chain(&output.artifacts) {
        validate_relative_workspace_path(path, "worker output")?;
    }
    if output
        .tests
        .iter()
        .any(|test| test.command.trim().is_empty() || test.summary.trim().is_empty())
    {
        return Err("worker test command and summary must not be empty".into());
    }
    Ok(())
}

fn verify_reviewer(request: &VerificationRequest) -> Result<(), String> {
    let output: ReviewerOutput = serde_json::from_str(&request.result.output)
        .map_err(|error| format!("reviewer output does not match its schema: {error}"))?;
    if output.summary.trim().is_empty() {
        return Err("reviewer summary must not be empty".into());
    }
    for finding in output.findings {
        if finding.message.trim().is_empty() || finding.evidence.trim().is_empty() {
            return Err("reviewer finding message and evidence must not be empty".into());
        }
        if finding.line == Some(0) {
            return Err("reviewer finding line must be one-based".into());
        }
        if finding.line.is_some() && finding.file.is_none() {
            return Err("reviewer finding with a line must name a file".into());
        }
        if let Some(file) = finding.file {
            validate_relative_workspace_path(&file, "reviewer finding")?;
        }
    }
    Ok(())
}

fn validate_relative_workspace_path(path: &Path, label: &str) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "{label} path `{}` must stay relative to the leased workspace",
            path.display()
        ));
    }
    Ok(())
}
