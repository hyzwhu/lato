use crate::HistoryItem;
use fs2::FileExt;
use std::{
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct TranscriptStore {
    sessions_dir: PathBuf,
}

impl TranscriptStore {
    pub fn open(lato_home: &Path) -> Result<Self, String> {
        let sessions_dir = lato_home.join("sessions");
        fs::create_dir_all(&sessions_dir).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        Ok(Self { sessions_dir })
    }

    pub fn append(&self, session_id: &str, items: &[HistoryItem]) -> Result<(), String> {
        validate_session_id(session_id)?;
        let path = self.path(session_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        file.lock_exclusive().map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        for item in items {
            serde_json::to_writer(&mut file, item).map_err(|e| e.to_string())?;
            file.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        file.flush().map_err(|e| e.to_string())?;
        file.unlock().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load(&self, session_id: &str) -> Result<Vec<HistoryItem>, String> {
        self.load_optional(session_id)?
            .ok_or_else(|| "legacy transcript not found".into())
    }

    pub fn load_optional(&self, session_id: &str) -> Result<Option<Vec<HistoryItem>>, String> {
        validate_session_id(session_id)?;
        let path = self.path(session_id);
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let history = std::io::BufReader::new(file)
            .lines()
            .filter(|line| {
                line.as_ref()
                    .map(|line| !line.trim().is_empty())
                    .unwrap_or(true)
            })
            .map(|line| {
                let line = line.map_err(|e| e.to_string())?;
                serde_json::from_str(&line).map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(history))
    }

    pub fn list(&self) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().and_then(|v| v.to_str()) == Some("jsonl")
                && let Some(id) = path.file_stem().and_then(|v| v.to_str())
            {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.jsonl"))
    }
}

fn validate_session_id(id: &str) -> Result<(), String> {
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        Ok(())
    } else {
        Err("invalid session id".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a1_5_transcript_persists_lists_and_loads_without_replay() {
        let d = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(d.path()).unwrap();
        store
            .append(
                "session-1",
                &[
                    HistoryItem::User("hi".into()),
                    HistoryItem::AssistantText("hello".into()),
                ],
            )
            .unwrap();
        assert_eq!(store.list().unwrap(), vec!["session-1"]);
        let loaded = store.load("session-1").unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(matches!(&loaded[1], HistoryItem::AssistantText(v) if v == "hello"));
    }

    #[test]
    fn transcript_rejects_path_traversal_session_id() {
        let d = tempfile::tempdir().unwrap();
        let store = TranscriptStore::open(d.path()).unwrap();
        assert!(store.append("../auth", &[]).is_err());
    }
}
