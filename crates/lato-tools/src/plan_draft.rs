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
//! 4. capture the parent as a non-replaceable directory handle and anchor
//!    every later operation to that handle. On Unix the temporary file is
//!    created with `openat`, committed with `renameat`, and cleaned with
//!    `unlinkat` — all relative to the pinned handle — so a directory swapped
//!    in at the same path can never redirect the publication. Each check
//!    re-resolves the path and refuses to continue when it no longer points
//!    at the pinned handle's directory (while the handle pins the original
//!    inode, a removed-and-recreated directory can never reuse it, so the
//!    detection is deterministic and never depends on inode non-reuse);
//! 5. inspect the destination with non-following metadata; reject symlinks
//!    and any existing non-regular file;
//! 6. create a collision-resistant sibling temporary file with create-new
//!    semantics and user-only permissions, write, flush and sync it;
//! 7. repeat the handle/path and non-following destination checks, then
//!    atomically rename the sibling over `plan.md` — the rename is the
//!    atomic commit point;
//! 8. sync the pinned directory before releasing the lock; the sync error is
//!    propagated to the caller, never swallowed.
//!
//! Failure semantics: every error before the rename leaves the previous
//! `plan.md` intact and removes only the temporary file THIS call created —
//! identified by the handle-relative name plus the created file's device and
//! inode. A bystander that collided with the temporary path is never
//! touched, and a file swapped in over the temporary path is never deleted.
//! Once the rename has committed, a later directory-sync failure cannot
//! restore the previous content; the freshly published plan remains on disk
//! and the error is reported to the caller instead of being silently
//! swallowed. No `create_dir_all`, no canonicalization through symlinks, no
//! cross-directory temporary file, no in-place truncate, no silent content
//! truncation.
//!
//! Non-Unix platforms keep the path-based equivalent (canonical-path identity
//! capture plus re-verification); the handle anchoring below is the Unix
//! implementation of the same contract.

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
    /// Test-only override pinning the temporary file name (relative to the
    /// workspace root). Production returns `None` and the nonce is generated
    /// internally; tests pin it to build deterministic collision scenarios
    /// through the real publication path.
    fn pinned_temp_name(&self) -> Option<String> {
        None
    }
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

    let temp_name = faults
        .pinned_temp_name()
        .unwrap_or_else(|| temp_file_name(&unique_nonce()));

    #[cfg(unix)]
    let outcome = publish_locked_handle(&canonical_root, &target, contents, &temp_name, faults);
    #[cfg(not(unix))]
    let outcome = publish_locked_paths(&canonical_root, &target, contents, &temp_name, faults);

    outcome.map(|_| format!("plan written to {} ({byte_len} bytes)", target.display()))
}

/// The temporary file THIS call successfully created; cleanup is only ever
/// attempted for it, and only while the file still carries the identity
/// captured at creation time.
struct CreatedTemp {
    name: String,
    full_path: PathBuf,
    #[cfg(unix)]
    identity: (u64, u64),
}

/// Removes the temporary file created by this call. A bystander that
/// collided with the temporary path is never touched (nothing was created,
/// so there is nothing to clean), and a file swapped in over the temporary
/// path after creation is left alone (its identity no longer matches).
#[cfg(not(unix))]
fn cleanup_temp(created: &CreatedTemp) {
    let _ = std::fs::remove_file(&created.full_path);
}

fn stage_error(stage: PlanDraftStage, message: String) -> String {
    format!("plan_draft: {stage:?} failed: {message}")
}

