use lato_core::{SandboxObligation, SandboxProfile};
use lato_workspace::{HostSandboxBackend, SandboxBackend, SandboxReadiness};
use std::path::{Path, PathBuf};

fn workspace_obligation(root: &Path) -> SandboxObligation {
    SandboxObligation::workspace(root)
}

fn missing_wrapper_backend() -> HostSandboxBackend {
    HostSandboxBackend::with_wrapper_override(PathBuf::from("/definitely/missing/lato-sandbox"))
}

#[test]
fn production_windows_non_off_readiness_matches_prepare_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    let backend = HostSandboxBackend::new();
    let readiness = backend.readiness(SandboxProfile::Workspace);
    let prepare = backend.prepare(&workspace_obligation(temp.path()), "echo forbidden");
    #[cfg(windows)]
    {
        assert_eq!(readiness, SandboxReadiness::Unsupported);
        assert_eq!(prepare.unwrap_err().code(), "sandbox.unsupported");
        assert!(!temp.path().join("forbidden").exists());
    }
    #[cfg(not(windows))]
    match readiness {
        SandboxReadiness::Ready => {
            let _ = prepare;
        }
        SandboxReadiness::Unavailable => {
            assert_eq!(prepare.unwrap_err().code(), "sandbox.unavailable");
        }
        SandboxReadiness::Unsupported => {
            assert_eq!(prepare.unwrap_err().code(), "sandbox.unsupported");
        }
    }
}

#[test]
fn missing_wrapper_is_unavailable_and_does_not_run() {
    let temp = tempfile::tempdir().unwrap();
    let backend = missing_wrapper_backend();
    assert_eq!(
        backend.readiness(SandboxProfile::Workspace),
        SandboxReadiness::Unavailable
    );
    let error = backend
        .prepare(&workspace_obligation(temp.path()), "echo forbidden")
        .unwrap_err();
    assert_eq!(error.code(), "sandbox.unavailable");
    assert!(!temp.path().join("forbidden").exists());
}

#[test]
fn read_only_obligation_rejects_writable_roots() {
    let temp = tempfile::tempdir().unwrap();
    let mut obligation = SandboxObligation::read_only(temp.path());
    obligation.writable_roots = vec![temp.path().to_path_buf()];
    let error = missing_wrapper_backend()
        .prepare(&obligation, "echo forbidden")
        .unwrap_err();
    assert_eq!(error.code(), "sandbox.unsupported");
    assert!(!temp.path().join("forbidden").exists());
}

#[test]
fn workspace_obligation_rejects_root_outside_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let mut obligation = workspace_obligation(temp.path());
    obligation.writable_roots = vec![temp.path().join("..").join("outside-lato-root")];
    let error = missing_wrapper_backend()
        .prepare(&obligation, "echo forbidden")
        .unwrap_err();
    assert_eq!(error.code(), "sandbox.unsupported");
    assert!(!temp.path().join("forbidden").exists());
}

#[test]
fn off_is_accepted_only_when_obligation_profile_is_off() {
    let temp = tempfile::tempdir().unwrap();
    let backend = missing_wrapper_backend();
    assert_eq!(
        backend.readiness(SandboxProfile::Off),
        SandboxReadiness::Ready
    );
    backend
        .prepare(&SandboxObligation::off(temp.path()), "echo ok")
        .unwrap();

    let mut disguised = SandboxObligation::off(temp.path());
    disguised.profile = SandboxProfile::Workspace;
    let error = backend.prepare(&disguised, "echo forbidden").unwrap_err();
    assert_eq!(error.code(), "sandbox.unavailable");
    assert!(!temp.path().join("forbidden").exists());
}
