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
    #[cfg(windows)]
    if profile != SandboxProfile::Off {
        return run_windows_restricted(cmd, cwd, profile).await;
    }
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
    if out.status.success() {
        Ok(s)
    } else {
        Err(format!("command failed ({:?}): {s}", out.status.code()))
    }
}

#[cfg(windows)]
async fn run_windows_restricted(
    cmd: &str,
    cwd: &Path,
    profile: SandboxProfile,
) -> Result<String, String> {
    let program = lato_workspace::default_shell()
        .to_string_lossy()
        .into_owned();
    let args = vec!["-NoProfile".into(), "-Command".into(), cmd.into()];
    let cwd = cwd.to_path_buf();
    let lato_home = std::env::var_os("LATO_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| cwd.join(".lato"));
    let mut env = std::env::vars().collect::<std::collections::HashMap<_, _>>();
    env.insert(
        "ZAGENS_HOME".into(),
        lato_home.to_string_lossy().into_owned(),
    );
    let writable_roots = if profile == SandboxProfile::Workspace {
        vec![cwd.clone()]
    } else {
        vec![]
    };
    let result = tokio::task::spawn_blocking(move || {
        let plan = zagens_windows_sandbox::plan_exec(zagens_windows_sandbox::PlanInput {
            program,
            args,
            cwd: cwd.clone(),
            env,
            writable_roots,
            protected_write_paths: vec![lato_home],
            network_allowed: true,
            mode: zagens_windows_sandbox::WindowsSandboxMode::Unelevated,
            private_desktop: false,
            tty: false,
        })
        .map_err(|e| e.to_string())?;
        zagens_windows_sandbox::spawn_sync(&plan, None, Some(std::time::Duration::from_secs(120)))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    let mut output = result.stdout;
    output.push_str(&result.stderr);
    if result.exit_code == 0 {
        Ok(output)
    } else {
        Err(format!(
            "sandboxed command failed ({}): {output}",
            result.exit_code
        ))
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

    #[cfg(windows)]
    #[tokio::test]
    async fn d1_4_windows_restricted_token_workspace_write_isolation() {
        let d = tempfile::tempdir().unwrap();
        run_terminal_command_sandboxed(
            "Set-Content -NoNewline -Path inside.txt -Value ok",
            d.path(),
            SandboxProfile::Workspace,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("inside.txt")).unwrap(),
            "ok"
        );
        let outside = std::env::temp_dir().join(format!("lato-outside-{}", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        let command = format!("Set-Content -Path '{}' -Value forbidden", outside.display());
        assert!(
            run_terminal_command_sandboxed(&command, d.path(), SandboxProfile::Workspace)
                .await
                .is_err()
        );
        assert!(!outside.exists());
    }

    #[cfg(target_os = "macos")]
    fn dirs_home() -> std::path::PathBuf {
        std::env::var_os("HOME")
            .map(Into::into)
            .unwrap_or_else(|| "/tmp".into())
    }
}
