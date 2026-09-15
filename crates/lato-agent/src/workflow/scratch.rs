// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/workflow/host_service.rs
// License: Apache-2.0
// Lato changes: Phase 7B6 scratch-file quota helpers — single-path-component
// names, per-file/total/file-count caps, no symlink following; errors are the
// stable strings the journal prune tests match against.

use std::path::Path;

use lato_workflow::HostError;

pub const MAX_SCRATCH_NAME: usize = 128;
pub const MAX_SCRATCH_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_SCRATCH_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_SCRATCH_FILES: usize = 64;

/// `name` must be a single path component matching `^[A-Za-z0-9._-]{1,128}$`,
/// excluding `.` and `..`.
pub fn validate_scratch_name(name: &str) -> Result<(), HostError> {
    let valid = !name.is_empty()
        && name.len() <= MAX_SCRATCH_NAME
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    valid
        .then_some(())
        .ok_or_else(|| HostError::Failed("invalid scratch name".into()))
}

/// Sum of regular-file sizes and file count in the scratch dir; symlinks and
/// subdirectories are ignored (they can never be written through this API).
fn scratch_usage(dir: &Path) -> (u64, usize) {
    let mut bytes: u64 = 0;
    let mut files: usize = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_file() {
                continue;
            }
            bytes = bytes.saturating_add(meta.len());
            files += 1;
        }
    }
    (bytes, files)
}

/// Write `dir/<name>` atomically; returns the stable run-relative id
/// `scratch/{name}`. Overwriting an existing file charges only the delta.
pub fn write_scratch(dir: &Path, name: &str, content: &str) -> Result<String, HostError> {
    validate_scratch_name(name)?;
    let bytes = content.len() as u64;
    if bytes > MAX_SCRATCH_FILE_BYTES {
        return Err(HostError::Failed("scratch byte quota exceeded".into()));
    }
    let (mut total_bytes, mut files) = scratch_usage(dir);
    let target = dir.join(name);
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(HostError::Failed("invalid scratch name".into()));
        }
        total_bytes = total_bytes.saturating_sub(meta.len());
        files = files.saturating_sub(1);
    }
    if files.saturating_add(1) > MAX_SCRATCH_FILES {
        return Err(HostError::Failed("scratch file quota exceeded".into()));
    }
    if total_bytes.saturating_add(bytes) > MAX_SCRATCH_TOTAL_BYTES {
        return Err(HostError::Failed("scratch byte quota exceeded".into()));
    }
    std::fs::create_dir_all(dir)
        .map_err(|error| HostError::Failed(format!("scratch dir: {error}")))?;
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, content)
        .map_err(|error| HostError::Failed(format!("scratch write: {error}")))?;
    std::fs::rename(&tmp, &target)
        .map_err(|error| HostError::Failed(format!("scratch write: {error}")))?;
    Ok(format!("scratch/{name}"))
}

/// Read back `dir/<name>`; missing files, symlinks, and non-regular files are
/// reported as not found with the stable message `scratch file not found: …`.
pub fn read_scratch(dir: &Path, name: &str) -> Result<String, HostError> {
    validate_scratch_name(name)?;
    let path = dir.join(name);
    let not_found = || HostError::Failed(format!("scratch file not found: {name}"));
    let meta = std::fs::symlink_metadata(&path).map_err(|_| not_found())?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(not_found());
    }
    if meta.len() > MAX_SCRATCH_FILE_BYTES {
        return Err(HostError::Failed("scratch byte quota exceeded".into()));
    }
    let body = std::fs::read(&path).map_err(|_| not_found())?;
    String::from_utf8(body).map_err(|_| HostError::Failed(format!("invalid scratch file: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_path_traversal_and_bad_chars() {
        assert!(validate_scratch_name("report.md").is_ok());
        assert!(validate_scratch_name("a-b_c.d").is_ok());
        for bad in ["", "..", ".", "../x", "a/b", "a\\b", "a b", "a\0b", &"x".repeat(MAX_SCRATCH_NAME + 1)] {
            assert!(
                matches!(validate_scratch_name(bad), Err(HostError::Failed(message)) if message == "invalid scratch name"),
                "expected reject: {bad}"
            );
        }
    }

    #[test]
    fn write_then_read_round_trips_with_stable_id() {
        let dir = tempfile::tempdir().unwrap();
        let id = write_scratch(dir.path(), "report.md", "hello body").unwrap();
        assert_eq!(id, "scratch/report.md");
        assert_eq!(read_scratch(dir.path(), "report.md").unwrap(), "hello body");
        // Atomic temp file is gone after the write.
        assert!(!dir.path().join(".report.md.tmp").exists());
    }

    #[test]
    fn overwrite_charges_only_the_delta() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(512 * 1024);
        write_scratch(dir.path(), "a.txt", &big).unwrap();
        // Same file rewritten repeatedly without hitting the total cap.
        for _ in 0..3 {
            write_scratch(dir.path(), "a.txt", &big).unwrap();
        }
        assert_eq!(read_scratch(dir.path(), "a.txt").unwrap(), big);
    }

    #[test]
    fn oversize_single_file_and_total_cap_fail_with_stable_message() {
        let dir = tempfile::tempdir().unwrap();
        let one_mib_plus = "x".repeat(MAX_SCRATCH_FILE_BYTES as usize + 1);
        assert!(
            matches!(
                write_scratch(dir.path(), "big.txt", &one_mib_plus),
                Err(HostError::Failed(message)) if message == "scratch byte quota exceeded"
            )
        );

        // Fill the 8 MiB total with exactly 8 × 1 MiB files, then one more fails.
        let one_mib = "x".repeat(MAX_SCRATCH_FILE_BYTES as usize);
        for seq in 0..8 {
            write_scratch(dir.path(), &format!("f{seq}.txt"), &one_mib).unwrap();
        }
        assert!(
            matches!(
                write_scratch(dir.path(), "c.txt", "tiny"),
                Err(HostError::Failed(message)) if message == "scratch byte quota exceeded"
            )
        );
    }

    #[test]
    fn file_count_cap_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        for seq in 0..MAX_SCRATCH_FILES {
            write_scratch(dir.path(), &format!("f{seq:03}.txt"), "x").unwrap();
        }
        assert!(
            matches!(
                write_scratch(dir.path(), "overflow.txt", "x"),
                Err(HostError::Failed(message)) if message == "scratch file quota exceeded"
            )
        );
        // Overwrite of an existing name still works at the cap.
        assert!(write_scratch(dir.path(), "f000.txt", "y").is_ok());
    }

    #[test]
    fn missing_or_nonregular_reads_report_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            matches!(
                read_scratch(dir.path(), "missing.md"),
                Err(HostError::Failed(message)) if message == "scratch file not found: missing.md"
            )
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../outside.txt", dir.path().join("link.txt")).unwrap();
            assert!(
                matches!(
                    read_scratch(dir.path(), "link.txt"),
                    Err(HostError::Failed(message)) if message == "scratch file not found: link.txt"
                )
            );
            assert!(
                matches!(
                    write_scratch(dir.path(), "link.txt", "x"),
                    Err(HostError::Failed(message)) if message == "invalid scratch name"
                )
            );
        }
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        assert!(
            matches!(
                read_scratch(dir.path(), "subdir"),
                Err(HostError::Failed(message)) if message == "scratch file not found: subdir"
            )
        );
    }
}
