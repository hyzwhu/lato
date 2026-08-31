use lato_workspace::{SandboxProfile, wrap_shell_command};
use std::path::Path;

pub async fn run_terminal_command(cmd: &str, cwd: &Path) -> Result<String, String> {
    run_terminal_command_sandboxed(cmd, cwd, SandboxProfile::Off).await
}

pub async fn run_terminal_command_sandboxed(
    cmd: &str,
    cwd: &Path,
    profile: SandboxProfile,
) -> Result<String, String> {
    let wrapped = wrap_shell_command(profile, cwd, cmd)?;
    let out = tokio::process::Command::new(&wrapped.program)
        .args(&wrapped.args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| format!("sandbox launch failed: {e}"))?;
    let mut s = String::new();
    s.push_str(&String::from_utf8_lossy(&out.stdout));
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    if s.len() > 20_000 {
        s.truncate(20_000);
    }
    if out.status.success() {
        Ok(s)
    } else {
        Err(format!("command failed ({:?}): {s}", out.status.code()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shell_echo_hello() {
        let d = tempfile::tempdir().unwrap();
        let out = run_terminal_command("echo hello", d.path()).await.unwrap();
        assert!(out.contains("hello"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn d1_1_macos_workspace_shell_writes_cwd() {
        let d = tempfile::tempdir().unwrap();
        run_terminal_command_sandboxed(
            "printf ok > inside.txt",
            d.path(),
            SandboxProfile::Workspace,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("inside.txt")).unwrap(),
            "ok"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn d1_2_macos_workspace_shell_denies_outside_cwd_without_retry() {
        let d = tempfile::tempdir().unwrap();
        let outside = dirs_home().join(format!("lato-sandbox-outside-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let cmd = format!("printf forbidden > '{}'", outside.display());
        let result =
            run_terminal_command_sandboxed(&cmd, d.path(), SandboxProfile::Workspace).await;
        assert!(result.is_err());
        assert!(!outside.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn d1_3_linux_bwrap_workspace_policy() {
        if !Path::new("/usr/bin/bwrap").is_file() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        run_terminal_command_sandboxed(
            "printf ok > inside.txt",
            d.path(),
            SandboxProfile::Workspace,
        )
        .await
        .unwrap();
        assert!(d.path().join("inside.txt").exists());
    }

    #[cfg(target_os = "macos")]
    fn dirs_home() -> std::path::PathBuf {
        std::env::var_os("HOME")
            .map(Into::into)
            .unwrap_or_else(|| "/tmp".into())
    }
}
