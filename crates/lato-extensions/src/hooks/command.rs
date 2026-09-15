use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time,
};
use tokio_util::sync::CancellationToken;

use super::{HookEventEnvelope, HookSpec, MAX_PAYLOAD_BYTES, MAX_RUNNER_OUTPUT_BYTES};

pub struct HookRunContext<'a> {
    pub session_id: &'a str,
    pub workspace_root: &'a Path,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct RawHookRun {
    pub stdout: String,
    pub stderr_preview: String,
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum HookRunError {
    #[error("hook event payload exceeds 128 KiB")]
    PayloadTooLarge,
    #[error("hook configuration is invalid")]
    InvalidConfiguration,
    #[error("hook command could not be started")]
    Spawn,
    #[error("hook command I/O failed")]
    Io,
    #[error("hook timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },
    #[error("hook was cancelled")]
    Cancelled,
    #[error("hook output exceeds 1 MiB")]
    OutputOverflow,
    #[error("hook URL is not allowed")]
    UnsafeUrl,
    #[error("hook request failed")]
    Http,
}

pub async fn run_command_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    context: &HookRunContext<'_>,
) -> Result<RawHookRun, HookRunError> {
    let payload = serde_json::to_vec(envelope).map_err(|_| HookRunError::PayloadTooLarge)?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(HookRunError::PayloadTooLarge);
    }
    let command = spec
        .command
        .as_deref()
        .ok_or(HookRunError::InvalidConfiguration)?;
    let mut process = build_command(command, &spec.source_dir)?;
    process
        .current_dir(context.workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&spec.extra_env)
        .env("LATO_HOOK_EVENT", spec.event.as_str())
        .env("LATO_HOOK_NAME", &spec.id)
        .env("LATO_SESSION_ID", context.session_id)
        .env("LATO_WORKSPACE_ROOT", context.workspace_root)
        .env("CLAUDE_PROJECT_DIR", context.workspace_root)
        .kill_on_drop(true);
    #[cfg(unix)]
    unsafe {
        process.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let mut child = process.spawn().map_err(|_| HookRunError::Spawn)?;
    let stdout = child.stdout.take().ok_or(HookRunError::Io)?;
    let stderr = child.stderr.take().ok_or(HookRunError::Io)?;
    let counter = Arc::new(AtomicUsize::new(0));
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout_task = tokio::spawn(read_bounded(
        stdout,
        Arc::clone(&counter),
        Arc::clone(&overflow),
    ));
    let stderr_task = tokio::spawn(read_bounded(
        stderr,
        Arc::clone(&counter),
        Arc::clone(&overflow),
    ));
    let mut stdin = child.stdin.take().ok_or(HookRunError::Io)?;
    let stdin_task = tokio::spawn(async move {
        // The hook may legitimately exit without reading its stdin; a failed
        // write to a closed pipe is not a hook failure.
        let _ = stdin.write_all(&payload).await;
        let _ = stdin.shutdown().await;
    });
    let timeout = Duration::from_millis(spec.timeout_ms);
    let status = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            terminate_tree(&mut child).await;
            return Err(HookRunError::Cancelled);
        }
        _ = time::sleep(timeout) => {
            terminate_tree(&mut child).await;
            return Err(HookRunError::Timeout { timeout_ms: spec.timeout_ms });
        }
        status = child.wait() => status.map_err(|_| HookRunError::Io)?,
    };
    stdin_task.await.map_err(|_| HookRunError::Io)?;
    let stdout = stdout_task
        .await
        .map_err(|_| HookRunError::Io)?
        .map_err(|_| HookRunError::Io)?;
    let stderr = stderr_task
        .await
        .map_err(|_| HookRunError::Io)?
        .map_err(|_| HookRunError::Io)?;
    if overflow.load(Ordering::Relaxed) {
        return Err(HookRunError::OutputOverflow);
    }
    Ok(RawHookRun {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr_preview: String::from_utf8_lossy(&stderr)
            .chars()
            .take(4096)
            .collect(),
        exit_code: status.code(),
        elapsed: started.elapsed(),
        truncated: false,
    })
}

fn build_command(command: &str, source_dir: &Path) -> Result<Command, HookRunError> {
    if is_simple_path(command) {
        let path = PathBuf::from(command);
        let path = if path.is_absolute() {
            path
        } else {
            source_dir.join(path)
        };
        let canonical_source =
            dunce::canonicalize(source_dir).map_err(|_| HookRunError::InvalidConfiguration)?;
        let canonical_path =
            dunce::canonicalize(path).map_err(|_| HookRunError::InvalidConfiguration)?;
        if !canonical_path.starts_with(&canonical_source) || !canonical_path.is_file() {
            return Err(HookRunError::InvalidConfiguration);
        }
        Ok(Command::new(canonical_path))
    } else {
        #[cfg(windows)]
        {
            // raw_arg hands the command line to cmd unescaped: std's
            // default MSVCRT quoting escapes quotes as \", which cmd then
            // passes through literally and corrupts any quoted argument.
            let mut process = Command::new("cmd");
            process.raw_arg("/C").raw_arg(command);
            Ok(process)
        }
        #[cfg(not(windows))]
        {
            let mut process = Command::new("/bin/sh");
            process.args(["-c", command]);
            Ok(process)
        }
    }
}

fn is_simple_path(command: &str) -> bool {
    !command.is_empty()
        && !command
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || b"|&;<>()$`\\\"'*!?[]{}".contains(&byte))
}

async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    counter: Arc<AtomicUsize>,
    overflow: Arc<AtomicBool>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let prior = counter.fetch_add(read, Ordering::Relaxed);
        if prior.saturating_add(read) > MAX_RUNNER_OUTPUT_BYTES {
            overflow.store(true, Ordering::Relaxed);
        } else {
            output.extend_from_slice(&buffer[..read]);
        }
    }
    Ok(output)
}

async fn terminate_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        if time::timeout(Duration::from_millis(500), child.wait())
            .await
            .is_ok()
        {
            return;
        }
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
}
