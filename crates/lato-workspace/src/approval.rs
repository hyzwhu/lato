use crate::{SandboxProfile, lock_key};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalMode {
    Ask,
    Auto,
    Always,
}

#[derive(Clone, Debug)]
pub struct SessionTrust {
    pub mode: ApprovalMode,
    pub persist_trust: bool,
    pub sandbox: SandboxProfile,
    process_trusted: Option<PathBuf>,
    cwd: PathBuf,
    approvals_once: Arc<AtomicUsize>,
}

impl SessionTrust {
    pub fn for_headless_prompt(cwd: impl AsRef<Path>) -> Self {
        let key = lock_key(cwd.as_ref());
        Self {
            mode: ApprovalMode::Always,
            persist_trust: false,
            sandbox: SandboxProfile::Off,
            process_trusted: Some(key.clone()),
            cwd: key,
            approvals_once: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn for_interactive(cwd: impl AsRef<Path>, trusted_on_disk: bool) -> Self {
        let key = lock_key(cwd.as_ref());
        Self {
            mode: ApprovalMode::Ask,
            persist_trust: true,
            sandbox: SandboxProfile::Off,
            process_trusted: trusted_on_disk.then(|| key.clone()),
            cwd: key,
            approvals_once: Arc::new(AtomicUsize::new(0)),
        }
    }
    pub fn for_interactive_auto(cwd: impl AsRef<Path>) -> Self {
        let mut trust = Self::for_interactive(cwd, true);
        trust.mode = ApprovalMode::Auto;
        trust.sandbox = SandboxProfile::Workspace;
        trust
    }

    pub fn cwd_trusted(&self) -> bool {
        self.process_trusted.as_ref() == Some(&self.cwd)
    }
    pub fn allow_once(&self) {
        self.approvals_once.fetch_add(1, Ordering::Release);
    }
    pub fn has_allow_once(&self) -> bool {
        self.approvals_once.load(Ordering::Acquire) > 0
    }
    pub fn consume_allow_once(&self) -> bool {
        self.approvals_once
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_ok()
    }
}

pub fn deny_write(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    name == ".env"
        || name == "id_rsa"
        || path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pem"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a0_1_headless_is_always_and_does_not_persist() {
        let t = SessionTrust::for_headless_prompt("/tmp/proj");
        assert_eq!(t.mode, ApprovalMode::Always);
        assert!(!t.persist_trust);
        assert!(t.cwd_trusted());
    }
    #[test]
    fn a0_2_interactive_defaults_ask() {
        let t = SessionTrust::for_interactive("/tmp/proj", false);
        assert_eq!(t.mode, ApprovalMode::Ask);
        assert!(!t.cwd_trusted());
        let t2 = SessionTrust::for_interactive("/tmp/proj", true);
        assert!(t2.cwd_trusted());
    }
    #[test]
    fn a0_3_deny_env() {
        assert!(deny_write(Path::new("/repo/.env")));
        assert!(deny_write(Path::new("/repo/secrets.pem")));
        assert!(deny_write(Path::new("/repo/id_rsa")));
        assert!(!deny_write(Path::new("/repo/src/lib.rs")));
    }
    #[test]
    fn g3_interactive_is_not_always() {
        assert_ne!(
            SessionTrust::for_interactive(".", false).mode,
            ApprovalMode::Always
        );
    }
}
