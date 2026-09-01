use lato_core::{PolicyDenial, SandboxObligation, SandboxProfile};
use std::path::{Component, Path, PathBuf};

pub fn validate_sandbox_obligation(obligation: &SandboxObligation) -> Result<(), PolicyDenial> {
    match obligation.profile {
        SandboxProfile::Off => Ok(()),
        SandboxProfile::ReadOnly => {
            if obligation.writable_roots.is_empty() {
                Ok(())
            } else {
                Err(PolicyDenial::new(
                    "sandbox.unsupported",
                    "read-only sandbox cannot grant writable roots",
                ))
            }
        }
        SandboxProfile::Workspace => {
            for root in &obligation.writable_roots {
                if !writable_root_is_inside_workspace(&obligation.workspace_root, root) {
                    return Err(PolicyDenial::new(
                        "sandbox.unsupported",
                        "writable root is outside the workspace",
                    ));
                }
            }
            Ok(())
        }
    }
}

fn writable_root_is_inside_workspace(workspace: &Path, root: &Path) -> bool {
    let workspace = lexical_normalize(workspace);
    let root = lexical_normalize(root);
    root.starts_with(&workspace)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_accepted_when_profile_is_off() {
        assert!(validate_sandbox_obligation(&SandboxObligation::off("/workspace")).is_ok());
    }

    #[test]
    fn read_only_rejects_writable_roots() {
        let mut obligation = SandboxObligation::read_only("/workspace");
        obligation.writable_roots = vec![PathBuf::from("/workspace")];
        let error = validate_sandbox_obligation(&obligation).unwrap_err();
        assert_eq!(error.code, "sandbox.unsupported");
    }

    #[test]
    fn workspace_rejects_outside_roots() {
        let mut obligation = SandboxObligation::workspace("/workspace");
        obligation.writable_roots = vec![PathBuf::from("/tmp/outside")];
        let error = validate_sandbox_obligation(&obligation).unwrap_err();
        assert_eq!(error.code, "sandbox.unsupported");
    }
}
