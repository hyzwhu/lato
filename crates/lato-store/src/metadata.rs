use crate::FileEventStore;
use fs2::FileExt;
use lato_core::{EventStore, JournalError, JournalRecord, SessionId};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const METADATA_SCHEMA_VERSION: u32 = 1;
const METADATA_FILE: &str = "metadata.json";
const METADATA_LOCK_FILE: &str = "metadata.json.lock";
const MAX_TITLE_CHARS: usize = 60;
const AUTOMATIC_TITLE_WORDS: usize = 10;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    Automatic,
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SessionMetadata {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub title: String,
    pub title_source: TitleSource,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub title: String,
    pub title_source: TitleSource,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl From<SessionMetadata> for SessionSummary {
    fn from(value: SessionMetadata) -> Self {
        Self {
            session_id: value.session_id,
            title: value.title,
            title_source: value.title_source,
            created_at_ms: value.created_at_ms,
            updated_at_ms: value.updated_at_ms,
        }
    }
}

pub fn derive_automatic_title(input: &str) -> String {
    let normalized = normalize_title(input);
    let words = normalized
        .split_whitespace()
        .take(AUTOMATIC_TITLE_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    let title = truncate_chars(&words, MAX_TITLE_CHARS);
    if title.is_empty() {
        "New session".into()
    } else {
        title
    }
}

pub fn normalize_manual_title(input: &str) -> Result<String, JournalError> {
    let title = truncate_chars(&normalize_title(input), MAX_TITLE_CHARS);
    if title.is_empty() {
        return Err(corrupt("session title must not be empty"));
    }
    Ok(title)
}

impl FileEventStore {
    pub async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, JournalError> {
        let mut summaries = Vec::new();
        for session_id in self.list_sessions().await? {
            let replay = self.replay(&session_id).await?;
            let (created_at_ms, updated_at_ms) = journal_times(&replay.envelopes);
            let metadata_path = metadata_path(self, &session_id)?;
            let summary = match read_metadata(&metadata_path, &session_id) {
                Ok(Some(metadata)) => SessionSummary::from(metadata),
                Ok(None) | Err(_) => {
                    let first_prompt = replay.envelopes.iter().find_map(|envelope| {
                        if let JournalRecord::TurnInputAccepted { input } = &envelope.record {
                            Some(input.text.as_str())
                        } else {
                            None
                        }
                    });
                    SessionSummary {
                        session_id: session_id.clone(),
                        title: derive_automatic_title(first_prompt.unwrap_or("")),
                        title_source: TitleSource::Automatic,
                        created_at_ms,
                        updated_at_ms,
                    }
                }
            };
            summaries.push(summary);
        }
        summaries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.session_id.cmp(&left.session_id))
        });
        Ok(summaries)
    }

    pub async fn ensure_automatic_title(
        &self,
        session_id: &SessionId,
        first_prompt: &str,
    ) -> Result<SessionSummary, JournalError> {
        let session_id = session_id.clone();
        let metadata_path = metadata_path(self, &session_id)?;
        let lock_path = metadata_path.with_file_name(METADATA_LOCK_FILE);
        let title = derive_automatic_title(first_prompt);
        let replay = self.replay(&session_id).await?;
        if !replay.exists {
            return Err(corrupt("cannot title an unknown session"));
        }
        let times = journal_times(&replay.envelopes);
        tokio::task::spawn_blocking(move || {
            update_metadata_locked(&metadata_path, &lock_path, &session_id, |current| {
                Ok(current.unwrap_or_else(|| SessionMetadata {
                    schema_version: METADATA_SCHEMA_VERSION,
                    session_id: session_id.clone(),
                    title,
                    title_source: TitleSource::Automatic,
                    created_at_ms: times.0,
                    updated_at_ms: times.1.max(now_ms()),
                }))
            })
            .map(SessionSummary::from)
        })
        .await
        .map_err(join_error)?
    }

    pub async fn rename_session(
        &self,
        session_id: &SessionId,
        title: &str,
    ) -> Result<SessionSummary, JournalError> {
        let title = normalize_manual_title(title)?;
        let session_id = session_id.clone();
        let metadata_path = metadata_path(self, &session_id)?;
        let lock_path = metadata_path.with_file_name(METADATA_LOCK_FILE);
        let replay = self.replay(&session_id).await?;
        if !replay.exists {
            return Err(corrupt("cannot rename an unknown session"));
        }
        let times = journal_times(&replay.envelopes);
        tokio::task::spawn_blocking(move || {
            update_metadata_locked(&metadata_path, &lock_path, &session_id, |current| {
                let mut metadata = current.unwrap_or_else(|| SessionMetadata {
                    schema_version: METADATA_SCHEMA_VERSION,
                    session_id: session_id.clone(),
                    title: String::new(),
                    title_source: TitleSource::Automatic,
                    created_at_ms: times.0,
                    updated_at_ms: times.1,
                });
                metadata.title = title;
                metadata.title_source = TitleSource::Manual;
                metadata.updated_at_ms = now_ms().max(metadata.updated_at_ms);
                Ok(metadata)
            })
            .map(SessionSummary::from)
        })
        .await
        .map_err(join_error)?
    }

    pub async fn delete_session(&self, session_id: &SessionId) -> Result<(), JournalError> {
        self.shutdown(session_id).await?;
        let session_dir = session_dir(self, session_id)?;
        tokio::task::spawn_blocking(move || delete_session_dir(&session_dir))
            .await
            .map_err(join_error)?
    }
}