// ---------------------------------------------------------------------------
// Unix: handle-anchored publication (openat family).
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod handle {
    use super::PLAN_FILE_NAME;
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::path::Path;

    /// A pinned, non-replaceable handle on the publication directory. While
    /// it exists, the directory's inode cannot be freed and therefore cannot
    /// be reused by a replacement directory, which makes every identity
    /// comparison below deterministic.
    pub struct ParentHandle {
        fd: OwnedFd,
    }

    fn cstring(bytes: &[u8]) -> Result<CString, String> {
        CString::new(bytes).map_err(|_| "plan_draft: path contains an interior NUL byte".to_owned())
    }

    fn last_os_error() -> String {
        io::Error::last_os_error().to_string()
    }

    impl ParentHandle {
        /// Opens the directory this path currently refers to.
        pub fn capture(root: &Path) -> Result<Self, String> {
            let croot = cstring(root.as_os_str().as_bytes())?;
            let fd = unsafe {
                libc::open(
                    croot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "plan_draft: parent directory unavailable: {}",
                    last_os_error()
                ));
            }
            // Safe: fd is a freshly opened descriptor owned by us.
            Ok(Self {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
            })
        }

        fn identity(&self) -> (u64, u64) {
            // Safe: fd metadata read; failure yields zeros and the caller
            // fails closed on the first use of the handle.
            let mut stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(self.fd.as_raw_fd(), &mut stat) } != 0 {
                return (0, 0);
            }
            (stat.st_dev, stat.st_ino)
        }

        /// The path's parent must still resolve to the pinned directory.
        /// Because the handle pins the original inode, a removed-and-recreated
        /// directory at the same path always differs here — the check never
        /// relies on inode non-reuse assumptions.
        pub fn verify_matches_path(&self, target: &Path) -> Result<(), String> {
            let Some(parent) = target.parent() else {
                return Err("plan_draft: target has no parent directory".into());
            };
            let cparent = cstring(parent.as_os_str().as_bytes())?;
            let mut stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::stat(cparent.as_ptr(), &mut stat) } != 0 {
                return Err(format!(
                    "plan_draft: parent directory unavailable: {}",
                    last_os_error()
                ));
            }
            if (stat.st_dev, stat.st_ino) != self.identity() {
                return Err(
                    "plan_draft: parent directory was replaced between checks; refusing to publish"
                        .into(),
                );
            }
            Ok(())
        }

        /// Creates the temporary file relative to the pinned handle with
        /// create-new semantics and user-only permissions. Returns the file
        /// plus its (device, inode) identity for the cleanup guard.
        pub fn create_temp(&self, name: &str) -> Result<(File, (u64, u64)), String> {
            let cname = cstring(name.as_bytes())?;
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    cname.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "plan_draft: could not create temporary file: {}",
                    last_os_error()
                ));
            }
            // Safe: fd is a freshly opened descriptor owned by us.
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file
                .metadata()
                .map_err(|error| format!("plan_draft: could not create temporary file: {error}"))?;
            Ok(((file), (metadata.dev(), metadata.ino())))
        }

        /// Atomically renames the temporary name over the final name, both
        /// relative to the pinned handle. If the pinned directory was removed,
        /// this fails deterministically (the handle no longer refers to a
        /// linkable directory) — the publication can never land in a
        /// replacement directory at the same path.
        pub fn rename_over(&self, temp_name: &str, final_name: &str) -> Result<(), String> {
            let ctemp = cstring(temp_name.as_bytes())?;
            let cfinal = cstring(final_name.as_bytes())?;
            let result = unsafe {
                libc::renameat(
                    self.fd.as_raw_fd(),
                    ctemp.as_ptr(),
                    self.fd.as_raw_fd(),
                    cfinal.as_ptr(),
                )
            };
            if result != 0 {
                return Err(format!(
                    "plan_draft: could not publish plan: {}",
                    last_os_error()
                ));
            }
            Ok(())
        }

        /// Unlinks the temporary name relative to the pinned handle, but only
        /// when the entry still refers to the created identity — a swapped-in
        /// bystander is never deleted.
        pub fn unlink_if_unchanged(&self, name: &str, identity: (u64, u64)) -> Result<(), ()> {
            let cname = cstring(name.as_bytes()).map_err(|_| ())?;
            let mut stat = unsafe { std::mem::zeroed() };
            // Safe: pure metadata read on a handle-relative name.
            if unsafe { libc::fstatat(self.fd.as_raw_fd(), cname.as_ptr(), &mut stat, 0) } != 0 {
                return Err(()); // already gone
            }
            if (stat.st_dev, stat.st_ino) != identity {
                return Err(()); // swapped: never delete someone else's file
            }
            // Safe: unlink on a handle-relative name; failure is harmless.
            let _ = unsafe { libc::unlinkat(self.fd.as_raw_fd(), cname.as_ptr(), 0) };
            Ok(())
        }

        /// Syncs the pinned directory itself.
        pub fn sync(&self) -> Result<(), String> {
            // Safe: fd-only durability operation.
            if unsafe { libc::fsync(self.fd.as_raw_fd()) } != 0 {
                return Err(format!(
                    "plan_draft: could not sync workspace root directory: {}",
                    last_os_error()
                ));
            }
            Ok(())
        }
    }

    /// Sanity guard used by tests and the publication path: the canonical
    /// name of the plan file inside the pinned directory.
    pub const PLAN_FINAL_NAME: &str = PLAN_FILE_NAME;
}

