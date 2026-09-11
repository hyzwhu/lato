// Derived from: Lato hooks command runner process-group + terminate_tree patterns
// (crates/lato-extensions/src/hooks/command.rs) and MCP stdio session shape.
// License: Apache-2.0 (workspace)
// Lato changes: persistent JSON-RPC multiplex over stdin/stdout with cancel/timeout
// and SessionEnd-friendly process-group reap; no one-shot hot path.

//! Persistent stdio JSON-RPC MCP session.

use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
    time,
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::McpServerSpec,
    error::McpError,
    protocol::{self, JsonRpcResponse},
};

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

pub struct StdioSession {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    cancel: CancellationToken,
    closed: Arc<AtomicBool>,
    reader: tokio::task::JoinHandle<()>,
    pid: u32,
}

impl StdioSession {
    pub async fn start(spec: &McpServerSpec, cancel: CancellationToken) -> Result<Self, McpError> {
        let command = spec.command.as_deref().ok_or_else(|| {
            McpError::InvalidConfiguration("stdio server requires command".into())
        })?;
        let mut process = Command::new(command);
        process
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .envs(&spec.env);
        if let Some(cwd) = spec.cwd.as_deref() {
            process.current_dir(cwd);
        } else if !spec.source_dir.as_os_str().is_empty() {
            process.current_dir(&spec.source_dir);
        }
        #[cfg(unix)]
        unsafe {
            process.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = process.spawn().map_err(|_| McpError::Spawn)?;
        let pid = child.id().ok_or(McpError::Spawn)?;
        let stdin = child.stdin.take().ok_or(McpError::Io)?;
        let stdout = child.stdout.take().ok_or(McpError::Io)?;
        // Drain stderr so a full pipe cannot stall the child.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(_)) = lines.next_line().await {}
            });
        }
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reader = tokio::spawn(read_loop(
            stdout,
            Arc::clone(&pending),
            Arc::clone(&closed),
            cancel.child_token(),
        ));
        Ok(Self {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            next_id: AtomicU64::new(1),
            cancel,
            closed,
            reader,
            pid,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_alive(&mut self) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => process_signal_alive(self.pid),
            Ok(Some(_)) | Err(_) => false,
        }
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::ShutDown);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let payload = protocol::encode_line(&protocol::request(id, method, params))?;
        {
            let mut stdin = self.stdin.lock().await;
            if stdin.write_all(&payload).await.is_err() || stdin.flush().await.is_err() {
                self.pending.lock().await.remove(&id);
                return Err(McpError::Io);
            }
        }
        let timeout_ms = timeout.as_millis() as u64;
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Cancelled)
            }
            _ = time::sleep(timeout) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout { timeout_ms })
            }
            result = rx => {
                match result {
                    Ok(value) => value,
                    Err(_) => Err(McpError::Io),
                }
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::ShutDown);
        }
        let payload = protocol::encode_line(&protocol::notification(method, params))?;
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&payload).await.map_err(|_| McpError::Io)?;
        stdin.flush().await.map_err(|_| McpError::Io)?;
        Ok(())
    }

    pub async fn shutdown(mut self, deadline: std::time::Instant) -> Result<(), McpError> {
        self.closed.store(true, Ordering::SeqCst);
        // Closing stdin signals graceful exit for well-behaved servers.
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }
        fail_pending(&self.pending, McpError::ShutDown).await;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let wait = time::timeout(remaining, self.child.wait());
        match wait.await {
            Ok(Ok(_)) => {
                self.reader.abort();
                Ok(())
            }
            _ => {
                terminate_tree(&mut self.child).await;
                self.reader.abort();
                Ok(())
            }
        }
    }

    pub async fn kill_now(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        fail_pending(&self.pending, McpError::Unhealthy).await;
        terminate_tree(&mut self.child).await;
        self.reader.abort();
    }
}

async fn read_loop<R: tokio::io::AsyncRead + Unpin>(
    stdout: R,
    pending: PendingMap,
    closed: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        dispatch_line(&pending, &line).await;
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
    fail_pending(&pending, McpError::Io).await;
}

async fn dispatch_line(pending: &PendingMap, line: &str) {
    let Ok(response) = serde_json::from_str::<JsonRpcResponse>(line) else {
        return;
    };
    // Notifications / unmatched traffic are ignored.
    let Some(id_value) = response.id else {
        return;
    };
    let Some(id) = id_value.as_u64().or_else(|| {
        id_value
            .as_i64()
            .and_then(|value| u64::try_from(value).ok())
    }) else {
        return;
    };
    let Some(tx) = pending.lock().await.remove(&id) else {
        return;
    };
    let result = if let Some(error) = response.error {
        Err(McpError::rpc(error.code, error.message))
    } else {
        Ok(response.result.unwrap_or(Value::Null))
    };
    let _ = tx.send(result);
}

async fn fail_pending(pending: &PendingMap, error: McpError) {
    let mut guard = pending.lock().await;
    for (_, tx) in guard.drain() {
        let _ = tx.send(Err(match &error {
            McpError::ShutDown => McpError::ShutDown,
            McpError::Cancelled => McpError::Cancelled,
            McpError::Unhealthy => McpError::Unhealthy,
            _ => McpError::Io,
        }));
    }
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

fn process_signal_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Helper for fixtures: whether a pid (and optionally its process group) still exists.
pub fn pid_alive(pid: u32) -> bool {
    process_signal_alive(pid)
}

/// Best-effort: true when any process in the child's process group still exists.
pub fn process_group_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Negative pid targets the process group whose pgid == pid (setpgid(0,0) child).
        unsafe { libc::kill(-(pid as i32), 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        process_signal_alive(pid)
    }
}