fn normalize_title(input: &str) -> String {
    input
        .chars()
        .filter_map(|character| {
            if character.is_whitespace() {
                Some(' ')
            } else if character.is_control() {
                None
            } else {
                Some(character)
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'))
        .trim()
        .to_string()
}

fn truncate_chars(input: &str, limit: usize) -> String {
    input.chars().take(limit).collect()
}

fn journal_times(envelopes: &[lato_core::JournalEnvelope]) -> (u64, u64) {
    let created = envelopes.first().map_or(0, |item| item.timestamp_ms);
    let updated = envelopes.last().map_or(created, |item| item.timestamp_ms);
    (created, updated)
}

fn session_dir(store: &FileEventStore, session_id: &SessionId) -> Result<PathBuf, JournalError> {
    store
        .journal_path(session_id)?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| corrupt("session journal has no parent directory"))
}

fn metadata_path(store: &FileEventStore, session_id: &SessionId) -> Result<PathBuf, JournalError> {
    Ok(session_dir(store, session_id)?.join(METADATA_FILE))
}

fn update_metadata_locked(
    metadata_path: &Path,
    lock_path: &Path,
    session_id: &SessionId,
    update: impl FnOnce(Option<SessionMetadata>) -> Result<SessionMetadata, JournalError>,
) -> Result<SessionMetadata, JournalError> {
    let parent = metadata_path
        .parent()
        .ok_or_else(|| corrupt("metadata path has no parent directory"))?;
    validate_session_directory(parent)?;
    let lock = open_lock_file(lock_path)?;
    lock.lock_exclusive().map_err(io_error)?;
    let result = (|| {
        let current = read_metadata(metadata_path, session_id)?;
        let next = update(current)?;
        validate_metadata(&next, session_id)?;
        write_metadata_atomic(metadata_path, &next)?;
        Ok(next)
    })();
    let _ = lock.unlock();
    result
}

fn read_metadata(
    path: &Path,
    expected_session_id: &SessionId,
) -> Result<Option<SessionMetadata>, JournalError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(JournalError::UnsafeRestore {
            message: format!("metadata is not a regular file: {}", path.display()),
        });
    }
    let bytes = fs::read(path).map_err(io_error)?;
    let parsed: SessionMetadata = serde_json::from_slice(&bytes).map_err(|error| {
        corrupt(format!(
            "invalid session metadata at {}: {error}",
            path.display()
        ))
    })?;
    validate_metadata(&parsed, expected_session_id)?;
    Ok(Some(parsed))
}

fn validate_metadata(
    metadata: &SessionMetadata,
    expected_session_id: &SessionId,
) -> Result<(), JournalError> {
    if metadata.schema_version != METADATA_SCHEMA_VERSION {
        return Err(JournalError::SchemaUnsupported {
            expected: METADATA_SCHEMA_VERSION,
            actual: metadata.schema_version,
        });
    }
    if &metadata.session_id != expected_session_id {
        return Err(JournalError::SessionMismatch {
            expected: expected_session_id.clone(),
            actual: metadata.session_id.clone(),
        });
    }
    if metadata.title.is_empty() || metadata.title.chars().count() > MAX_TITLE_CHARS {
        return Err(corrupt("session metadata contains an invalid title"));
    }
    Ok(())
}

fn validate_session_directory(path: &Path) -> Result<(), JournalError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(JournalError::UnsafeRestore {
            message: format!("session path is not a directory: {}", path.display()),
        });
    }
    Ok(())
}

fn open_lock_file(path: &Path) -> Result<File, JournalError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(JournalError::UnsafeRestore {
            message: format!("metadata lock is not a regular file: {}", path.display()),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(io_error)
}

fn write_metadata_atomic(path: &Path, metadata: &SessionMetadata) -> Result<(), JournalError> {
    let bytes = serde_json::to_vec_pretty(metadata).map_err(|error| corrupt(error.to_string()))?;
    let parent = path
        .parent()
        .ok_or_else(|| corrupt("metadata path has no parent directory"))?;
    let temp = parent.join(format!(
        "metadata.json.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(io_error)?;
    let result = (|| {
        file.write_all(&bytes).map_err(io_error)?;
        file.write_all(b"\n").map_err(io_error)?;
        file.flush().map_err(io_error)?;
        file.sync_data().map_err(io_error)?;
        replace_file(&temp, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), JournalError> {
    fs::rename(source, destination).map_err(io_error)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), JournalError> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(destination).map_err(io_error)?;
            fs::rename(source, destination).map_err(io_error)
        }
        Err(error) => Err(io_error(error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), JournalError> {
    File::open(path)
        .map_err(io_error)?
        .sync_all()
        .map_err(io_error)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), JournalError> {
    Ok(())
}

fn delete_session_dir(path: &Path) -> Result<(), JournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(JournalError::UnsafeRestore {
                message: format!("session path is not a directory: {}", path.display()),
            })
        }
        Ok(_) => fs::remove_dir_all(path).map_err(io_error),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn io_error(error: std::io::Error) -> JournalError {
    JournalError::Io {
        message: error.to_string(),
    }
}

fn join_error(error: tokio::task::JoinError) -> JournalError {
    JournalError::Io {
        message: error.to_string(),
    }
}

fn corrupt(message: impl Into<String>) -> JournalError {
    JournalError::Corrupt {
        message: message.into(),
    }
}
