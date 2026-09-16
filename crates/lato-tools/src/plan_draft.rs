//! `plan_draft` — the sole mutation permitted while Plan mode is active
//! (Phase 8B frozen spec v1.0 §3.1).
//!
//! Secure publication contract, in order:
//! 1. derive the target from the immutable canonical workspace root — the
//!    caller can never supply a path;
//! 2. reject UTF-8 payloads above [`PLAN_DRAFT_MAX_BYTES`] bytes before any
//!    filesystem mutation (never truncate);
//! 3. hold the session-shared [`FileLocks`] entry for the derived target
//!    through publication and directory sync (all aliases share the key via
//!    `lock_key` canonicalization);
//! 4. re-resolve the target parent and require it to be the canonical
//!    workspace root (rejects missing/non-directory parents, path escapes and
//!    changed parent identity);
//! 5. inspect the destination with non-following metadata; reject symlinks
//!    and any existing non-regular file;
//! 6. create a collision-resistant sibling temporary file with create-new
//!    semantics and user-only permissions, write, flush and sync it;
//! 7. repeat the parent-identity and non-following destination checks, then
//!    atomically rename the sibling over `plan.md` (rename never follows the
//!    final destination component);
//! 8. sync the workspace-root directory before releasing the lock.
//!
//! On any error only the temporary file created by this call is removed and
//! the previous `plan.md` is left intact. No `create_dir_all`, no
//! canonicalization through symlinks, no cross-directory temporary file, no
//! in-place truncate, no silent content truncation.

use lato_core::{PLAN_DRAFT_MAX_BYTES, PLAN_FILE_NAME};
use lato_workspace::FileLocks;
use std::path::{Path, PathBuf};

pub async fn plan_draft(
    locks: &FileLocks,
    workspace_root: &Path,
    contents: &str,
) -> Result<String, String> {
    // (1) immutable canonical root, derived once; the payload carries no path.
    let canonical_root = std::fs::canonicalize(workspace_root)
        .map_err(|error| format!("plan_draft: workspace root unavailable: {error}"))?;
    let target: PathBuf = canonical_root.join(PLAN_FILE_NAME);

    // (2) byte bound before any filesystem mutation, UTF-8 bytes, no truncate.
    let byte_len = contents.len();
    if byte_len > PLAN_DRAFT_MAX_BYTES {
        return Err(format!(
            "plan_draft: draft is {byte_len} bytes, exceeding the hard limit of {PLAN_DRAFT_MAX_BYTES}; it was not written"
        ));
    }

    // (3) session-shared lock held through publication and directory sync.
    let _guard = locks.acquire(&target).await;

    // (4) parent identity: must still be the canonical workspace root.
    verify_parent(&canonical_root, &target)?;

    // (5) destination must not be a symlink or any non-regular file.
    verify_destination(&target)?;

    // (6) collision-resistant sibling temporary file, create-new semantics.
    let temp_path = sibling_temp_path(&target);
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|error| format!("plan_draft: could not create temporary file: {error}"))?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                let _ = std::fs::remove_file(&temp_path);
                format!("plan_draft: could not write temporary file: {error}")
            })?;
    }

    // (7) repeat both checks, then atomic same-directory rename.
    if let Err(error) =
        verify_parent(&canonical_root, &target).and_then(|()| verify_destination(&target))
    {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp_path, &target) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("plan_draft: could not publish plan: {error}"));
    }

    // (8) sync the workspace-root directory before releasing the lock.
    sync_directory(&canonical_root);

    Ok(format!(
        "plan written to {} ({byte_len} bytes)",
        target.display()
    ))
}

fn verify_parent(canonical_root: &Path, target: &Path) -> Result<(), String> {
    let Some(parent) = target.parent() else {
        return Err("plan_draft: target has no parent directory".into());
    };
    let parent_identity = std::fs::canonicalize(parent)
        .map_err(|error| format!("plan_draft: parent directory unavailable: {error}"))?;
    if parent_identity != canonical_root {
        return Err("plan_draft: parent identity changed or escaped the workspace root".into());
    }
    Ok(())
}

fn verify_destination(target: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "plan_draft: could not inspect destination: {error}"
        )),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err("plan_draft: destination is a symlink; refusing to publish".into());
            }
            if !metadata.is_file() {
                return Err(
                    "plan_draft: destination is not a regular file; refusing to publish".into(),
                );
            }
            Ok(())
        }
    }
}

