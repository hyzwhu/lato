// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/workflow/manager.rs
// License: Apache-2.0
// Lato changes: Phase 7B5 per-run persistence layout (`run.json` + `script.rhai`
// + `journal.jsonl`) under the session workflows directory; restore scan skips
// malformed runs instead of failing the session.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::tracker::{WORKFLOW_HISTORY_MAX, WorkflowRunStatus};

pub const RUN_RECORD_VERSION: u32 = 1;
pub const MAX_WORKFLOW_SOURCE_BYTES: u64 = 1024 * 1024;
pub const RUN_RECORD_FILE: &str = "run.json";
pub const SCRIPT_FILE: &str = "script.rhai";
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// Immutable per-run snapshot written to `<workflows_dir>/<runId>/run.json`.
/// Field names align with the ACP `lato/session/workflow/runs` snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedRun {
    pub version: u32,
    pub run_id: String,
    pub display_name: String,
    pub status: WorkflowRunStatus,
    pub phase: Option<String>,
    pub agent_budget: Option<u64>,
    pub agents_used: u64,
    pub pause_message: Option<String>,
    pub elapsed_ms_floor: u64,
    pub workflow_id: String,
    pub source: String,
    pub compiled: bool,
    pub description: String,
    pub args: serde_json::Value,
}

/// A run directory that passed the `run.json` gate during a restore scan.
pub struct RestoredRun {
    pub record: PersistedRun,
    pub dir: PathBuf,
}

pub fn run_dir(workflows_dir: &Path, run_id: &str) -> PathBuf {
    workflows_dir.join(run_id)
}

/// Write `run.json` (create the directory, replace atomically via rename).
pub fn write_run_record(dir: &Path, record: &PersistedRun) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let body = serde_json::to_string_pretty(record)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let tmp = dir.join(format!("{RUN_RECORD_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, dir.join(RUN_RECORD_FILE))
}

/// Write the captured script once at launch; resume never rewrites it.
pub fn write_script(dir: &Path, script: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{SCRIPT_FILE}.tmp"));
    std::fs::write(&tmp, script)?;
    std::fs::rename(&tmp, dir.join(SCRIPT_FILE))
}

/// Read `run.json`; `None` on any damage or a foreign `version`.
pub fn read_run_record(dir: &Path) -> Option<PersistedRun> {
    let body = std::fs::read(dir.join(RUN_RECORD_FILE)).ok()?;
    let record: PersistedRun = serde_json::from_slice(&body).ok()?;
    (record.version == RUN_RECORD_VERSION).then_some(record)
}

/// Read the captured script; `None` when missing, symlinked, not a regular
/// file, oversized, or not UTF-8.
pub fn read_script(dir: &Path) -> Option<String> {
    let path = dir.join(SCRIPT_FILE);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return None;
    }
    let script = std::fs::read_to_string(&path).ok()?;
    (script.len() as u64 <= MAX_WORKFLOW_SOURCE_BYTES).then_some(script)
}

/// Scan one level of `<workflows_dir>` for restorable runs, most recent last,
/// keeping at most [`WORKFLOW_HISTORY_MAX`] entries. Directories without a
/// parseable current-version `run.json` are skipped.
pub fn scan_restore_candidates(workflows_dir: &Path) -> Vec<RestoredRun> {
    let Ok(read_dir) = std::fs::read_dir(workflows_dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<(std::time::SystemTime, RestoredRun)> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|dir| {
            let record = read_run_record(&dir)?;
            let modified = std::fs::metadata(&dir)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((modified, RestoredRun { record, dir }))
        })
        .collect();
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.record.run_id.cmp(&right.1.record.run_id))
    });
    let skip = candidates.len().saturating_sub(WORKFLOW_HISTORY_MAX);
    candidates
        .into_iter()
        .skip(skip)
        .map(|(_, run)| run)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tracker::WorkflowRunStatus;

    fn sample(run_id: &str, display_name: &str, status: WorkflowRunStatus) -> PersistedRun {
        PersistedRun {
            version: RUN_RECORD_VERSION,
            run_id: run_id.to_owned(),
            display_name: display_name.to_owned(),
            status,
            phase: Some("Review".into()),
            agent_budget: Some(128),
            agents_used: 3,
            pause_message: Some("need human".into()),
            elapsed_ms_floor: 1200,
            workflow_id: "review-changes".into(),
            source: "user".into(),
            compiled: false,
            description: "d".into(),
            args: serde_json::json!({"path": "src"}),
        }
    }

    #[test]
    fn run_record_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("wf_1");
        let record = sample("wf_1", "review", WorkflowRunStatus::UserPaused);
        write_run_record(&run_dir, &record).unwrap();
        assert_eq!(read_run_record(&run_dir), Some(record));
    }

    #[test]
    fn malformed_or_foreign_version_run_records_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("wf_broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(RUN_RECORD_FILE), "{not json").unwrap();
        assert!(read_run_record(&broken).is_none());

        let foreign = dir.path().join("wf_foreign");
        let mut record = sample("wf_foreign", "foreign", WorkflowRunStatus::Blocked);
        record.version = 99;
        write_run_record(&foreign, &record).unwrap();
        assert!(read_run_record(&foreign).is_none());

        let missing = dir.path().join("wf_missing");
        std::fs::create_dir_all(&missing).unwrap();
        assert!(read_run_record(&missing).is_none());
    }

    #[test]
    fn script_round_trips_and_oversize_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("wf_1");
        write_script(&run_dir, "let meta = #{ name: \"w\" };").unwrap();
        assert_eq!(
            read_script(&run_dir).as_deref(),
            Some("let meta = #{ name: \"w\" };")
        );

        let big = dir.path().join("wf_big");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(
            big.join(SCRIPT_FILE),
            "x".repeat(MAX_WORKFLOW_SOURCE_BYTES as usize + 1),
        )
        .unwrap();
        assert!(read_script(&big).is_none());
    }

    #[test]
    fn scan_keeps_only_the_most_recent_runs_and_skips_damage() {
        let dir = tempfile::tempdir().unwrap();
        for seq in 0..(WORKFLOW_HISTORY_MAX + 4) {
            let run_id = format!("wf_{seq:06}");
            let run_dir = dir.path().join(&run_id);
            write_run_record(
                &run_dir,
                &sample(&run_id, &run_id, WorkflowRunStatus::Complete),
            )
            .unwrap();
        }
        let broken = dir.path().join("wf_broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join(RUN_RECORD_FILE), "garbage").unwrap();

        let scanned = scan_restore_candidates(dir.path());
        assert_eq!(scanned.len(), WORKFLOW_HISTORY_MAX);
        assert_eq!(scanned[0].record.run_id, "wf_000004");
        assert_eq!(
            scanned.last().unwrap().record.run_id,
            format!("wf_{:06}", WORKFLOW_HISTORY_MAX + 3)
        );
        assert!(scanned.iter().all(|run| run.record.run_id != "wf_broken"));
    }
}
