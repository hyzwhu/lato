use lato_core::SandboxObligation;
use lato_workspace::{
    HostSandboxBackend, SandboxBackend, SandboxCommand, SandboxProfile, wrap_shell_command,
};
use std::path::Path;
use std::process::Stdio;
use tokio_util::sync::CancellationToken;

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
    spawn_wrapped(wrapped, cwd, None, None).await
}

pub async fn run_terminal_command_with_obligation(
    cmd: &str,
    cwd: &Path,
    obligation: &SandboxObligation,
) -> Result<String, String> {
    run_terminal_command_with_backend(cmd, cwd, obligation, &HostSandboxBackend::new()).await
}

pub async fn run_terminal_command_with_backend(
    cmd: &str,
    cwd: &Path,
    obligation: &SandboxObligation,
    backend: &dyn SandboxBackend,
) -> Result<String, String> {
    let wrapped = backend
        .prepare(obligation, cmd)
        .map_err(|error| format!("{}: {error}", error.code()))?;
    spawn_wrapped(wrapped, cwd, Some(obligation), None).await
}

pub async fn run_terminal_command_with_obligation_cancellable(
    cmd: &str,
    cwd: &Path,
    obligation: &SandboxObligation,
    cancellation: CancellationToken,
) -> Result<String, String> {
    let wrapped = HostSandboxBackend::new()
        .prepare(obligation, cmd)
        .map_err(|error| format!("{}: {error}", error.code()))?;
    spawn_wrapped(wrapped, cwd, Some(obligation), Some(&cancellation)).await
}

async fn spawn_wrapped(
    wrapped: SandboxCommand,
    cwd: &Path,
    obligation: Option<&SandboxObligation>,
    cancellation: Option<&CancellationToken>,
) -> Result<String, String> {
    let mut command = tokio::process::Command::new(&wrapped.program);
    command
        .args(&wrapped.args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    if let Some(obligation) = obligation {
        command.env_clear();
        for (key, value) in child_environment(obligation) {
            command.env(key, value);
        }
    }
    let child = command
        .spawn()
        .map_err(|e| format!("sandbox launch failed: {e}"))?;
    let process_id = child.id();
    let mut wait = Box::pin(child.wait_with_output());
    let out = if let Some(cancellation) = cancellation {
        tokio::select! {
            output = &mut wait => output,
            _ = cancellation.cancelled() => {
                terminate_process_tree(process_id);
                if tokio::time::timeout(std::time::Duration::from_secs(2), &mut wait)
                    .await
                    .is_err()
                {
                    kill_process_tree(process_id);
                    #[cfg(unix)]
                    let _ = wait.await;
                }
                return Err("tool cancelled".into());
            }
        }
    } else {
        wait.await
    }
    .map_err(|e| format!("sandbox process failed: {e}"))?;
    let mut s = String::new();
    s.push_str(&String::from_utf8_lossy(&out.stdout));
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(s)
    } else {
        Err(format!("command failed ({:?}): {s}", out.status.code()))
    }
}

#[cfg(unix)]
fn terminate_process_tree(process_id: Option<u32>) {
    if let Some(process_id) = process_id.and_then(|id| i32::try_from(id).ok()) {
        // SAFETY: a negative PID targets the isolated process group created above.
        unsafe { libc::kill(-process_id, libc::SIGTERM) };
    }
}

#[cfg(not(unix))]
fn terminate_process_tree(_process_id: Option<u32>) {}

