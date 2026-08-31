use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxProfile {
    #[default]
    Off,
    Workspace,
    ReadOnly,
}

#[derive(Clone, Debug)]
pub struct SandboxCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

pub fn wrap_shell_command(
    profile: SandboxProfile,
    cwd: &Path,
    command: &str,
) -> Result<SandboxCommand, String> {
    wrap_shell_command_with(profile, cwd, command, None)
}

pub fn wrap_shell_command_with(
    profile: SandboxProfile,
    cwd: &Path,
    command: &str,
    wrapper_override: Option<&Path>,
) -> Result<SandboxCommand, String> {
    if profile == SandboxProfile::Off {
        return Ok(native_shell(command));
    }

    #[cfg(target_os = "macos")]
    {
        let wrapper = wrapper_override
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/bin/sandbox-exec"));
        if !wrapper.is_file() {
            return Err(format!(
                "sandbox wrapper unavailable: {}",
                wrapper.display()
            ));
        }
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let escaped = cwd.to_string_lossy().replace('"', "\\\"");
        let policy = match profile {
            SandboxProfile::Workspace => format!(
                "(version 1) (allow default) (deny file-write*) (allow file-write* (subpath \"{escaped}\"))"
            ),
            SandboxProfile::ReadOnly => "(version 1) (allow default) (deny file-write*)".into(),
            SandboxProfile::Off => unreachable!(),
        };
        Ok(SandboxCommand {
            program: wrapper,
            args: vec![
                "-p".into(),
                policy,
                "bash".into(),
                "-lc".into(),
                command.into(),
            ],
        })
    }

    #[cfg(target_os = "linux")]
    {
        let wrapper = wrapper_override
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/bin/bwrap"));
        if !wrapper.is_file() {
            return Err(format!(
                "sandbox wrapper unavailable: {}",
                wrapper.display()
            ));
        }
        let mut args = vec![
            "--die-with-parent".into(),
            "--ro-bind".into(),
            "/".into(),
            "/".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
        ];
        if profile == SandboxProfile::Workspace {
            let c = cwd.to_string_lossy().into_owned();
            args.extend(["--bind".into(), c.clone(), c]);
        }
        args.extend([
            "--chdir".into(),
            cwd.to_string_lossy().into_owned(),
            "bash".into(),
            "-lc".into(),
            command.into(),
        ]);
        return Ok(SandboxCommand {
            program: wrapper,
            args,
        });
    }

    #[cfg(windows)]
    {
        // The executor must create a Restricted Token and Job Object. Refuse rather than
        // silently running unsandboxed until that native executor is selected.
        let _ = (cwd, command, wrapper_override);
        Err("windows Restricted Token sandbox executor required".into())
    }
}

fn native_shell(command: &str) -> SandboxCommand {
    #[cfg(windows)]
    {
        SandboxCommand {
            program: PathBuf::from(crate::default_shell()),
            args: vec!["-NoProfile".into(), "-Command".into(), command.into()],
        }
    }
    #[cfg(not(windows))]
    {
        SandboxCommand {
            program: PathBuf::from("bash"),
            args: vec!["-lc".into(), command.into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d1_5_missing_wrapper_refuses_without_off_fallback() {
        #[cfg(windows)]
        {
            let err = wrap_shell_command_with(
                SandboxProfile::Workspace,
                Path::new("."),
                "echo should-not-run",
                None,
            )
            .unwrap_err();
            assert!(err.contains("Restricted Token"));
            return;
        }
        let err = wrap_shell_command_with(
            SandboxProfile::Workspace,
            Path::new("."),
            "echo should-not-run",
            Some(Path::new("/definitely/missing/lato-sandbox")),
        )
        .unwrap_err();
        assert!(err.contains("unavailable"));
    }
}
