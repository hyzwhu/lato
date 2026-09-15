#[cfg(not(windows))]
use std::path::Component;
use std::path::{Path, PathBuf};

pub fn lock_key(path: impl AsRef<Path>) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(windows_lock_key_for_test(
            path.as_ref().to_string_lossy().as_ref(),
        ))
    }
    #[cfg(not(windows))]
    {
        unix_lock_key(path.as_ref())
    }
}

pub fn windows_lock_key_for_test(raw: &str) -> String {
    let mut s = raw.replace('/', "\\");
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    s.to_lowercase()
}

#[cfg(not(windows))]
fn unix_lock_key(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    normalize_components(&abs)
}

#[cfg(not(windows))]
fn normalize_components(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CwdGuard(PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[test]
    fn a3_1_windows_spellings_share_lock_key() {
        #[cfg(windows)]
        {
            let a = lock_key(r"C:\Work\App\src\Lib.rs");
            let b = lock_key(r"c:/work/app/src/lib.rs");
            let c = lock_key(r"\\?\C:\Work\App\src\Lib.rs");
            assert_eq!(a, b);
            assert_eq!(b, c);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                windows_lock_key_for_test(r"C:\Work\App\src\Lib.rs"),
                windows_lock_key_for_test(r"c:/work/app/src/lib.rs")
            );
            assert_eq!(
                windows_lock_key_for_test(r"c:/work/app/src/lib.rs"),
                windows_lock_key_for_test(r"\\?\C:\Work\App\src\Lib.rs")
            );
        }
    }

    // The relative/absolute equivalence relies on Unix lock-key semantics
    // (canonicalize); Windows keys are spelling-normalized instead and are
    // covered by a3_1/a3_4.
    #[cfg(not(windows))]
    #[test]
    fn a3_3_unix_relative_and_absolute() {
        let old = std::env::current_dir().unwrap();
        let _guard = CwdGuard(old);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let k1 = lock_key("a.txt");
        let k2 = lock_key(&file);
        assert_eq!(k1, k2);
    }

    #[test]
    fn a3_4_windows_folder_trust_spellings_share_key() {
        assert_eq!(
            windows_lock_key_for_test(r"C:\Work\App"),
            windows_lock_key_for_test(r"c:/work/app")
        );
        assert_eq!(
            windows_lock_key_for_test(r"c:/work/app"),
            windows_lock_key_for_test(r"\\?\C:\Work\App")
        );
    }
}
