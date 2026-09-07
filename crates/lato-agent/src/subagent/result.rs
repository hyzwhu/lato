use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorerOutput {
    pub answer: String,
    pub evidence: Vec<ExplorerEvidence>,
    pub citations: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorerEvidence {
    pub id: String,
    pub summary: String,
    pub location: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOutput {
    pub summary: String,
    pub changed_files: Vec<PathBuf>,
    pub tests: Vec<WorkerTestResult>,
    pub artifacts: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerTestResult {
    pub command: String,
    pub passed: bool,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerOutput {
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub severity: ReviewSeverity,
    pub message: String,
    pub evidence: String,
    pub file: Option<PathBuf>,
    pub line: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Critical,
    Major,
    Minor,
}