fn sibling_temp_path(target: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    target.with_file_name(format!(
        ".{}.tmp-{}-{}-{}",
        PLAN_FILE_NAME,
        std::process::id(),
        nanos,
        temp_counter().fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

fn temp_counter() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    &COUNTER
}

fn sync_directory(root: &Path) {
    #[cfg(unix)]
    {
        if let Ok(directory) = std::fs::File::open(root) {
            let _ = directory.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = root;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_workspace(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("lato-plan-draft-{tag}-"))
            .tempdir()
            .unwrap()
    }

    #[tokio::test]
    async fn publishes_plan_and_replaces_atomically() {
        let directory = temp_workspace("publish");
        let root = directory.path().to_path_buf();
        let locks = FileLocks::new();

        let first = plan_draft(&locks, &root, "# plan v1").await.unwrap();
        assert!(first.contains("plan.md"));
        assert_eq!(
            std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
            b"# plan v1"
        );

        let second = plan_draft(&locks, &root, "# plan v2").await.unwrap();
        assert!(second.contains("plan.md"));
        assert_eq!(
            std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
            b"# plan v2"
        );

        // No temporary files survive publication.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files leaked: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn rejects_oversized_payloads_without_truncation() {
        let directory = temp_workspace("oversize");
        let root = directory.path().to_path_buf();
        let locks = FileLocks::new();

        // Exactly at the cap succeeds.
        let exact = "a".repeat(PLAN_DRAFT_MAX_BYTES);
        plan_draft(&locks, &root, &exact).await.unwrap();

        // One byte over is rejected and leaves the previous draft intact.
        let oversized = "a".repeat(PLAN_DRAFT_MAX_BYTES + 1);
        let error = plan_draft(&locks, &root, &oversized).await.unwrap_err();
        assert!(error.contains("exceeding"), "{error}");
        assert_eq!(
            std::fs::read(root.join(PLAN_FILE_NAME)).unwrap().len(),
            PLAN_DRAFT_MAX_BYTES
        );
    }

    #[tokio::test]
    async fn rejects_symlink_destination_without_touching_it() {
        let directory = temp_workspace("symlink");
        let root = directory.path().to_path_buf();
        let outside = directory.path().join("outside.md");
        std::fs::write(&outside, "outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join(PLAN_FILE_NAME)).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, root.join(PLAN_FILE_NAME)).unwrap();

        let error = plan_draft(&FileLocks::new(), &root, "# hijack")
            .await
            .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_non_regular_destination_fifo() {
        let directory = temp_workspace("fifo");
        let root = directory.path().to_path_buf();
        let fifo = root.join(PLAN_FILE_NAME);
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        let error = plan_draft(&FileLocks::new(), &root, "# x")
            .await
            .unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        assert!(fifo.exists());
    }

    #[tokio::test]
    async fn rejects_directory_destination_and_keeps_it() {
        let directory = temp_workspace("dir-dest");
        let root = directory.path().to_path_buf();
        std::fs::create_dir(root.join(PLAN_FILE_NAME)).unwrap();
        let error = plan_draft(&FileLocks::new(), &root, "# x")
            .await
            .unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        assert!(root.join(PLAN_FILE_NAME).is_dir());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_preserves_previous_draft_and_cleans_temp_files() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let directory = temp_workspace("preserve");
        let root = directory.path().to_path_buf();
        let locks = FileLocks::new();
        plan_draft(&locks, &root, "previous").await.unwrap();

        // Make the workspace read-only so creating the temporary file fails.
        // If the probe write still succeeds (e.g. running as root), the
        // injection is ineffective and the scenario is skipped.
        let mut permissions = std::fs::metadata(&root).unwrap().permissions();
        permissions.set_mode(0o500);
        std::fs::set_permissions(&root, permissions).unwrap();
        let probe = root.join(".perm-probe");
        if std::fs::File::create(&probe).is_ok_and(|mut file| {
            file.write_all(b"probe").is_ok() && std::fs::remove_file(&probe).is_ok()
        }) {
            let mut permissions = std::fs::metadata(&root).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&root, permissions).unwrap();
            return;
        }

        let error = plan_draft(&locks, &root, "replacement").await.unwrap_err();
        assert!(!error.is_empty());

        let mut permissions = std::fs::metadata(&root).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&root, permissions).unwrap();

        assert_eq!(
            std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
            b"previous"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files leaked: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_publications_serialize_on_the_shared_lock() {
        let directory = temp_workspace("concurrent");
        let root: Arc<PathBuf> = Arc::new(directory.path().to_path_buf());
        let locks = Arc::new(FileLocks::new());
        let mut handles = Vec::new();
        for index in 0..8 {
            let locks = locks.clone();
            let root = root.clone();
            handles.push(tokio::spawn(async move {
                plan_draft(&locks, &root, &format!("# draft {index}")).await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        let final_draft = std::fs::read_to_string(root.join(PLAN_FILE_NAME)).unwrap();
        assert!(final_draft.starts_with("# draft "));
        // The final content is exactly one complete draft, not interleaved.
        assert_eq!(final_draft.matches("# draft").count(), 1);
    }
}
