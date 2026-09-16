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
//!    every later operation to that handle. Each check re-resolves the path
//!    and refuses to continue when it no longer points at the pinned
//!    handle's directory (while the handle pins the original inode, a
//!    removed-and-recreated directory can never reuse it, so the detection
//!    is deterministic and never depends on inode non-reuse);
//! 5. inspect the destination with non-following metadata; reject symlinks
//!    and any existing non-regular file;
//! 6. create the temporary file with user-only permissions, write, flush
//!    and sync it;
//! 7. repeat the handle/path and non-following destination checks, then
//!    atomically publish — the commit point;
//! 8. sync the pinned directory before releasing the lock; the sync error is
//!    propagated to the caller, never swallowed.
//!
//! The temporary file is platform-specific by necessity:
//!
//! * **Linux** uses `O_TMPFILE`: the temporary has NO directory entry for its
//!   whole lifetime, so there is no path a bystander could collide with or be
//!   swapped into, and cleanup is simply `close` — no `unlink` on any path,
//!   ever. Publication binds the nameless inode to `plan.md` atomically with
//!   `linkat(.., AT_EMPTY_PATH)` plus `renameat`.
//! * **Other Unix (macOS)** keeps a named sibling temporary file. Cleanup
//!   never `unlink`s the public temporary path directly: the file is first
//!   atomically moved aside (`renameat` to a private name), its identity is
//!   verified against the still-open descriptor, and only then is the
//!   private name removed. If the identity does not match — or the
//!   [`PlanDraftFaults::on_before_removal`] seam reports a swap after the
//!   identity check — the removal is safely ABANDONED and the isolated file
//!   is restored to its original path, so a bystander can never be deleted
//!   by this call.
//! * **Windows** keeps the path-based equivalent (create-new plus
//!   identity-checked cleanup), unchanged.
//!
//! Failure semantics: every error before the commit leaves the previous
//! `plan.md` intact and removes only the temporary file THIS call created —
//! and on Linux there is never anything to remove. Once the commit has
//! happened, a later directory-sync failure cannot restore the previous
//! content; the freshly published plan remains on disk and the error is
//! reported to the caller instead of being silently swallowed. No
//! `create_dir_all`, no canonicalization through symlinks, no
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
    /// Test-only override pinning the temporary file name (relative to the
    /// workspace root; named-temporary platforms only). Production returns
    /// `None` and the nonce is generated internally; tests pin it to build
    /// deterministic collision scenarios through the real publication path.
    fn pinned_temp_name(&self) -> Option<String> {
        None
    }
    /// Test-only seam invoked AFTER the identity check of the isolated
    /// temporary file succeeded and BEFORE its final removal (named-
    /// temporary platforms). Returning `Err` makes the cleanup safely
    /// ABANDON the removal: the isolated file is restored to its original
    /// location so a bystander swapped in at that instant can never be
    /// deleted by this call. Production returns `Ok(())`.
    fn on_before_removal(&self, isolated_path: &str) -> Result<(), String> {
        let _ = isolated_path;
        Ok(())
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

    #[cfg(all(unix, not(target_os = "linux")))]
    let outcome = publish_locked_handle(&canonical_root, &target, contents, &temp_name, faults);
    #[cfg(target_os = "linux")]
    let outcome = publish_locked_nameless(&canonical_root, &target, contents, &temp_name, faults);
    #[cfg(not(unix))]
    let outcome = publish_locked_paths(&canonical_root, &target, contents, &temp_name, faults);

    outcome.map(|_| format!("plan written to {} ({byte_len} bytes)", target.display()))
}

/// The temporary file THIS call successfully created; cleanup is only ever
/// attempted for it, and only while the file still carries the identity
/// captured at creation time. (Linux does not need it: the O_TMPFILE
/// temporary has no directory entry and cleanup is just `close`.)
#[cfg(not(target_os = "linux"))]
struct CreatedTemp {
    #[cfg(all(unix, not(target_os = "linux")))]
    name: String,
    #[cfg(not(unix))]
    full_path: PathBuf,
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
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
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

