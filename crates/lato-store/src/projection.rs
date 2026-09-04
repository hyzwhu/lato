// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs
// License: Apache-2.0
// Lato changes: reduced chat persistence to a canonical-journal-derived history projection

use crate::{MAX_JOURNAL_BYTES, MAX_JOURNAL_RECORDS};
use lato_core::{
    HISTORY_PROJECTION_SCHEMA_VERSION, HistoryCheckpoint, HistoryProjectionEntry,
    HistoryProjectionMetadata, JournalEnvelope, JournalError, JournalRecord, ModelMessage,
    ProjectionError, SessionId, checkpoint_digest, history_digest, projection_message,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub(crate) struct ProjectionPaths {
    pub history: PathBuf,
    pub metadata: PathBuf,
    pub corrupt_history: PathBuf,
    pub corrupt_metadata: PathBuf,
    pub checkpoints: PathBuf,
}

impl ProjectionPaths {
    pub(crate) fn new(session_dir: &Path) -> Self {
        Self {
            history: session_dir.join("history.jsonl"),
            metadata: session_dir.join("history.meta.json"),
            corrupt_history: session_dir.join("history.jsonl.corrupt"),
            corrupt_metadata: session_dir.join("history.meta.json.corrupt"),
            checkpoints: session_dir.join("compaction_checkpoints"),
        }
    }
}

pub(crate) fn load_or_rebuild(
    session_id: &SessionId,
    session_dir: &Path,
    envelopes: &[JournalEnvelope],
    canonical_messages: &[ModelMessage],
) -> Result<Vec<ModelMessage>, JournalError> {
    if envelopes.is_empty() {
        return Ok(Vec::new());
    }
    let paths = ProjectionPaths::new(session_dir);
    let expected = current_messages(&paths, envelopes, canonical_messages)?;
    let active_checkpoint = active_checkpoint_id(envelopes);
    match load_valid(session_id, &paths, envelopes, active_checkpoint.as_deref()) {
        Ok(messages) => Ok(messages),
        Err(LoadError::MissingOrStale) => {
            replace_cache(
                session_id,
                &paths,
                envelopes,
                &expected,
                0,
                active_checkpoint,
            )?;
            Ok(expected)
        }
        Err(LoadError::Damaged(error)) => {
            quarantine(&paths).map_err(JournalError::Projection)?;
            replace_cache(
                session_id,
                &paths,
                envelopes,
                &expected,
                0,
                active_checkpoint,
            )?;
            let _ = error;
            Ok(expected)
        }
    }
}

pub(crate) fn apply_committed_envelope(
    session_id: &SessionId,
    journal_path: &Path,
    envelope: &JournalEnvelope,
) -> Result<(), ProjectionError> {
    let session_dir = journal_path
        .parent()
        .ok_or_else(|| ProjectionError::WriteFailed {
            message: "journal has no session directory".into(),
        })?;
    let paths = ProjectionPaths::new(session_dir);
    let mut entries = if paths.history.exists() {
        read_entries(&paths.history)?
    } else if envelope.journal_sequence == 0 {
        Vec::new()
    } else {
        return Err(ProjectionError::Divergent {
            message: "history projection is missing before append".into(),
        });
    };
    if let Some(message) = projection_message(&envelope.record) {
        entries.push(HistoryProjectionEntry::new(
            envelope.journal_sequence,
            envelope.record_id.clone(),
            message,
        )?);
    }
    let messages: Vec<_> = entries.iter().map(|entry| entry.message.clone()).collect();
    write_projection(
        session_id,
        &paths,
        &entries,
        envelope.journal_sequence,
        envelope.record_id.clone(),
        history_digest(&messages)?,
        0,
        None,
    )
}

fn load_valid(
    session_id: &SessionId,
    paths: &ProjectionPaths,
    envelopes: &[JournalEnvelope],
    active_checkpoint_id: Option<&str>,
) -> Result<Vec<ModelMessage>, LoadError> {
    if !paths.history.exists() && !paths.metadata.exists() {
        return Err(LoadError::MissingOrStale);
    }
    if !paths.history.exists() || !paths.metadata.exists() {
        return Err(LoadError::Damaged(ProjectionError::Corrupt {
            message: "history and metadata must exist together".into(),
        }));
    }
    let metadata = read_metadata(&paths.metadata).map_err(LoadError::Damaged)?;
    if metadata.schema_version != HISTORY_PROJECTION_SCHEMA_VERSION
        || &metadata.session_id != session_id
    {
        return Err(LoadError::Damaged(ProjectionError::Corrupt {
            message: "history metadata schema or session mismatch".into(),
        }));
    }
    let last = envelopes.last().expect("non-empty envelopes");
    if metadata.last_journal_sequence != last.journal_sequence
        || metadata.last_record_id != last.record_id
        || metadata.active_checkpoint_id.as_deref() != active_checkpoint_id
    {
        return Err(LoadError::MissingOrStale);
    }
    let entries = read_entries(&paths.history).map_err(LoadError::Damaged)?;
    if metadata.entry_count != entries.len() as u64 {
        return Err(LoadError::Damaged(ProjectionError::Corrupt {
            message: "history entry count mismatch".into(),
        }));
    }
    let byte_length = fs::metadata(&paths.history)
        .map_err(write_error)
        .map_err(LoadError::Damaged)?
        .len();
    if byte_length != metadata.byte_length {
        return Err(LoadError::Damaged(ProjectionError::Corrupt {
            message: "history byte length mismatch".into(),
        }));
    }
    let messages: Vec<_> = entries.into_iter().map(|entry| entry.message).collect();
    if history_digest(&messages).map_err(LoadError::Damaged)? != metadata.history_digest {
        return Err(LoadError::Damaged(ProjectionError::Corrupt {
            message: "history digest mismatch".into(),
        }));
    }
    Ok(messages)
}

enum LoadError {
    MissingOrStale,
    Damaged(ProjectionError),
}

fn replace_cache(
    session_id: &SessionId,
    paths: &ProjectionPaths,
    envelopes: &[JournalEnvelope],
    messages: &[ModelMessage],
    generation: u64,
    active_checkpoint_id: Option<String>,
) -> Result<(), JournalError> {
    let entries = entries_for_current(envelopes, messages)?;
    let last = envelopes.last().ok_or_else(|| {
        JournalError::Projection(ProjectionError::Divergent {
            message: "cannot materialize an empty journal".into(),
        })
    })?;
    write_projection(
        session_id,
        paths,
        &entries,
        last.journal_sequence,
        last.record_id.clone(),
        history_digest(messages)?,
        generation,
        active_checkpoint_id,
    )?;
    Ok(())
}

fn entries_from_envelopes(
    envelopes: &[JournalEnvelope],
) -> Result<Vec<HistoryProjectionEntry>, ProjectionError> {
    envelopes
        .iter()
        .filter_map(|envelope| {
            projection_message(&envelope.record).map(|message| {
                HistoryProjectionEntry::new(
                    envelope.journal_sequence,
                    envelope.record_id.clone(),
                    message,
                )
            })
        })
        .collect()
}

fn entries_for_current(
    envelopes: &[JournalEnvelope],
    messages: &[ModelMessage],
) -> Result<Vec<HistoryProjectionEntry>, ProjectionError> {
    let Some((marker_index, marker)) = envelopes.iter().enumerate().rev().find(|(_, envelope)| {
        matches!(
            envelope.record,
            JournalRecord::HistoryProjectionReplaced { .. }
        )
    }) else {
        return entries_from_envelopes(envelopes);
    };
    let mut entries = Vec::new();
    let checkpoint_count = match &marker.record {
        JournalRecord::HistoryProjectionReplaced {
            replacement_entry_count,
            ..
        } => *replacement_entry_count as usize,
        _ => unreachable!(),
    };
    for message in messages.iter().take(checkpoint_count) {
        entries.push(HistoryProjectionEntry::new(
            marker.journal_sequence,
            marker.record_id.clone(),
            message.clone(),
        )?);
    }
    for envelope in &envelopes[marker_index + 1..] {
        if let Some(message) = projection_message(&envelope.record) {
            entries.push(HistoryProjectionEntry::new(
                envelope.journal_sequence,
                envelope.record_id.clone(),
                message,
            )?);
        }
    }
    Ok(entries)
}

fn active_checkpoint_id(envelopes: &[JournalEnvelope]) -> Option<String> {
    envelopes
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.record {
            JournalRecord::HistoryProjectionReplaced { checkpoint_id, .. } => {
                Some(checkpoint_id.clone())
            }
            _ => None,
        })
}

