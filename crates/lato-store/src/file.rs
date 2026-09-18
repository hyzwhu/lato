// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-workflow/src/journal.rs
// License: Apache-2.0
// Lato changes: generalized bounded JSONL replay to canonical per-session event journals

use crate::{MAX_JOURNAL_BYTES, MAX_JOURNAL_RECORDS, projection, writer::WriterHandle};
use async_trait::async_trait;
use fs2::FileExt;
use lato_core::{
    EventStore, HistoryProjectionMetadata, HistoryProjectionStore, HistoryReplacementReason,
    JournalDurability, JournalEnvelope, JournalError, JournalReplay, ModelMessage, ProjectionError,
    SessionId, decode_journal_envelope, project_journal,
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::Mutex;

static NEXT_IMPORT_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultPoint {
    BeforeWrite,
    AfterWrite,
    BeforeFlush,
    AfterFlush,
    BeforeSyncData,
    AfterSyncData,
    BeforeRename,
    AfterRename,
    BeforeCheckpointPublish,
    BeforeHistoryPublish,
    BeforeMetadataPublish,
}

#[async_trait]
impl HistoryProjectionStore for FileEventStore {
    async fn replace_history(
        &self,
        session_id: &SessionId,
        messages: Vec<ModelMessage>,
        reason: HistoryReplacementReason,
    ) -> Result<HistoryProjectionMetadata, ProjectionError> {
        self.writer(session_id)
            .await
            .map_err(|error| ProjectionError::WriteFailed {
                message: error.to_string(),
            })?
            .replace_history(messages, reason)
            .await
    }
}

#[doc(hidden)]
pub trait FileFaultInjector: Send + Sync {
    fn check(&self, point: FaultPoint) -> Result<(), JournalError>;
}

struct NoFaults;

impl FileFaultInjector for NoFaults {
    fn check(&self, _point: FaultPoint) -> Result<(), JournalError> {
        Ok(())
    }
}

pub struct FileEventStore {
    sessions_dir: PathBuf,
    writers: Mutex<BTreeMap<SessionId, WriterHandle>>,
    faults: Arc<dyn FileFaultInjector>,
}

impl FileEventStore {
    pub fn open(lato_home: &Path) -> Result<Self, JournalError> {
        Self::open_with_fault_injector(lato_home, Arc::new(NoFaults))
    }

    #[doc(hidden)]
    pub fn open_with_fault_injector(
        lato_home: &Path,
        faults: Arc<dyn FileFaultInjector>,
    ) -> Result<Self, JournalError> {
        let sessions_dir = lato_home.join("sessions");
        ensure_secure_directory(&sessions_dir)?;
        Ok(Self {
            sessions_dir,
            writers: Mutex::new(BTreeMap::new()),
            faults,
        })
    }

    pub fn journal_path(&self, session_id: &SessionId) -> Result<PathBuf, JournalError> {
        validate_session_id(session_id)?;
        Ok(self
            .sessions_dir
            .join(session_id.as_str())
            .join("events.jsonl"))
    }

    pub fn history_path(&self, session_id: &SessionId) -> Result<PathBuf, JournalError> {
        Ok(self
            .journal_path(session_id)?
            .with_file_name("history.jsonl"))
    }

    pub fn history_metadata_path(&self, session_id: &SessionId) -> Result<PathBuf, JournalError> {
        Ok(self
            .journal_path(session_id)?
            .with_file_name("history.meta.json"))
    }

    async fn writer(&self, session_id: &SessionId) -> Result<WriterHandle, JournalError> {
        let mut writers = self.writers.lock().await;
        if let Some(writer) = writers.get(session_id) {
            return Ok(writer.clone());
        }
        let writer = WriterHandle::spawn(
            session_id.clone(),
            self.journal_path(session_id)?,
            self.faults.clone(),
        );
        writers.insert(session_id.clone(), writer.clone());
        Ok(writer)
    }
}

#[async_trait]
impl EventStore for FileEventStore {
    async fn append(
        &self,
        envelope: JournalEnvelope,
        durability: JournalDurability,
    ) -> Result<(), JournalError> {
        self.writer(&envelope.session_id)
            .await?
            .append(envelope, durability)
            .await
    }

    async fn replay(&self, session_id: &SessionId) -> Result<JournalReplay, JournalError> {
        self.writer(session_id).await?.replay().await
    }

    async fn import_if_absent(
        &self,
        session_id: &SessionId,
        envelopes: Vec<JournalEnvelope>,
    ) -> Result<JournalReplay, JournalError> {
        let projection = project_journal(session_id, &envelopes)?;
        let bytes = encoded_len(&envelopes)?;
        ensure_capacity(envelopes.len(), bytes, envelopes.len() as u64)?;
        let imported_session_id = session_id.clone();
        let path = self.journal_path(&imported_session_id)?;
        let faults = self.faults.clone();
        let imported = tokio::task::spawn_blocking(move || {
            if path.exists() {
                return replay_path_blocking(&imported_session_id, &path);
            }
            let parent = path.parent().ok_or_else(|| JournalError::MigrationFailed {
                message: "journal path has no parent".into(),
            })?;
            ensure_secure_directory(parent)?;
            let temp = parent.join(format!(
                "events.jsonl.{}.{}.{}.tmp",
                std::process::id(),
                now_nanos(),
                // Wall-clock timestamps can coincide across concurrent imports.
                NEXT_IMPORT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            write_import_file(&temp, &envelopes)?;
            if path.exists() {
                let _ = fs::remove_file(&temp);
                return replay_path_blocking(&imported_session_id, &path);
            }
            faults.check(FaultPoint::BeforeRename)?;
            match fs::hard_link(&temp, &path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&temp);
                    return replay_path_blocking(&imported_session_id, &path);
                }
                Err(error) => {
                    return Err(JournalError::MigrationFailed {
                        message: error.to_string(),
                    });
                }
            }
            faults.check(FaultPoint::AfterRename)?;
            fs::remove_file(&temp).map_err(io_error)?;
            sync_directory(parent)?;
            let replay = replay_path_blocking(&imported_session_id, &path)?;
            if replay.projection != projection {
                return Err(JournalError::MigrationFailed {
                    message: "imported projection changed after rename".into(),
                });
            }
            Ok(replay)
        })
        .await
        .map_err(|error| JournalError::MigrationFailed {
            message: error.to_string(),
        })??;
        if imported.exists {
            self.replay(session_id).await
        } else {
            Ok(imported)
        }
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, JournalError> {
        let sessions_dir = self.sessions_dir.clone();
        tokio::task::spawn_blocking(move || {
            let mut sessions = Vec::new();
            for entry in fs::read_dir(sessions_dir).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                if !entry.file_type().map_err(io_error)?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(session_id) = SessionId::parse(name) else {
                    continue;
                };
                if fs::symlink_metadata(entry.path().join("events.jsonl"))
                    .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                {
                    sessions.push(session_id);
                }
            }
            sessions.sort();
            Ok(sessions)
        })
        .await
        .map_err(|error| JournalError::Io {
            message: error.to_string(),
        })?
    }

    async fn shutdown(&self, session_id: &SessionId) -> Result<(), JournalError> {
        let writer = self.writers.lock().await.remove(session_id);
        match writer {
            Some(writer) => writer.shutdown().await,
            None => Ok(()),
        }
    }
}

pub(crate) fn append_envelope_blocking(
    session_id: &SessionId,
    path: &Path,
    envelope: &JournalEnvelope,
    durability: JournalDurability,
    faults: &dyn FileFaultInjector,
) -> Result<(), JournalError> {
    // Cross-process critical section: sequence validation and the append must
    // be indivisible, otherwise two independent store instances (or processes)
    // can both validate against the same tail and write duplicate sequences.
    // The lock lives on a dedicated sidecar file so the journal inode itself
    // is never locked (mandatory byte-range locks on some platforms would
    // otherwise block the re-read performed inside the section).
    let parent = path.parent().ok_or_else(|| JournalError::Io {
        message: "journal path has no parent".into(),
    })?;
    ensure_secure_directory(parent)?;
    let lock = open_lock_file(path)?;
    lock.lock_exclusive().map_err(io_error)?;
    let result = append_envelope_locked(session_id, path, envelope, durability, faults);
    let _ = lock.unlock();
    result
}

fn append_envelope_locked(
    session_id: &SessionId,
    path: &Path,
    envelope: &JournalEnvelope,
    durability: JournalDurability,
    faults: &dyn FileFaultInjector,
) -> Result<(), JournalError> {
    let replay = replay_path_blocking(session_id, path)?;
    let mut candidate = replay.envelopes;
    candidate.push(envelope.clone());
    project_journal(session_id, &candidate)?;
    let mut line = serde_json::to_vec(envelope).map_err(|error| JournalError::Io {
        message: error.to_string(),
    })?;
    line.push(b'\n');
    let existing_bytes = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    ensure_capacity(
        candidate.len(),
        existing_bytes + line.len() as u64,
        envelope.journal_sequence,
    )?;
    let parent = path.parent().ok_or_else(|| JournalError::Io {
        message: "journal path has no parent".into(),
    })?;
    ensure_secure_directory(parent)?;
    let new_file = !path.exists();
    let mut file = secure_append(path)?;
    faults.check(FaultPoint::BeforeWrite)?;
    file.write_all(&line).map_err(io_error)?;
    faults.check(FaultPoint::AfterWrite)?;
    faults.check(FaultPoint::BeforeFlush)?;
    file.flush().map_err(io_error)?;
    faults.check(FaultPoint::AfterFlush)?;
    if durability == JournalDurability::SyncData {
        faults.check(FaultPoint::BeforeSyncData)?;
        file.sync_data().map_err(io_error)?;
        faults.check(FaultPoint::AfterSyncData)?;
        if new_file {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

/// Sidecar lock file guarding the validate-then-append critical section of
/// `events.jsonl` across independent store instances and processes.
fn journal_lock_path(journal_path: &Path) -> PathBuf {
    let mut name = journal_path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    journal_path.with_file_name(name)
}

fn open_lock_file(journal_path: &Path) -> Result<File, JournalError> {
    let path = journal_lock_path(journal_path);
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(&path).map_err(io_error)
}

pub(crate) fn satisfy_existing_durability(
    path: &Path,
    durability: JournalDurability,
) -> Result<(), JournalError> {
    let mut file = secure_append(path)?;
    file.flush().map_err(io_error)?;
    if durability == JournalDurability::SyncData {
        file.sync_data().map_err(io_error)?;
    }
    Ok(())
}

pub(crate) fn replay_path_blocking(
    session_id: &SessionId,
    path: &Path,
) -> Result<JournalReplay, JournalError> {
    validate_session_id(session_id)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalReplay::empty(session_id.clone()));
        }
        Err(error) => return Err(io_error(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(JournalError::UnsafeRestore {
            message: format!("journal is not a regular file: {}", path.display()),
        });
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(JournalError::UnsafeRestore {
            message: format!("journal exceeds {MAX_JOURNAL_BYTES} bytes"),
        });
    }
    let mut file = secure_read(path)?;
    let opened = file.metadata().map_err(io_error)?;
    if !opened.is_file() || opened.len() > MAX_JOURNAL_BYTES || !same_file(&metadata, &opened) {
        return Err(JournalError::UnsafeRestore {
            message: "journal changed during open".into(),
        });
    }
    let mut content = Vec::with_capacity(opened.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(io_error)?;
    if content.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(JournalError::UnsafeRestore {
            message: format!("journal exceeds {MAX_JOURNAL_BYTES} bytes"),
        });
    }
    let envelopes = decode_and_repair(path, &content)?;
    let projection = project_journal(session_id, &envelopes)?;
    Ok(JournalReplay {
        exists: true,
        envelopes,
        projection,
    })
}

pub(crate) fn replay_with_projection_blocking(
    session_id: &SessionId,
    path: &Path,
) -> Result<JournalReplay, JournalError> {
    let mut replay = replay_path_blocking(session_id, path)?;
    if replay.exists {
        let parent = path.parent().ok_or_else(|| JournalError::Io {
            message: "journal path has no parent".into(),
        })?;
        replay.projection.messages = projection::load_or_rebuild(
            session_id,
            parent,
            &replay.envelopes,
            &replay.projection.messages,
        )?;
    }
    Ok(replay)
}

fn decode_and_repair(path: &Path, content: &[u8]) -> Result<Vec<JournalEnvelope>, JournalError> {
    let mut envelopes = Vec::new();
    let mut offset = 0;
    let mut line_number = 0;
    while offset < content.len() {
        line_number += 1;
        let Some(relative_newline) = content[offset..].iter().position(|byte| *byte == b'\n')
        else {
            let tail = &content[offset..];
            if tail.iter().all(u8::is_ascii_whitespace) {
                truncate_and_sync(path, offset as u64)?;
                break;
            }
            match decode_journal_envelope(tail) {
                Ok(envelope) => {
                    ensure_record_count(envelopes.len() + 1, envelope.journal_sequence)?;
                    envelopes.push(envelope);
                    terminate_and_sync(path)?;
                }
                // Only a genuinely unparseable unterminated tail is
                // repaired (frozen WIN-26 behavior). Reader-version gate
                // failures (future/unsupported schema) fail closed WITHOUT
                // truncation — data of an unreadable version is preserved.
                Err(JournalError::Parse { .. }) => truncate_and_sync(path, offset as u64)?,
                Err(error) => return Err(error),
            }
            break;
        };
        let end = offset + relative_newline;
        let line = &content[offset..end];
        offset = end + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let envelope: JournalEnvelope =
            decode_journal_envelope(line).map_err(|error| match error {
                JournalError::Parse { message, .. } => JournalError::Parse {
                    line: line_number,
                    message,
                },
                other => other,
            })?;
        ensure_record_count(envelopes.len() + 1, envelope.journal_sequence)?;
        envelopes.push(envelope);
    }
    Ok(envelopes)
}

fn write_import_file(path: &Path, envelopes: &[JournalEnvelope]) -> Result<(), JournalError> {
    let mut file = secure_create_new(path)?;
    for envelope in envelopes {
        serde_json::to_writer(&mut file, envelope).map_err(|error| {
            JournalError::MigrationFailed {
                message: error.to_string(),
            }
        })?;
        file.write_all(b"\n").map_err(io_error)?;
    }
    file.flush().map_err(io_error)?;
    file.sync_data().map_err(io_error)
}

fn encoded_len(envelopes: &[JournalEnvelope]) -> Result<u64, JournalError> {
    envelopes.iter().try_fold(0_u64, |total, envelope| {
        let len = serde_json::to_vec(envelope)
            .map_err(|error| JournalError::MigrationFailed {
                message: error.to_string(),
            })?
            .len() as u64
            + 1;
        Ok(total.saturating_add(len))
    })
}

fn ensure_capacity(records: usize, bytes: u64, sequence: u64) -> Result<(), JournalError> {
    ensure_record_count(records, sequence)?;
    if bytes > MAX_JOURNAL_BYTES {
        return Err(JournalError::Full {
            sequence,
            limit: MAX_JOURNAL_BYTES,
        });
    }
    Ok(())
}

fn ensure_record_count(records: usize, sequence: u64) -> Result<(), JournalError> {
    if records > MAX_JOURNAL_RECORDS {
        return Err(JournalError::Full {
            sequence,
            limit: MAX_JOURNAL_RECORDS as u64,
        });
    }
    Ok(())
}

fn validate_session_id(session_id: &SessionId) -> Result<(), JournalError> {
    let value = session_id.as_str();
    if matches!(value, "." | "..")
        || value.starts_with('.')
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(JournalError::UnsafeRestore {
            message: "session id is unsafe for journal paths".into(),
        });
    }
    Ok(())
}

fn secure_read(path: &Path) -> Result<File, JournalError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(io_error)
}

fn secure_append(path: &Path) -> Result<File, JournalError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(io_error)?;
    set_file_permissions(path)?;
    Ok(file)
}