#[cfg(unix)]
fn kill_process_tree(process_id: Option<u32>) {
    if let Some(process_id) = process_id.and_then(|id| i32::try_from(id).ok()) {
        // SAFETY: a negative PID targets the isolated process group created above.
        unsafe { libc::kill(-process_id, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
fn kill_process_tree(_process_id: Option<u32>) {}

fn child_environment(obligation: &SandboxObligation) -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(key, _)| is_allowed_env(key, &obligation.environment.allowed_keys))
        .collect()
}

fn is_allowed_env(name: &str, extra: &[String]) -> bool {
    if is_secret_env_name(name) {
        return false;
    }
    const SAFE_ENV_KEYS: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "SHELL",
        "TZ",
        "PWD",
        "SystemRoot",
        "SYSTEMROOT",
        "windir",
        "WINDIR",
        "COMSPEC",
        "ComSpec",
        "PATHEXT",
        "USERPROFILE",
        "USERNAME",
        "HOMEDRIVE",
        "HOMEPATH",
        "OS",
        "SystemDrive",
    ];
    SAFE_ENV_KEYS
        .iter()
        .any(|key| key.eq_ignore_ascii_case(name))
        || extra
            .iter()
            .any(|key| key.eq_ignore_ascii_case(name) && !is_secret_env_name(key))
}

fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.contains("API_KEY")
        || upper.contains("AUTHORIZATION")
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_terminates_the_entire_shell_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let obligation = SandboxObligation::off(directory.path());
        let cancellation = CancellationToken::new();
        let child_cancellation = cancellation.clone();
        let cwd = directory.path().to_path_buf();
        let run = tokio::spawn(async move {
            run_terminal_command_with_obligation_cancellable(
                "sleep 30 & child=$!; printf '%s' \"$child\" > child.pid; wait",
                &cwd,
                &obligation,
                child_cancellation,
            )
            .await
        });
        let pid_path = directory.path().join("child.pid");
        let child_pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&pid_path)
                    && let Ok(pid) = value.parse::<i32>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancellation.cancel();
        assert_eq!(run.await.unwrap().unwrap_err(), "tool cancelled");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                // SAFETY: signal 0 only checks whether the captured child PID exists.
                if unsafe { libc::kill(child_pid, 0) } != 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background child survived process-group cancellation");
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
        // Two failure modes must refuse rather than fall back to native bash:
        //   1. wrapper missing (file not on disk)
        //   2. wrapper present but unusable (cannot set up user namespace, e.g.
        //      /proc/sys/kernel/unprivileged_userns_clone=0 on this host)
        // Only when the wrapper is genuinely usable do we exercise the workspace
        // happy path; that requires a host where bwrap can create user namespaces.
        let backend = lato_workspace::HostSandboxBackend::new();
        let readiness = backend.readiness(SandboxProfile::Workspace);
        match readiness {
            lato_workspace::SandboxReadiness::Ready => {
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
            lato_workspace::SandboxReadiness::Unavailable => {
                // The CLI / dispatch layer must surface a typed unavailable error
                // and must not run the command unsandboxed.
                let d = tempfile::tempdir().unwrap();
                let result = run_terminal_command_sandboxed(
                    "printf forbidden > forbidden.txt",
                    d.path(),
                    SandboxProfile::Workspace,
                )
                .await;
                let error = result.expect_err("unavailable sandbox must refuse");
                assert!(
                    error.contains("sandbox wrapper unavailable"),
                    "expected typed unavailable error, got: {error}"
                );
                assert!(
                    !d.path().join("forbidden.txt").exists(),
                    "command must not run unsandboxed when profile is unavailable"
                );
                // Backend prepare path must agree.
                let prepare_error = backend
                    .prepare(
                        &lato_core::SandboxObligation::workspace(d.path()),
                        "printf forbidden > forbidden.txt",
                    )
                    .expect_err("backend prepare must refuse when readiness is Unavailable");
                assert_eq!(prepare_error.code(), "sandbox.unavailable");
                assert!(!d.path().join("forbidden.txt").exists());
            }
            lato_workspace::SandboxReadiness::Unsupported => {
                // Linux production path never reports Unsupported; skip silently.
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn d1_3b_linux_unusable_wrapper_refuses_workspace_command() {
        // Pointing the backend at /bin/false simulates a wrapper binary that is on
        // disk but cannot actually execute a sandboxed command. This must surface
        // a typed unavailable error and must not silently run the command unsandboxed.
        let backend = lato_workspace::HostSandboxBackend::with_wrapper_override("/bin/false");
        let readiness = backend.readiness(SandboxProfile::Workspace);
        // /bin/false is a regular file, so readiness will report Ready or Unavailable
        // depending on whether the probe accepts it; either way prepare must refuse.
        let d = tempfile::tempdir().unwrap();
        let prepare_error = backend
            .prepare(
                &lato_core::SandboxObligation::workspace(d.path()),
                "printf forbidden > forbidden.txt",
            )
            .expect_err("prepare must refuse when wrapper is unusable");
        assert_eq!(prepare_error.code(), "sandbox.unavailable");
        assert!(!d.path().join("forbidden.txt").exists());
        let _ = readiness; // silence unused warning
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