fn current_messages(
    paths: &ProjectionPaths,
    envelopes: &[JournalEnvelope],
    canonical: &[ModelMessage],
) -> Result<Vec<ModelMessage>, JournalError> {
    let Some((index, marker)) = envelopes.iter().enumerate().rev().find(|(_, envelope)| {
        matches!(
            envelope.record,
            JournalRecord::HistoryProjectionReplaced { .. }
        )
    }) else {
        return Ok(canonical.to_vec());
    };
    let (checkpoint_id, expected_digest, replacement_count, expected_history_digest) =
        match &marker.record {
            JournalRecord::HistoryProjectionReplaced {
                checkpoint_id,
                checkpoint_digest,
                replacement_entry_count,
                history_digest,
                ..
            } => (
                checkpoint_id,
                checkpoint_digest,
                *replacement_entry_count,
                history_digest,
            ),
            _ => unreachable!(),
        };
    let path = paths.checkpoints.join(format!("{checkpoint_id}.json"));
    if !path.exists() {
        return Err(ProjectionError::CheckpointMissing {
            checkpoint_id: checkpoint_id.clone(),
        }
        .into());
    }
    let checkpoint: HistoryCheckpoint =
        serde_json::from_slice(&secure_read_bounded(&path, MAX_JOURNAL_BYTES)?).map_err(
            |error| ProjectionError::CheckpointMismatch {
                message: error.to_string(),
            },
        )?;
    let actual = checkpoint_digest(
        &checkpoint.session_id,
        checkpoint.replaced_through_sequence,
        &checkpoint.replaced_through_record_id,
        checkpoint.prior_checkpoint_id.as_deref(),
        &checkpoint.messages,
    )?;
    if checkpoint.checkpoint_id != *checkpoint_id
        || checkpoint.content_digest != *expected_digest
        || actual != *expected_digest
        || checkpoint.messages.len() as u64 != replacement_count
        || history_digest(&checkpoint.messages)? != *expected_history_digest
    {
        return Err(ProjectionError::CheckpointMismatch {
            message: format!("checkpoint {checkpoint_id} does not match its journal marker"),
        }
        .into());
    }
    let mut messages = checkpoint.messages;
    for envelope in &envelopes[index + 1..] {
        if let Some(message) = projection_message(&envelope.record) {
            messages.push(message);
        }
    }
    Ok(messages)
}