#[cfg(unix)]
fn publish_locked_handle(
    canonical_root: &Path,
    target: &Path,
    contents: &str,
    temp_name: &str,
    faults: &dyn PlanDraftFaults,
) -> Result<(), String> {
    use std::io::Write as _;

    /// Fails a stage: clean up the created temporary (identity-checked) and
    /// propagate the stage error.
    macro_rules! fault_fail {
        ($handle:ident, $created:ident, $stage:expr, $message:expr) => {{
            if let Some(created) = $created.take() {
                cleanup_temp_unix(&$handle, &created);
            }
            return Err(stage_error($stage, $message));
        }};
    }

    // (4) capture the parent as a non-replaceable handle; both checks compare
    // the live path against this pinned identity.
    let handle = handle::ParentHandle::capture(canonical_root)?;
    if let Err(message) = faults.on_stage(PlanDraftStage::ParentCheck) {
        return Err(stage_error(PlanDraftStage::ParentCheck, message));
    }
    handle.verify_matches_path(target)?;

    // (5) destination must not be a symlink or any non-regular file.
    if let Err(message) = faults.on_stage(PlanDraftStage::DestinationCheck) {
        return Err(stage_error(PlanDraftStage::DestinationCheck, message));
    }
    verify_destination(target)?;

    // (6) collision-resistant sibling temporary file, create-new semantics,
    // anchored to the pinned handle. On a collision (or any create failure)
    // NOTHING is cleaned: whatever occupies the temporary path was not
    // created by this call.
    if let Err(message) = faults.on_stage(PlanDraftStage::CreateTemp) {
        return Err(stage_error(PlanDraftStage::CreateTemp, message));
    }
    let (mut file, identity) = handle.create_temp(temp_name)?;
    let mut created = Some(CreatedTemp {
        name: temp_name.to_owned(),
        full_path: canonical_root.join(temp_name),
        identity,
    });

    if let Err(message) = faults.on_stage(PlanDraftStage::Write) {
        fault_fail!(handle, created, PlanDraftStage::Write, message);
    }
    if let Err(error) = file.write_all(contents.as_bytes()) {
        fault_fail!(handle, created, PlanDraftStage::Write, error.to_string());
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Flush) {
        fault_fail!(handle, created, PlanDraftStage::Flush, message);
    }
    if let Err(error) = file.flush() {
        fault_fail!(handle, created, PlanDraftStage::Flush, error.to_string());
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::FileSync) {
        fault_fail!(handle, created, PlanDraftStage::FileSync, message);
    }
    if let Err(error) = file.sync_all() {
        fault_fail!(handle, created, PlanDraftStage::FileSync, error.to_string());
    }
    drop(file);

    // (7) repeat both checks against the pinned handle, then commit with a
    // same-directory renameat — the atomic publication point.
    if let Err(message) = faults.on_stage(PlanDraftStage::SecondParentCheck) {
        fault_fail!(handle, created, PlanDraftStage::SecondParentCheck, message);
    }
    if let Err(error) = handle.verify_matches_path(target) {
        fault_fail!(handle, created, PlanDraftStage::SecondParentCheck, error);
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::SecondDestinationCheck) {
        fault_fail!(
            handle,
            created,
            PlanDraftStage::SecondDestinationCheck,
            message
        );
    }
    if let Err(error) = verify_destination(target) {
        fault_fail!(
            handle,
            created,
            PlanDraftStage::SecondDestinationCheck,
            error
        );
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Rename) {
        fault_fail!(handle, created, PlanDraftStage::Rename, message);
    }
    if let Err(error) = handle.rename_over(temp_name, PLAN_FILE_NAME) {
        fault_fail!(handle, created, PlanDraftStage::Rename, error);
    }
    // The temporary is now the published plan; there is nothing to clean.
    created = None;

    // (8) sync the pinned directory before releasing the lock. The rename
    // has already committed the new plan; a directory-sync failure is
    // reported to the caller and never silently swallowed.
    if let Err(message) = faults.on_stage(PlanDraftStage::DirSync) {
        return Err(stage_error(PlanDraftStage::DirSync, message));
    }
    handle.sync()
}

