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
//! 4. capture the parent identity (canonical path and, on Unix, device and
//!    inode) and require it to be the canonical workspace root — rejects
//!    missing/non-directory parents, path escapes, and a parent directory
//!    swapped between checks;
//! 5. inspect the destination with non-following metadata; reject symlinks
//!    and any existing non-regular file;
//! 6. create a collision-resistant sibling temporary file with create-new
//!    semantics and user-only permissions, write, flush and sync it;
//! 7. repeat the parent-identity and non-following destination checks, then
//!    atomically rename the sibling over `plan.md` (rename never follows the
//!    final destination component) — the rename is the atomic commit point;
//! 8. sync the workspace-root directory before releasing the lock; the sync
//!    error is propagated to the caller, never swallowed.
//!
//! Failure semantics: every error before the rename leaves the previous
//! `plan.md` intact and removes only the temporary file created by this call.
//! Once the rename has committed, a later directory-sync failure cannot
//! restore the previous content; the freshly published plan remains on disk
//! and the error is reported to the caller instead of being silently
//! swallowed. No `create_dir_all`, no canonicalization through symlinks, no
//! cross-directory temporary file, no in-place truncate, no silent content
//! truncation.

use lato_core::{PLAN_DRAFT_MAX_BYTES, PLAN_FILE_NAME};
use lato_workspace::FileLocks;
use std::path::{Path, PathBuf};

/// Publication stages at which a test or fault drill can inject an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanDraftStage {
    ParentCheck,
    DestinationCheck,
    CreateTemp,
    Write,
    Flush,
    FileSync,
    SecondParentCheck,
    SecondDestinationCheck,
    Rename,
    DirSync,
}

/// Deterministic injection seam: return `Err(message)` to fail the given
/// stage with that message (the publication then follows the ordinary
/// failure path), or `Ok(())` to proceed. Production always uses
/// [`NO_PLAN_DRAFT_FAULTS`].
pub trait PlanDraftFaults: Send + Sync {
    fn on_stage(&self, stage: PlanDraftStage) -> Result<(), String>;
}

/// Production seam: never injects anything.
pub struct NoPlanDraftFaults;

impl PlanDraftFaults for NoPlanDraftFaults {
    fn on_stage(&self, _stage: PlanDraftStage) -> Result<(), String> {
        Ok(())
    }
}

pub async fn plan_draft(
    locks: &FileLocks,
    workspace_root: &Path,
    contents: &str,
) -> Result<String, String> {
    plan_draft_with_faults(locks, workspace_root, contents, &NO_PLAN_DRAFT_FAULTS).await
}

pub async fn plan_draft_with_faults(
    locks: &FileLocks,
    workspace_root: &Path,
    contents: &str,
    faults: &dyn PlanDraftFaults,
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

    // (4) capture the parent identity once; both checks compare against it.
    let parent = capture_parent_identity(&canonical_root)?;
    faults
        .on_stage(PlanDraftStage::ParentCheck)
        .map_err(|message| stage_error(PlanDraftStage::ParentCheck, message))?;
    verify_parent(&parent, &target)?;

    // (5) destination must not be a symlink or any non-regular file.
    faults
        .on_stage(PlanDraftStage::DestinationCheck)
        .map_err(|message| stage_error(PlanDraftStage::DestinationCheck, message))?;
    verify_destination(&target)?;

    // (6) collision-resistant sibling temporary file, create-new semantics.
    let temp_path = sibling_temp_path(&target);
    faults
        .on_stage(PlanDraftStage::CreateTemp)
        .map_err(|message| fail_stage(&temp_path, PlanDraftStage::CreateTemp, message))?;
    let mut file = create_temp_file(&temp_path)
        .map_err(|error| fail_stage(&temp_path, PlanDraftStage::CreateTemp, error))?;
    faults
        .on_stage(PlanDraftStage::Write)
        .map_err(|message| fail_stage(&temp_path, PlanDraftStage::Write, message))?;
    use std::io::Write as _;
    file.write_all(contents.as_bytes())
        .map_err(|error| fail_stage(&temp_path, PlanDraftStage::Write, error.to_string()))?;
    faults
        .on_stage(PlanDraftStage::Flush)
        .map_err(|message| fail_stage(&temp_path, PlanDraftStage::Flush, message))?;
    file.flush()
        .map_err(|error| fail_stage(&temp_path, PlanDraftStage::Flush, error.to_string()))?;
    faults
        .on_stage(PlanDraftStage::FileSync)
        .map_err(|message| fail_stage(&temp_path, PlanDraftStage::FileSync, message))?;
    file.sync_all()
        .map_err(|error| fail_stage(&temp_path, PlanDraftStage::FileSync, error.to_string()))?;
    drop(file);

    // (7) repeat both checks against the captured identity, then atomic
    // same-directory rename — the commit point of the publication.
    faults
        .on_stage(PlanDraftStage::SecondParentCheck)
        .map_err(|message| fail_stage(&temp_path, PlanDraftStage::SecondParentCheck, message))?;
    if let Err(error) = verify_parent(&parent, &target) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    faults
        .on_stage(PlanDraftStage::SecondDestinationCheck)
        .map_err(|message| {
            fail_stage(&temp_path, PlanDraftStage::SecondDestinationCheck, message)
        })?;
    if let Err(error) = verify_destination(&target) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    faults
        .on_stage(PlanDraftStage::Rename)
        .map_err(|message| fail_stage(&temp_path, PlanDraftStage::Rename, message))?;
    if let Err(error) = std::fs::rename(&temp_path, &target) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!("plan_draft: could not publish plan: {error}"));
    }

    // (8) sync the workspace-root directory before releasing the lock. The
    // rename has already committed the new plan; a directory-sync failure is
    // reported to the caller and never silently swallowed.
    faults
        .on_stage(PlanDraftStage::DirSync)
        .map_err(|message| stage_error(PlanDraftStage::DirSync, message))?;
    sync_directory(&canonical_root)?;

    Ok(format!(
        "plan written to {} ({byte_len} bytes)",
        target.display()
    ))
}