pub(crate) fn publish_checkpoint(
    paths: &ProjectionPaths,
    checkpoint: &HistoryCheckpoint,
) -> Result<(), ProjectionError> {
    fs::create_dir_all(&paths.checkpoints).map_err(write_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&paths.checkpoints, fs::Permissions::from_mode(0o700))
            .map_err(write_error)?;
    }
    let bytes =
        serde_json::to_vec_pretty(checkpoint).map_err(|error| ProjectionError::WriteFailed {
            message: error.to_string(),
        })?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(ProjectionError::LimitExceeded {
            message: "checkpoint exceeds configured bounds".into(),
        });
    }
    let path = paths
        .checkpoints
        .join(format!("{}.json", checkpoint.checkpoint_id));
    atomic_write(&path, &bytes)?;
    sync_directory(&paths.checkpoints)
}

pub(crate) fn publish_replacement(
    session_id: &SessionId,
    paths: &ProjectionPaths,
    envelopes: &[JournalEnvelope],
    messages: &[ModelMessage],
    generation: u64,
    checkpoint_id: String,
) -> Result<HistoryProjectionMetadata, ProjectionError> {
    let entries = entries_for_current(envelopes, messages)?;
    let last = envelopes.last().ok_or_else(|| ProjectionError::Divergent {
        message: "replacement journal is empty".into(),
    })?;
    write_projection(
        session_id,
        paths,
        &entries,
        last.journal_sequence,
        last.record_id.clone(),
        history_digest(messages)?,
        generation,
        Some(checkpoint_id),
    )?;
    read_metadata(&paths.metadata)
}