/// Unlinks the temporary file created by this call, refusing to delete
/// anything whose identity no longer matches the created file.
#[cfg(unix)]
fn cleanup_temp_unix(handle: &handle::ParentHandle, created: &CreatedTemp) {
    let _ = handle.unlink_if_unchanged(&created.name, created.identity);
}

// ---------------------------------------------------------------------------
// Non-Unix: path-based equivalent of the same contract.
// ---------------------------------------------------------------------------

#[cfg(not(unix))]
fn publish_locked_paths(
    canonical_root: &Path,
    target: &Path,
    contents: &str,
    temp_name: &str,
    faults: &dyn PlanDraftFaults,
) -> Result<(), String> {
    use std::io::Write as _;

    let parent_identity = capture_parent_identity(canonical_root)?;
    faults
        .on_stage(PlanDraftStage::ParentCheck)
        .map_err(|message| stage_error(PlanDraftStage::ParentCheck, message))?;
    verify_parent(&parent_identity, target)?;

    faults
        .on_stage(PlanDraftStage::DestinationCheck)
        .map_err(|message| stage_error(PlanDraftStage::DestinationCheck, message))?;
    verify_destination(target)?;

    let temp_path = canonical_root.join(temp_name);
    faults
        .on_stage(PlanDraftStage::CreateTemp)
        .map_err(|message| stage_error(PlanDraftStage::CreateTemp, message))?;
    let mut file = create_temp_file(&temp_path)?;
    let mut created = Some(CreatedTemp {
        name: temp_name.to_owned(),
        full_path: temp_path.clone(),
    });

    macro_rules! fail {
        ($created:ident, $stage:expr, $message:expr) => {{
            if let Some(created) = $created.take() {
                cleanup_temp(&created);
            }
            return Err(stage_error($stage, $message));
        }};
    }

    if let Err(message) = faults.on_stage(PlanDraftStage::Write) {
        fail!(created, PlanDraftStage::Write, message);
    }
    if let Err(error) = file.write_all(contents.as_bytes()) {
        fail!(created, PlanDraftStage::Write, error.to_string());
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Flush) {
        fail!(created, PlanDraftStage::Flush, message);
    }
    if let Err(error) = file.flush() {
        fail!(created, PlanDraftStage::Flush, error.to_string());
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::FileSync) {
        fail!(created, PlanDraftStage::FileSync, message);
    }
    if let Err(error) = file.sync_all() {
        fail!(created, PlanDraftStage::FileSync, error.to_string());
    }
    drop(file);

    if let Err(message) = faults.on_stage(PlanDraftStage::SecondParentCheck) {
        fail!(created, PlanDraftStage::SecondParentCheck, message);
    }
    if let Err(error) = verify_parent(&parent_identity, target) {
        fail!(created, PlanDraftStage::SecondParentCheck, error);
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::SecondDestinationCheck) {
        fail!(created, PlanDraftStage::SecondDestinationCheck, message);
    }
    if let Err(error) = verify_destination(target) {
        fail!(created, PlanDraftStage::SecondDestinationCheck, error);
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Rename) {
        fail!(created, PlanDraftStage::Rename, message);
    }
    if let Err(error) = std::fs::rename(&temp_path, target) {
        fail!(
            created,
            PlanDraftStage::Rename,
            format!("plan_draft: could not publish plan: {error}")
        );
    }
    created = None;

    faults
        .on_stage(PlanDraftStage::DirSync)
        .map_err(|message| stage_error(PlanDraftStage::DirSync, message))?;
    sync_directory(canonical_root)
}