/// Creates the sibling temporary file with create-new semantics and
/// user-only permissions. A name collision (the path already exists) is a
/// hard failure: the caller never truncates or reuses an existing file.
fn create_temp_file(temp_path: &Path) -> Result<std::fs::File, String> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(temp_path)
        .map_err(|error| format!("plan_draft: could not create temporary file: {error}"))?;
    use std::io::Write as _;
    file.write_all(b"")
        .map_err(|error| format!("plan_draft: could not create temporary file: {error}"))?;
    Ok(file)
}

fn stage_error(stage: PlanDraftStage, message: String) -> String {
    format!("plan_draft: {stage:?} failed: {message}")
}

/// Failing a pre-rename stage removes only the temporary file created by
/// this call.
fn fail_stage(temp_path: &Path, stage: PlanDraftStage, message: String) -> String {
    let _ = std::fs::remove_file(temp_path);
    stage_error(stage, message)
}

struct ParentIdentity {
    path: PathBuf,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

fn capture_parent_identity(canonical_root: &Path) -> Result<ParentIdentity, String> {
    let metadata = std::fs::metadata(canonical_root)
        .map_err(|error| format!("plan_draft: parent directory unavailable: {error}"))?;
    if !metadata.is_dir() {
        return Err("plan_draft: workspace root is not a directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(ParentIdentity {
            path: canonical_root.to_path_buf(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    Ok(ParentIdentity {
        path: canonical_root.to_path_buf(),
    })
}

fn verify_parent(parent: &ParentIdentity, target: &Path) -> Result<(), String> {
    let Some(target_parent) = target.parent() else {
        return Err("plan_draft: target has no parent directory".into());
    };
    let current = std::fs::canonicalize(target_parent)
        .map_err(|error| format!("plan_draft: parent directory unavailable: {error}"))?;
    if current != parent.path {
        return Err("plan_draft: parent identity changed or escaped the workspace root".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = std::fs::metadata(target_parent)
            .map_err(|error| format!("plan_draft: parent directory unavailable: {error}"))?;
        if metadata.dev() != parent.dev || metadata.ino() != parent.ino {
            return Err(
                "plan_draft: parent directory was replaced between checks; refusing to publish"
                    .into(),
            );
        }
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
    sibling_temp_path_with_nonce(target, &unique_nonce())
}

fn sibling_temp_path_with_nonce(target: &Path, nonce: &str) -> PathBuf {
    target.with_file_name(format!(".{PLAN_FILE_NAME}.tmp-{nonce}"))
}

/// Collision-resistant nonce: wall-clock nanos, process id, and a per-process
/// counter (tests may pin the nonce via `sibling_temp_path_with_nonce`).
fn unique_nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{}-{}",
        nanos,
        std::process::id(),
        temp_counter().fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn temp_counter() -> &'static std::sync::atomic::AtomicU64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    &COUNTER
}

fn sync_directory(root: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let directory = std::fs::File::open(root).map_err(|error| {
            format!("plan_draft: could not open workspace root for sync: {error}")
        })?;
        directory.sync_all().map_err(|error| {
            format!("plan_draft: could not sync workspace root directory: {error}")
        })
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(())
    }
}

pub static NO_PLAN_DRAFT_FAULTS: NoPlanDraftFaults = NoPlanDraftFaults;

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

    /// Injects an error at one configured stage (and only there).
    struct FailAt(PlanDraftStage);
    impl PlanDraftFaults for FailAt {
        fn on_stage(&self, stage: PlanDraftStage) -> Result<(), String> {
            if stage == self.0 {
                Err(format!("injected failure at {stage:?}"))
            } else {
                Ok(())
            }
        }
    }

    /// Runs an action at one stage, then lets the stage succeed.
    struct ActAt<A: Fn(PlanDraftStage) + Send + Sync>(PlanDraftStage, A);
    impl<A: Fn(PlanDraftStage) + Send + Sync> PlanDraftFaults for ActAt<A> {
        fn on_stage(&self, stage: PlanDraftStage) -> Result<(), String> {
            if stage == self.0 {
                (self.1)(stage);
            }
            Ok(())
        }
    }

    fn leftovers(root: &Path) -> Vec<String> {
        std::fs::read_dir(root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp-"))
            .collect()
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
        assert!(leftovers(&root).is_empty());
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

    #[tokio::test]
    async fn pre_rename_stage_failures_preserve_previous_draft_and_clean_temp_files() {
        let stages_before_rename = [
            PlanDraftStage::ParentCheck,
            PlanDraftStage::DestinationCheck,
            PlanDraftStage::CreateTemp,
            PlanDraftStage::Write,
            PlanDraftStage::Flush,
            PlanDraftStage::FileSync,
            PlanDraftStage::SecondParentCheck,
            PlanDraftStage::SecondDestinationCheck,
        ];
        for stage in stages_before_rename {
            let directory = temp_workspace("stage-fail");
            let root = directory.path().to_path_buf();
            let locks = FileLocks::new();
            plan_draft(&locks, &root, "previous").await.unwrap();

            let error = plan_draft_with_faults(&locks, &root, "replacement", &FailAt(stage))
                .await
                .unwrap_err();
            assert!(
                error.contains(&format!("{stage:?}")),
                "stage {stage:?}: {error}"
            );
            assert_eq!(
                std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
                b"previous",
                "stage {stage:?} must preserve the previous draft"
            );
            assert!(
                leftovers(&root).is_empty(),
                "stage {stage:?} leaked temporary files"
            );
        }
    }

    #[tokio::test]
    async fn dir_sync_failure_is_propagated_and_never_reports_silent_success() {
        let directory = temp_workspace("dir-sync");
        let root = directory.path().to_path_buf();
        let locks = FileLocks::new();
        plan_draft(&locks, &root, "previous").await.unwrap();

        // After the rename has committed, a directory-sync failure cannot
        // restore the previous content: the freshly published plan remains,
        // and the caller receives the error instead of a success message.
        let error = plan_draft_with_faults(
            &locks,
            &root,
            "replacement",
            &FailAt(PlanDraftStage::DirSync),
        )
        .await
        .unwrap_err();
        assert!(error.contains("DirSync"), "{error}");
        assert_eq!(
            std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
            b"replacement"
        );
        assert!(leftovers(&root).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parent_directory_replaced_between_checks_is_rejected() {
        let directory = temp_workspace("parent-swap");
        let root = directory.path().to_path_buf();
        let locks = FileLocks::new();
        plan_draft(&locks, &root, "previous").await.unwrap();

        // Replace the workspace root directory (new device/inode identity)
        // between the first and second parent checks. The stale temporary
        // file disappears with the old directory; the new directory keeps the
        // previous draft and the publication is refused.
        let faults = ActAt(PlanDraftStage::SecondParentCheck, |_| {
            std::fs::remove_dir_all(&root).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join(PLAN_FILE_NAME), "previous").unwrap();
        });
        let error = plan_draft_with_faults(&locks, &root, "replacement", &faults)
            .await
            .unwrap_err();
        assert!(
            error.contains("replaced between checks"),
            "parent swap must be rejected: {error}"
        );
        assert_eq!(
            std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
            b"previous"
        );
    }

    #[test]
    fn temporary_file_name_collision_fails_without_truncating_the_existing_file() {
        let directory = temp_workspace("collision");
        let collision =
            sibling_temp_path_with_nonce(&directory.path().join(PLAN_FILE_NAME), "pinned");
        std::fs::write(&collision, b"innocent-bystander").unwrap();

        // Create-new semantics: an existing file at the temporary path is a
        // hard failure — the collided file is never truncated or reused.
        let error = create_temp_file(&collision).unwrap_err();
        assert!(error.contains("could not create temporary file"), "{error}");
        assert_eq!(std::fs::read(&collision).unwrap(), b"innocent-bystander");
        assert!(
            sibling_temp_path_with_nonce(&directory.path().join(PLAN_FILE_NAME), "other")
                != collision,
            "distinct nonces must produce distinct temporary paths"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failure_preserves_previous_draft_and_cleans_temp_files_without_seam() {
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
        assert!(leftovers(&root).is_empty());
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
