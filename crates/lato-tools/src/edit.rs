use lato_workspace::FileLocks;
use std::path::Path;

pub async fn search_replace(
    locks: &FileLocks,
    path: &Path,
    old: &str,
    new: &str,
) -> Result<(), String> {
    let _guard = locks.acquire(path).await;
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| e.to_string())?;
    if text.matches(old).count() != 1 {
        return Err("search_replace requires exactly one match".into());
    }
    tokio::fs::write(path, text.replacen(old, new, 1))
        .await
        .map_err(|e| e.to_string())
}

pub async fn write_file(locks: &FileLocks, path: &Path, contents: &str) -> Result<(), String> {
    let _guard = locks.acquire(path).await;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(path, contents)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn a2_3_same_file_serialized() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.txt");
        std::fs::write(&p, "ab").unwrap();
        let locks = Arc::new(FileLocks::new());
        let p1 = p.clone();
        let p2 = d.path().join("./f.txt");
        let l1 = locks.clone();
        let l2 = locks.clone();
        let a = tokio::spawn(async move { search_replace(&l1, &p1, "a", "A").await });
        let b = tokio::spawn(async move { search_replace(&l2, &p2, "b", "B").await });
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
        let got = std::fs::read_to_string(p).unwrap();
        assert_eq!(got, "AB");
    }

    #[tokio::test]
    async fn a3_2_parallel_edit_spellings_serialized() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.txt");
        std::fs::write(&p, "ab").unwrap();
        let locks = Arc::new(FileLocks::new());
        let p1 = p.clone();
        let p2 = d.path().join("./f.txt");
        let l1 = locks.clone();
        let l2 = locks.clone();
        let a = tokio::spawn(async move { search_replace(&l1, &p1, "a", "A").await });
        let b = tokio::spawn(async move { search_replace(&l2, &p2, "b", "B").await });
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "AB");
    }

    #[tokio::test]
    async fn write_file_creates_new_utf8_file_and_parents() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("src").join("hello.go");
        let locks = FileLocks::new();
        write_file(&locks, &p, "package main\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "package main\n");
    }
}