fn secure_create_new(path: &Path) -> Result<File, JournalError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(io_error)
}

fn truncate_and_sync(path: &Path, len: u64) -> Result<(), JournalError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io_error)?;
    file.set_len(len).map_err(io_error)?;
    file.sync_data().map_err(io_error)
}

fn terminate_and_sync(path: &Path) -> Result<(), JournalError> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(b"\n").map_err(io_error)?;
    file.sync_data().map_err(io_error)
}

#[cfg(unix)]
fn set_dir_permissions(path: &Path) -> Result<(), JournalError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &Path) -> Result<(), JournalError> {
    Ok(())
}

fn ensure_secure_directory(path: &Path) -> Result<(), JournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(JournalError::UnsafeRestore {
                message: format!("journal directory is unsafe: {}", path.display()),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(io_error)?;
        }
        Err(error) => return Err(io_error(error)),
    }
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(JournalError::UnsafeRestore {
            message: format!(
                "journal directory changed during creation: {}",
                path.display()
            ),
        });
    }
    set_dir_permissions(path)
}

#[cfg(unix)]
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev() && before.ino() == after.ino()
}

#[cfg(not(unix))]
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.len() == after.len()
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<(), JournalError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_error)
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<(), JournalError> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), JournalError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), JournalError> {
    Ok(())
}

fn io_error(error: std::io::Error) -> JournalError {
    JournalError::Io {
        message: error.to_string(),
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
