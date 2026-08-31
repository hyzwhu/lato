use tokio::sync::{Mutex, mpsc};

pub const CONTEXT_HARD_LIMIT_BYTES: usize = 512_000;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum StreamPiece {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StreamError(pub String);

pub struct FakeModelStream {
    pub script: Mutex<Vec<Vec<StreamPiece>>>,
}
impl FakeModelStream {
    pub fn new(script: Vec<Vec<StreamPiece>>) -> Self {
        Self {
            script: Mutex::new(script),
        }
    }
    pub async fn stream(
        &self,
        prompt_bytes: usize,
        tx: mpsc::Sender<StreamPiece>,
    ) -> Result<(), String> {
        if prompt_bytes > CONTEXT_HARD_LIMIT_BYTES {
            return Err("context exceeds hard limit; compact not implemented".into());
        }
        let next = {
            let mut s = self.script.lock().await;
            if s.is_empty() {
                vec![StreamPiece::Text("ok".into())]
            } else {
                s.remove(0)
            }
        };
        for p in next {
            let _ = tx.send(p).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a2_6_oversize_fails_without_truncate() {
        let fake = FakeModelStream::new(vec![]);
        let (tx, _rx) = mpsc::channel(1);
        let err = fake
            .stream(CONTEXT_HARD_LIMIT_BYTES + 1, tx)
            .await
            .unwrap_err();
        assert!(err.contains("compact"));
    }
}