    /// Platform-neutral identity widening: `st_dev` is `i32` on macOS, `u64`
    /// on Linux, `u32` on Windows, and `st_ino` is `u64` on the Unix targets
    /// but `u32` on Windows. Every platform widens both to `u64` here, so
    /// all identity comparisons in this module are type-stable across
    /// platforms (device numbers and inode numbers are non-negative, so a
    /// plain numeric widening is lossless).
    // The cast is a real `i32 -> u64` widening on macOS (and a `u32 -> u64`
    // widening on Windows), so it must stay despite being a no-op on Linux.
    #[allow(clippy::unnecessary_cast)]
    fn widen_dev(dev: libc::dev_t) -> u64 {
        dev as u64
    }

    #[allow(clippy::unnecessary_cast)]
    fn widen_ino(ino: libc::ino_t) -> u64 {
        ino as u64
    }

    /// The `(device, inode)` identity in the platform-neutral widened form.
    type FileIdentity = (u64, u64);

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

        fn identity(&self) -> FileIdentity {
            // Safe: fd metadata read; failure yields zeros and the caller
            // fails closed on the first use of the handle.
            let mut stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(self.fd.as_raw_fd(), &mut stat) } != 0 {
                return (0, 0);
            }
            (widen_dev(stat.st_dev), widen_ino(stat.st_ino))
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
            if (widen_dev(stat.st_dev), widen_ino(stat.st_ino)) != self.identity() {
                return Err(
                    "plan_draft: parent directory was replaced between checks; refusing to publish"
                        .into(),
                );
            }
            Ok(())
        }

        /// Creates the temporary file relative to the pinned handle with
        /// create-new semantics and user-only permissions (named-temporary
        /// platforms only; Linux uses [`Self::create_nameless_temp`]).
        #[cfg(all(unix, not(target_os = "linux")))]
        pub fn create_temp(&self, name: &str) -> Result<File, String> {
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
            // Safe: fd is a freshly opened descriptor owned by us. The
            // caller keeps it open until the cleanup decision has been made,
            // pinning the inode against reuse.
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        /// Linux: creates a NAMELESS temporary file inside the pinned
        /// directory (`O_TMPFILE`). The file has no directory entry for its
        /// whole lifetime, so no bystander can collide with it or be swapped
        /// into it, and cleanup is simply closing the descriptor — this call
        /// never needs to unlink anything.
        #[cfg(target_os = "linux")]
        pub fn create_nameless_temp(&self) -> Result<File, String> {
            // Safe: O_TMPFILE on the pinned directory; the path component is
            // ignored except to resolve to the directory itself.
            let dot = cstring(b".")?;
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_TMPFILE | libc::O_WRONLY | libc::O_CLOEXEC,
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
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        /// Linux: binds the NAMELESS inode referenced by `file` to
        /// `final_name` atomically. `linkat(.., AT_EMPTY_PATH)` operates on
        /// the descriptor itself — it cannot be redirected by any path
        /// swap — and `EEXIST` on the intermediate name is impossible (a
        /// fresh random nonce). The final `renameat` then atomically moves
        /// OUR inode over `plan.md` (the pre-existing regular file there is
        /// replaced, the established publication semantics). If the
        /// intermediate link was swapped away meanwhile, the `renameat`
        /// fails with `ENOENT` and the caller closes the descriptor: our
        /// file has no directory entry left, so cleanup needs no `unlink`
        /// and a swapped-in bystander is never touched.
        #[cfg(target_os = "linux")]
        pub fn publish_by_identity(
            &self,
            file: &File,
            middle_name: &str,
            final_name: &str,
        ) -> Result<(), String> {
            let empty = cstring(b"")?;
            let cmiddle = cstring(middle_name.as_bytes())?;
            // Safe: binds the descriptor's own inode to a fresh name.
            let linked = unsafe {
                libc::linkat(
                    file.as_raw_fd(),
                    empty.as_ptr(),
                    self.fd.as_raw_fd(),
                    cmiddle.as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            if linked != 0 {
                let error = io::Error::last_os_error();
                // Some filesystems/containers restrict AT_EMPTY_PATH for
                // non-O_PATH descriptors; fall back to the /proc magic
                // symlink, which links the very same inode.
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM)
                        | Some(libc::EOPNOTSUPP)
                        | Some(libc::EINVAL)
                        | Some(libc::ENOENT)
                ) {
                    let procpath =
                        cstring(format!("/proc/self/fd/{}", file.as_raw_fd()).as_bytes())?;
                    // Safe: links the descriptor's own inode via procfs.
                    let retry = unsafe {
                        libc::linkat(
                            libc::AT_FDCWD,
                            procpath.as_ptr(),
                            self.fd.as_raw_fd(),
                            cmiddle.as_ptr(),
                            libc::AT_SYMLINK_FOLLOW,
                        )
                    };
                    if retry != 0 {
                        return Err(format!(
                            "plan_draft: could not publish plan: {}",
                            last_os_error()
                        ));
                    }
                } else {
                    return Err(format!("plan_draft: could not publish plan: {error}"));
                }
            }
            let cfinal = cstring(final_name.as_bytes())?;
            // Safe: atomic move of our freshly linked name over the target.
            if unsafe {
                libc::renameat(
                    self.fd.as_raw_fd(),
                    cmiddle.as_ptr(),
                    self.fd.as_raw_fd(),
                    cfinal.as_ptr(),
                )
            } != 0
            {
                return Err(format!(
                    "plan_draft: could not publish plan: {}",
                    last_os_error()
                ));
            }
            Ok(())
        }

        /// Atomically renames the temporary name over the final name, both
        /// relative to the pinned handle. If the pinned directory was removed,
        /// this fails deterministically (the handle no longer refers to a
        /// linkable directory) — the publication can never land in a
        /// replacement directory at the same path.
        #[cfg(all(unix, not(target_os = "linux")))]
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

        /// Unix: removes the temporary file created by this call WITHOUT
        /// ever unlinking a path that could have been swapped. The temporary
        /// name is first moved aside ATOMICALLY (`renameat` to a private
        /// `.reap` name — this deletes nothing), the isolated name's identity
        /// is then verified against the still-open created descriptor, and
        /// only on a match is the PRIVATE name removed. On any mismatch — or
        /// when the [`PlanDraftFaults::on_before_removal`] seam reports a
        /// swap after the identity check — the removal is safely abandoned
        /// and the isolated file is restored to its original location, so a
        /// bystander can never be deleted by this call. (Linux reaches this
        /// only on the rare failure path after the intermediate link was
        /// already created; the normal path has no directory entry at all.)
        pub fn remove_isolated(
            &self,
            name: &str,
            owned: &File,
            faults: &dyn crate::PlanDraftFaults,
        ) {
            let reap_name = format!("{name}.reap-{}", super::unique_nonce());
            let cname = match cstring(name.as_bytes()) {
                Ok(value) => value,
                Err(_) => return,
            };
            let creap = match cstring(reap_name.as_bytes()) {
                Ok(value) => value,
                Err(_) => return,
            };
            // Safe: atomic move of OUR temporary name to a private name;
            // nothing is deleted and any content is preserved under `reap`.
            if unsafe {
                libc::renameat(
                    self.fd.as_raw_fd(),
                    cname.as_ptr(),
                    self.fd.as_raw_fd(),
                    creap.as_ptr(),
                )
            } != 0
            {
                return; // nothing to clean at that name
            }
            // Identity of the created file, from its still-open descriptor
            // (pins the inode against reuse).
            let mut own = unsafe { std::mem::zeroed() };
            // Safe: metadata read on our own still-open descriptor.
            if unsafe { libc::fstat(owned.as_raw_fd(), &mut own) } != 0 {
                return;
            }
            let mut current = unsafe { std::mem::zeroed() };
            // Safe: pure metadata read on a handle-relative name.
            if unsafe { libc::fstatat(self.fd.as_raw_fd(), creap.as_ptr(), &mut current, 0) } != 0 {
                return; // already gone
            }
            let matches = (widen_dev(current.st_dev), widen_ino(current.st_ino))
                == (widen_dev(own.st_dev), widen_ino(own.st_ino));
            if !matches {
                // A bystander occupies the isolated name: restore it to its
                // original location, untouched, and abandon the removal.
                let _ = unsafe {
                    libc::renameat(
                        self.fd.as_raw_fd(),
                        creap.as_ptr(),
                        self.fd.as_raw_fd(),
                        cname.as_ptr(),
                    )
                };
                return;
            }
            // Identity verified — this is OUR file. Give the deterministic
            // seam a chance to swap a bystander in after the check; in that
            // case the removal is abandoned and whatever occupies the
            // isolated name is restored to its original location.
            if faults.on_before_removal(&reap_name).is_err() {
                let _ = unsafe {
                    libc::renameat(
                        self.fd.as_raw_fd(),
                        creap.as_ptr(),
                        self.fd.as_raw_fd(),
                        cname.as_ptr(),
                    )
                };
                return;
            }
            // Safe: unlink of the PRIVATE reap name, verified to refer to
            // the inode we created (pinned by `owned`).
            let _ = unsafe { libc::unlinkat(self.fd.as_raw_fd(), creap.as_ptr(), 0) };
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

    #[cfg(test)]
    mod identity_tests {
        use super::*;

        fn widened_stat(path: &Path) -> FileIdentity {
            let cpath = cstring(path.as_os_str().as_bytes()).unwrap();
            let mut stat = unsafe { std::mem::zeroed() };
            // Safe: pure metadata read.
            assert_eq!(unsafe { libc::stat(cpath.as_ptr(), &mut stat) }, 0);
            (widen_dev(stat.st_dev), widen_ino(stat.st_ino))
        }

        /// Platform-neutral semantics: the same file always widens to the
        /// same `(u64, u64)` identity, and two distinct files never collapse
        /// into one — regardless of the platform's native `st_dev`/`st_ino`
        /// widths (macOS `i32`/`u64`, Linux `u64`/`u64`).
        #[test]
        fn widened_identity_is_stable_for_one_file_and_distinct_across_files() {
            let directory = tempfile::tempdir().unwrap();
            let a = directory.path().join("a");
            let b = directory.path().join("b");
            std::fs::write(&a, b"one").unwrap();
            std::fs::write(&b, b"two").unwrap();

            let identity_a_first = widened_stat(&a);
            let identity_a_again = widened_stat(&a);
            let identity_b = widened_stat(&b);

            assert_eq!(
                identity_a_first, identity_a_again,
                "the same file must widen to the same identity"
            );
            assert_ne!(
                identity_a_first, identity_b,
                "distinct files must never share a widened identity"
            );
        }
    }
}

// Named-temporary Unix platforms (macOS etc.): named sibling temporary file,
// rename-aside + identity-verified + seam-aware cleanup.
#[cfg(all(unix, not(target_os = "linux")))]
fn publish_locked_handle(
    canonical_root: &Path,
    target: &Path,
    contents: &str,
    temp_name: &str,
    faults: &dyn PlanDraftFaults,
) -> Result<(), String> {
    use std::io::Write as _;

    /// Fails a stage: clean up the created temporary while its descriptor is
    /// still open (pinned inode), then propagate the stage error.
    macro_rules! fault_fail {
        ($handle:ident, $created:ident, $file:ident, $stage:expr, $message:expr) => {{
            if let Some(created) = $created.take() {
                if let Some(open) = $file.take() {
                    $handle.remove_isolated(&created.name, &open, faults);
                    // `open` is dropped here — only after the cleanup
                    // decision has been made under the pinned inode.
                }
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
    let mut file = Some(handle.create_temp(temp_name)?);
    let mut created = Some(CreatedTemp {
        name: temp_name.to_owned(),
    });

    if let Err(message) = faults.on_stage(PlanDraftStage::Write) {
        fault_fail!(handle, created, file, PlanDraftStage::Write, message);
    }
    if let Err(error) = file.as_mut().unwrap().write_all(contents.as_bytes()) {
        fault_fail!(
            handle,
            created,
            file,
            PlanDraftStage::Write,
            error.to_string()
        );
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Flush) {
        fault_fail!(handle, created, file, PlanDraftStage::Flush, message);
    }
    if let Err(error) = file.as_mut().unwrap().flush() {
        fault_fail!(
            handle,
            created,
            file,
            PlanDraftStage::Flush,
            error.to_string()
        );
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::FileSync) {
        fault_fail!(handle, created, file, PlanDraftStage::FileSync, message);
    }
    if let Err(error) = file.as_mut().unwrap().sync_all() {
        fault_fail!(
            handle,
            created,
            file,
            PlanDraftStage::FileSync,
            error.to_string()
        );
    }
    // The descriptor stays OPEN through the second checks and any failure
    // cleanup: a still-open descriptor pins the inode, so the cleanup
    // identity comparison can never be fooled by immediate inode reuse.

    // (7) repeat both checks against the pinned handle, then commit with a
    // same-directory renameat — the atomic publication point.
    if let Err(message) = faults.on_stage(PlanDraftStage::SecondParentCheck) {
        fault_fail!(
            handle,
            created,
            file,
            PlanDraftStage::SecondParentCheck,
            message
        );
    }
    if let Err(error) = handle.verify_matches_path(target) {
        fault_fail!(
            handle,
            created,
            file,
            PlanDraftStage::SecondParentCheck,
            error
        );
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::SecondDestinationCheck) {
        fault_fail!(
            handle,
            created,
            file,
            PlanDraftStage::SecondDestinationCheck,
            message
        );
    }
    if let Err(error) = verify_destination(target) {
        fault_fail!(
            handle,
            created,
            file,
            PlanDraftStage::SecondDestinationCheck,
            error
        );
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Rename) {
        fault_fail!(handle, created, file, PlanDraftStage::Rename, message);
    }
    if let Err(error) = handle.rename_over(temp_name, PLAN_FILE_NAME) {
        fault_fail!(handle, created, file, PlanDraftStage::Rename, error);
    }
    // The temporary is now the published plan: close the descriptor and
    // leave nothing to clean.
    drop(file.take());

    // (8) sync the pinned directory before releasing the lock. The rename
    // has already committed the new plan; a directory-sync failure is
    // reported to the caller and never silently swallowed.
    if let Err(message) = faults.on_stage(PlanDraftStage::DirSync) {
        return Err(stage_error(PlanDraftStage::DirSync, message));
    }
    handle.sync()
}

// Linux: NAMELESS temporary file (`O_TMPFILE`). There is no directory entry
// for the whole lifetime of the temporary — nothing for a bystander to
// collide with or to be swapped into, and cleanup is just closing the
// descriptor: no `unlink` on any path, ever.
#[cfg(target_os = "linux")]
fn publish_locked_nameless(
    canonical_root: &Path,
    target: &Path,
    contents: &str,
    _temp_name: &str,
    faults: &dyn PlanDraftFaults,
) -> Result<(), String> {
    use std::io::Write as _;

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

    // (6) nameless temporary file, user-only permissions, anchored to the
    // pinned handle. Cleanup for every failure below is `close`: the file has
    // no directory entry, so there is nothing to unlink and no bystander can
    // ever be affected.
    if let Err(message) = faults.on_stage(PlanDraftStage::CreateTemp) {
        return Err(stage_error(PlanDraftStage::CreateTemp, message));
    }
    let mut file = Some(handle.create_nameless_temp()?);

    if let Err(message) = faults.on_stage(PlanDraftStage::Write) {
        return Err(stage_error(PlanDraftStage::Write, message));
    }
    if let Err(error) = file.as_mut().unwrap().write_all(contents.as_bytes()) {
        return Err(stage_error(PlanDraftStage::Write, error.to_string()));
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Flush) {
        return Err(stage_error(PlanDraftStage::Flush, message));
    }
    if let Err(error) = file.as_mut().unwrap().flush() {
        return Err(stage_error(PlanDraftStage::Flush, error.to_string()));
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::FileSync) {
        return Err(stage_error(PlanDraftStage::FileSync, message));
    }
    if let Err(error) = file.as_mut().unwrap().sync_all() {
        return Err(stage_error(PlanDraftStage::FileSync, error.to_string()));
    }

    // (7) repeat both checks against the pinned handle, then commit.
    if let Err(message) = faults.on_stage(PlanDraftStage::SecondParentCheck) {
        return Err(stage_error(PlanDraftStage::SecondParentCheck, message));
    }
    if let Err(error) = handle.verify_matches_path(target) {
        return Err(stage_error(PlanDraftStage::SecondParentCheck, error));
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::SecondDestinationCheck) {
        return Err(stage_error(PlanDraftStage::SecondDestinationCheck, message));
    }
    if let Err(error) = verify_destination(target) {
        return Err(stage_error(PlanDraftStage::SecondDestinationCheck, error));
    }
    if let Err(message) = faults.on_stage(PlanDraftStage::Rename) {
        return Err(stage_error(PlanDraftStage::Rename, message));
    }
    // Identity-bound atomic publication: link the nameless inode under a
    // fresh private name (the linkat syscall itself cannot be redirected by
    // any path swap), then atomically move it over plan.md. On any failure
    // the descriptor is simply closed — no unlink of any path.
    let middle_name = temp_file_name(&format!("link-{}", unique_nonce()));
    if let Err(error) =
        handle.publish_by_identity(file.as_ref().unwrap(), &middle_name, PLAN_FILE_NAME)
    {
        // If the intermediate link still exists, isolate and verify it
        // before removal — the same abandon-on-doubt semantics as the
        // named-temporary platforms.
        handle.remove_isolated(&middle_name, file.as_ref().unwrap(), faults);
        return Err(error);
    }
    // The nameless inode is now the published plan: close the descriptor.
    drop(file.take());

    // (8) sync the pinned directory before releasing the lock. The commit
    // has already happened; a directory-sync failure is reported to the
    // caller and never silently swallowed.
    if let Err(message) = faults.on_stage(PlanDraftStage::DirSync) {
        return Err(stage_error(PlanDraftStage::DirSync, message));
    }
    handle.sync()
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
    #[cfg(not(target_os = "linux"))]
    struct PinTempName(String);
    #[cfg(not(target_os = "linux"))]
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
    /// (Named-temporary platforms only: Linux uses O_TMPFILE and never
    /// creates a named temporary at all — its equivalent guarantee is
    /// covered by `publish_failure_leaves_arbitrary_bystanders_untouched`.)
    #[cfg(all(unix, not(target_os = "linux")))]
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

    /// A bystander occupying the pinned temporary path is a hard collision:
    /// the publication fails, and the bystander's content AND identity are
    /// preserved byte-for-byte (Windows path-based implementation).
    #[cfg(not(unix))]
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

            let error =
                plan_draft_with_faults(&locks, &root, "replacement", &PinTempName(name.clone()))
                    .await
                    .unwrap_err();
            assert!(
                error.contains("could not create temporary file"),
                "attempt {attempt}: {error}"
            );
            assert_eq!(
                std::fs::read(&bystander).unwrap(),
                b"innocent-bystander",
                "attempt {attempt}: bystander content must be preserved"
            );
            assert_eq!(
                std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
                b"previous"
            );
            assert_eq!(leftovers(&root), vec![name], "attempt {attempt}");
        }
    }

    /// Linux (O_TMPFILE): a bystander written anywhere in the workspace
    /// during the publication — including at any temporary-style path — is
    /// never touched by cleanup, because cleanup on Linux is closing the
    /// nameless descriptor: no `unlink` on any path, ever. Ten consecutive
    /// runs, as required by the acceptance gate.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn publish_failure_leaves_arbitrary_bystanders_untouched_ten_runs() {
        for attempt in 0..10 {
            let directory = temp_workspace("bystander");
            let root = directory.path().to_path_buf();
            let locks = FileLocks::new();
            plan_draft(&locks, &root, "previous").await.unwrap();

            let bystander_name = pinned_temp_name("bystander");
            struct PlantBystander(String);
            impl PlanDraftFaults for PlantBystander {
                fn on_stage(&self, stage: PlanDraftStage) -> Result<(), String> {
                    match stage {
                        // The nameless temporary exists; plant a bystander
                        // and fail the publication right after.
                        PlanDraftStage::Rename => {
                            std::fs::write(std::path::Path::new(&self.0), "innocent-bystander")
                                .unwrap();
                            Err("injected failure at Rename".into())
                        }
                        _ => Ok(()),
                    }
                }
            }

            let error = plan_draft_with_faults(
                &locks,
                &root,
                "replacement",
                &PlantBystander(root.join(&bystander_name).to_string_lossy().to_string()),
            )
            .await
            .unwrap_err();
            assert!(error.contains("Rename"), "attempt {attempt}: {error}");
            // The bystander survives byte-for-byte: cleanup closed the
            // nameless descriptor and never unlinked any path.
            assert_eq!(
                std::fs::read(root.join(&bystander_name)).unwrap(),
                b"innocent-bystander",
                "attempt {attempt}: the bystander must survive"
            );
            // The previous draft is intact; the only leftover is the
            // bystander itself.
            assert_eq!(
                std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
                b"previous",
                "attempt {attempt}"
            );
            assert_eq!(leftovers(&root), vec![bystander_name], "attempt {attempt}");
        }
    }

    /// The reviewer-mandated deterministic seam: a bystander swapped in
    /// AFTER the identity check succeeded and BEFORE the removal must be
    /// preserved. Named-temporary platforms implement this with the
    /// `on_before_removal` seam: the removal is safely abandoned and the
    /// isolated file (whatever occupies it at that instant) is restored to
    /// its original location. Ten consecutive runs.
    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn swap_after_the_identity_check_never_deletes_the_bystander_ten_runs() {
        for attempt in 0..10 {
            let directory = temp_workspace("temp-swap");
            let root = directory.path().to_path_buf();
            let locks = FileLocks::new();
            plan_draft(&locks, &root, "previous").await.unwrap();

            let name = pinned_temp_name("swap");
            struct SwapAfterCheck {
                workspace_root: PathBuf,
                temp_name: String,
            }
            impl PlanDraftFaults for SwapAfterCheck {
                fn on_stage(&self, stage: PlanDraftStage) -> Result<(), String> {
                    // Fail the publication at the commit stage so the
                    // failure-cleanup path (isolate → verify → remove) runs.
                    match stage {
                        PlanDraftStage::Rename => Err("injected failure at Rename".into()),
                        _ => Ok(()),
                    }
                }
                fn pinned_temp_name(&self) -> Option<String> {
                    Some(self.temp_name.clone())
                }
                fn on_before_removal(&self, isolated_path: &str) -> Result<(), String> {
                    // Exactly the reviewer's window: the identity check has
                    // passed, the removal has not happened yet. Swap a
                    // bystander in and abort the removal. The isolated path
                    // is handle-relative, so anchor it at the workspace root.
                    assert!(isolated_path.contains(".reap-"), "{isolated_path}");
                    let isolated = self.workspace_root.join(isolated_path);
                    std::fs::remove_file(&isolated).unwrap();
                    std::fs::write(&isolated, "swapped-in-bystander").unwrap();
                    Err("bystander swapped in after the identity check".into())
                }
            }

            let error = plan_draft_with_faults(
                &locks,
                &root,
                "replacement",
                &SwapAfterCheck {
                    workspace_root: root.clone(),
                    temp_name: name.clone(),
                },
            )
            .await
            .unwrap_err();
            assert!(error.contains("Rename"), "attempt {attempt}: {error}");
            // The swapped-in bystander was restored to its original
            // location, byte-for-byte.
            assert_eq!(
                std::fs::read(root.join(&name)).unwrap(),
                b"swapped-in-bystander",
                "attempt {attempt}: the swapped-in bystander must survive"
            );
            // The previous draft is intact and no temp residue remains
            // besides the restored bystander itself.
            assert_eq!(
                std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
                b"previous",
                "attempt {attempt}"
            );
            std::fs::remove_file(root.join(&name)).unwrap();
        }
    }

    /// The reviewer-mandated deterministic seam, Linux (O_TMPFILE) variant:
    /// a bystander swapped in after the identity-bound publication check
    /// must be preserved — cleanup closes the nameless descriptor and never
    /// unlinks any path, so there is nothing that could delete it. Ten
    /// consecutive runs.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn swap_after_the_identity_check_never_deletes_the_bystander_ten_runs() {
        for attempt in 0..10 {
            let directory = temp_workspace("temp-swap");
            let root = directory.path().to_path_buf();
            let locks = FileLocks::new();
            plan_draft(&locks, &root, "previous").await.unwrap();

            let bystander_name = pinned_temp_name("swap");
            struct SwapAfterCheck(String);
            impl PlanDraftFaults for SwapAfterCheck {
                fn on_stage(&self, stage: PlanDraftStage) -> Result<(), String> {
                    match stage {
                        // The identity checks have passed; swap a bystander
                        // in and abort the publication right before the
                        // identity-bound commit.
                        PlanDraftStage::Rename => {
                            std::fs::write(std::path::Path::new(&self.0), "swapped-in-bystander")
                                .unwrap();
                            Err("injected failure at Rename".into())
                        }
                        _ => Ok(()),
                    }
                }
            }

            let error = plan_draft_with_faults(
                &locks,
                &root,
                "replacement",
                &SwapAfterCheck(root.join(&bystander_name).to_string_lossy().to_string()),
            )
            .await
            .unwrap_err();
            assert!(error.contains("Rename"), "attempt {attempt}: {error}");
            // The swapped-in bystander survives byte-for-byte: cleanup
            // closed the nameless descriptor and never unlinked any path.
            assert_eq!(
                std::fs::read(root.join(&bystander_name)).unwrap(),
                b"swapped-in-bystander",
                "attempt {attempt}: the swapped-in bystander must survive"
            );
            assert_eq!(
                std::fs::read(root.join(PLAN_FILE_NAME)).unwrap(),
                b"previous",
                "attempt {attempt}"
            );
            assert_eq!(leftovers(&root), vec![bystander_name], "attempt {attempt}");
        }
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
