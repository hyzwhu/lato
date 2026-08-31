use crate::lock_key;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Default)]
pub struct FileLocks {
    inner: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}
impl FileLocks {
    pub fn new() -> Self {
        Self::default()
    }
    pub async fn acquire(&self, path: &Path) -> OwnedMutexGuard<()> {
        let key = lock_key(path);
        let lock = {
            let mut map = self.inner.lock().await;
            map.entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }
}