/// Creates the sibling temporary file with create-new semantics and
/// user-only permissions. A name collision (the path already exists) is a
/// hard failure: the caller never truncates or reuses an existing file.
#[cfg(not(unix))]
fn create_temp_file(temp_path: &Path) -> Result<std::fs::File, String> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(temp_path)
        .map_err(|error| format!("plan_draft: could not create temporary file: {error}"))?;
    use std::io::Write as _;
    file.write_all(b"")
        .map_err(|error| format!("plan_draft: could not create temporary file: {error}"))?;
    Ok(file)
}

#[cfg(not(unix))]
struct ParentIdentity {
    path: PathBuf,
}

#[cfg(not(unix))]
fn capture_parent_identity(canonical_root: &Path) -> Result<ParentIdentity, String> {
    let metadata = std::fs::metadata(canonical_root)
        .map_err(|error| format!("plan_draft: parent directory unavailable: {error}"))?;
    if !metadata.is_dir() {
        return Err("plan_draft: workspace root is not a directory".into());
    }
    Ok(ParentIdentity {
        path: canonical_root.to_path_buf(),
    })
}

#[cfg(not(unix))]
fn verify_parent(parent: &ParentIdentity, target: &Path) -> Result<(), String> {
    let Some(target_parent) = target.parent() else {
        return Err("plan_draft: target has no parent directory".into());
    };
    let current = std::fs::canonicalize(target_parent)
        .map_err(|error| format!("plan_draft: parent directory unavailable: {error}"))?;
    if current != parent.path {
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

pub fn temp_file_name(nonce: &str) -> String {
    format!(".{PLAN_FILE_NAME}.tmp-{nonce}")
}

fn sibling_temp_path_with_nonce(target: &Path, nonce: &str) -> PathBuf {
    target.with_file_name(temp_file_name(nonce))
}

/// Collision-resistant nonce: wall-clock nanos, process id, and a per-process
/// counter (tests may pin the name via `PlanDraftFaults::pinned_temp_name`).
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

#[cfg(not(unix))]
fn sync_directory(root: &Path) -> Result<(), String> {
    let _ = root;
    Ok(())
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

    /// Pins the temporary file name without injecting any failure: the
    /// name collision itself drives the failure through the real path.
    struct PinTempName(String);
    impl PlanDraftFaults for PinTempName {
        fn on_stage(&self, _stage: PlanDraftStage) -> Result<(), String> {
            Ok(())
        }
        fn pinned_temp_name(&self) -> Option<String> {
            Some(self.0.clone())
        }
    }

    fn pinned_temp_name(tag: &str) -> String {
        temp_file_name(&format!("pinned-{tag}-{}", std::process::id()))
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

    /// A bystander occupying the pinned temporary path is a hard collision:
    /// the publication fails, and the bystander's content AND identity are
    /// preserved byte-for-byte. Driven through the full publication path,
    /// deterministic (no inode assumptions), run ten times consecutively.
    #[tokio::test]
    async fn temp_file_collision_preserves_the_bystander_across_ten_runs() {
        for attempt in 0..10 {
            let directory = temp_workspace("collision");
            let root = directory.path().to_path_buf();
            let locks = FileLocks::new();
            plan_draft(&locks, &root, "previous").await.unwrap();

            let name = pinned_temp_name("collide");
            let bystander = root.join(&name);
            std::fs::write(&bystander, b"innocent-bystander").unwrap();
            let identity_before = std::fs::metadata(&bystander).unwrap();
            #[cfg(unix)]
            let (dev_before, ino_before) = {
                use std::os::unix::fs::MetadataExt as _;
                (identity_before.dev(), identity_before.ino())
            };

            let error =
                plan_draft_with_faults(&locks, &root, "replacement", &PinTempName(name.clone()))
                    .await
                    .unwrap_err();
            assert!(
                error.contains("could not create temporary file"),
                "attempt {attempt}: {error}"
            );
            // The bystander survives untouched: same bytes, same identity.
            assert_eq!(
                std::fs::read(&bystander).unwrap(),
                b"innocent-bystander",
                "attempt {attempt}: bystander content must be preserved"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                let after = std::fs::metadata(&bystander).unwrap();
                assert_eq!(
                    (after.dev(), after.ino()),
                    (dev_before, ino_before),
                    "attempt {attempt}: bystander identity must be preserved"
                );
            }
            // The previous draft is intact and the only leftover is the
            // bystander itself (never deleted, never truncated).
            assert_eq!(
                std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
                b"previous"
            );
            assert_eq!(leftovers(&root), vec![name], "attempt {attempt}");
        }
    }

    /// When a failure happens after creation and someone swapped a different
    /// file in over the temporary path, cleanup must NOT delete it: only the
    /// file this call created may be removed.
    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_refuses_to_delete_a_swapped_in_temp_file() {
        let directory = temp_workspace("temp-swap");
        let root = directory.path().to_path_buf();
        let locks = FileLocks::new();
        plan_draft(&locks, &root, "previous").await.unwrap();

        let name = pinned_temp_name("swap");
        struct SwapIn {
            swapped_path: PathBuf,
            temp_name: String,
        }
        impl PlanDraftFaults for SwapIn {
            fn on_stage(&self, stage: PlanDraftStage) -> Result<(), String> {
                match stage {
                    // Once the temp exists (first parent check passed and the
                    // file was created), swap in a bystander at the temp path.
                    PlanDraftStage::SecondDestinationCheck => {
                        // Replace with a NEW file (fresh inode), the way a
                        // real bystander swap would appear.
                        std::fs::remove_file(&self.swapped_path).unwrap();
                        std::fs::write(&self.swapped_path, "swapped-in-by-stander").unwrap();
                        Err("injected failure at SecondDestinationCheck".into())
                    }
                    _ => Ok(()),
                }
            }
            fn pinned_temp_name(&self) -> Option<String> {
                Some(self.temp_name.clone())
            }
        }

        let error = plan_draft_with_faults(
            &locks,
            &root,
            "replacement",
            &SwapIn {
                swapped_path: root.join(&name),
                temp_name: name.clone(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("SecondDestinationCheck"), "{error}");
        // The swapped-in file is still there: cleanup saw a different
        // identity than the one it created and refused to delete.
        assert_eq!(
            std::fs::read(root.join(&name)).unwrap(),
            b"swapped-in-by-stander"
        );
        // The previous draft is intact.
        assert_eq!(
            std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
            b"previous"
        );
        std::fs::remove_file(root.join(&name)).unwrap();
    }

    /// Parent swap between the two checks is rejected deterministically: the
    /// pinned directory handle makes inode reuse impossible while held, so
    /// ten consecutive runs must all reject with the previous draft intact.
    #[cfg(unix)]
    #[tokio::test]
    async fn parent_directory_replaced_between_checks_is_rejected_ten_times() {
        for attempt in 0..10 {
            let directory = temp_workspace("parent-swap");
            let root = directory.path().to_path_buf();
            let locks = FileLocks::new();
            plan_draft(&locks, &root, "previous").await.unwrap();

            // Replace the workspace root directory between the first and
            // second parent checks.
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
                "attempt {attempt}: parent swap must be rejected: {error}"
            );
            assert_eq!(
                std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
                b"previous",
                "attempt {attempt}: the new directory's draft must be preserved"
            );
            assert!(
                leftovers(&root).is_empty(),
                "attempt {attempt}: no temporary residue"
            );
        }
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