#[allow(clippy::too_many_arguments)]
fn write_projection(
    session_id: &SessionId,
    paths: &ProjectionPaths,
    entries: &[HistoryProjectionEntry],
    last_sequence: u64,
    last_record_id: lato_core::JournalRecordId,
    digest: String,
    generation: u64,
    active_checkpoint_id: Option<String>,
) -> Result<(), ProjectionError> {
    let history_bytes = encode_jsonl(entries)?;
    if history_bytes.len() as u64 > MAX_JOURNAL_BYTES || entries.len() > MAX_JOURNAL_RECORDS {
        return Err(ProjectionError::LimitExceeded {
            message: "history projection exceeds configured bounds".into(),
        });
    }
    let mut metadata = HistoryProjectionMetadata::new(
        session_id.clone(),
        last_sequence,
        last_record_id,
        entries.len() as u64,
        history_bytes.len() as u64,
        digest,
        active_checkpoint_id,
    );
    metadata.generation = generation;
    let metadata_bytes =
        serde_json::to_vec_pretty(&metadata).map_err(|error| ProjectionError::WriteFailed {
            message: error.to_string(),
        })?;
    atomic_write(&paths.history, &history_bytes)?;
    atomic_write(&paths.metadata, &metadata_bytes)?;
    sync_directory(paths.history.parent().expect("history has parent"))?;
    Ok(())
}

fn read_entries(path: &Path) -> Result<Vec<HistoryProjectionEntry>, ProjectionError> {
    let bytes = secure_read_bounded(path, MAX_JOURNAL_BYTES)?;
    let mut entries = Vec::new();
    for (index, line) in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if entries.len() >= MAX_JOURNAL_RECORDS {
            return Err(ProjectionError::LimitExceeded {
                message: "history record limit exceeded".into(),
            });
        }
        let entry: HistoryProjectionEntry =
            serde_json::from_slice(line).map_err(|error| ProjectionError::Corrupt {
                message: format!("history line {}: {error}", index + 1),
            })?;
        if entry.schema_version != HISTORY_PROJECTION_SCHEMA_VERSION {
            return Err(ProjectionError::Corrupt {
                message: "unsupported history schema".into(),
            });
        }
        entry.validate_hash()?;
        entries.push(entry);
    }
    Ok(entries)
}

fn read_metadata(path: &Path) -> Result<HistoryProjectionMetadata, ProjectionError> {
    serde_json::from_slice(&secure_read_bounded(path, 1024 * 1024)?).map_err(|error| {
        ProjectionError::Corrupt {
            message: error.to_string(),
        }
    })
}

fn quarantine(paths: &ProjectionPaths) -> Result<(), ProjectionError> {
    quarantine_one(&paths.history, &paths.corrupt_history)?;
    quarantine_one(&paths.metadata, &paths.corrupt_metadata)
}

fn quarantine_one(source: &Path, destination: &Path) -> Result<(), ProjectionError> {
    if !source.exists() {
        return Ok(());
    }
    if destination.exists() {
        fs::remove_file(source).map_err(|error| ProjectionError::QuarantineFailed {
            message: error.to_string(),
        })?;
        return Ok(());
    }
    fs::rename(source, destination).map_err(|error| ProjectionError::QuarantineFailed {
        message: error.to_string(),
    })
}

fn encode_jsonl<T: serde::Serialize>(items: &[T]) -> Result<Vec<u8>, ProjectionError> {
    let mut bytes = Vec::new();
    for item in items {
        serde_json::to_writer(&mut bytes, item).map_err(|error| ProjectionError::WriteFailed {
            message: error.to_string(),
        })?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn secure_read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, ProjectionError> {
    let metadata = fs::symlink_metadata(path).map_err(write_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ProjectionError::Corrupt {
            message: format!("projection is not a regular file: {}", path.display()),
        });
    }
    if metadata.len() > limit {
        return Err(ProjectionError::LimitExceeded {
            message: format!("projection exceeds {limit} bytes"),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(write_error)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(write_error)?;
    if bytes.len() as u64 > limit {
        return Err(ProjectionError::LimitExceeded {
            message: format!("projection exceeds {limit} bytes"),
        });
    }
    Ok(bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProjectionError> {
    let parent = path.parent().ok_or_else(|| ProjectionError::WriteFailed {
        message: "projection path has no parent".into(),
    })?;
    fs::create_dir_all(parent).map_err(write_error)?;
    let temp = parent.join(format!(
        ".history.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temp).map_err(write_error)?;
    file.write_all(bytes)
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_data())
        .map_err(write_error)?;
    fs::rename(&temp, path).map_err(write_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(write_error)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ProjectionError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(write_error)
}
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ProjectionError> {
    Ok(())
}

fn write_error(error: std::io::Error) -> ProjectionError {
    ProjectionError::WriteFailed {
        message: error.to_string(),
    }
}
