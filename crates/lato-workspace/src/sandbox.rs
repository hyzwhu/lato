use lato_core::SandboxObligation;
use std::path::{Component, Path, PathBuf};

pub use lato_core::SandboxProfile;

#[derive(Clone, Debug)]
pub struct SandboxCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxReadiness {
    Ready,
    Unavailable,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct SandboxError {
    code: &'static str,
    message: String,
}

impl SandboxError {
    pub fn code(&self) -> &str {
        self.code
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "sandbox.unavailable",
            message: message.into(),
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: "sandbox.unsupported",
            message: message.into(),
        }
    }

    pub fn preparation_failed(message: impl Into<String>) -> Self {
        Self {
            code: "sandbox.preparation_failed",
            message: message.into(),
        }
    }
}

pub trait SandboxBackend: Send + Sync {
    fn readiness(&self, profile: SandboxProfile) -> SandboxReadiness;
    fn prepare(
        &self,
        obligation: &SandboxObligation,
        command: &str,
    ) -> Result<SandboxCommand, SandboxError>;
}

#[derive(Clone, Debug, Default)]
pub struct HostSandboxBackend {
    wrapper_override: Option<PathBuf>,
}

impl HostSandboxBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_wrapper_override(path: impl Into<PathBuf>) -> Self {
        Self {
            wrapper_override: Some(path.into()),
        }
    }
}

impl SandboxBackend for HostSandboxBackend {
    fn readiness(&self, profile: SandboxProfile) -> SandboxReadiness {
        if profile == SandboxProfile::Off {
            return SandboxReadiness::Ready;
        }
        let wrapper = self
            .wrapper_override
            .as_deref()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_wrapper);
        if !wrapper.is_file() {
            return SandboxReadiness::Unavailable;
        }
        platform_readiness(&wrapper)
    }

    fn prepare(
        &self,
        obligation: &SandboxObligation,
        command: &str,
    ) -> Result<SandboxCommand, SandboxError> {
        validate_host_obligation(obligation)?;
        if obligation.profile != SandboxProfile::Off
            && let Some(wrapper) = &self.wrapper_override
            && !wrapper.is_file()
        {
            return Err(SandboxError::unavailable(format!(
                "sandbox wrapper unavailable: {}",
                wrapper.display()
            )));
        }
        wrap_shell_command_with(
            obligation.profile,
            &obligation.workspace_root,
            command,
            self.wrapper_override.as_deref(),
        )
        .map_err(map_wrap_error)
    }
}

fn platform_readiness(wrapper: &Path) -> SandboxReadiness {
    #[cfg(windows)]
    {
        let _ = wrapper;
        SandboxReadiness::Unsupported
    }
    #[cfg(target_os = "linux")]
    {
        if probe_wrapper_usable(wrapper) {
            SandboxReadiness::Ready
        } else {
            SandboxReadiness::Unavailable
        }
    }
    #[cfg(all(not(windows), not(target_os = "linux")))]
    {
        let _ = wrapper;
        // macOS sandbox-exec ships with the OS and is always usable when present.
        // We deliberately do not probe it here so the macOS readiness check remains
        // identical to its historical file-existence semantics.
        SandboxReadiness::Ready
    }
}

#[cfg(target_os = "linux")]
fn probe_wrapper_usable(wrapper: &Path) -> bool {
    // Cached per-wrapper-path so repeated readiness/prepare calls don't fork bwrap
    // on every check. The cache key is the wrapper itself, so test overrides that
    // swap in a fake wrapper do not poison the default-wrapper cache.
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(usable) = cache.get(wrapper)
    {
        return *usable;
    }
    let usable = probe_bwrap(wrapper);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(wrapper.to_path_buf(), usable);
    }
    usable
}

#[cfg(target_os = "linux")]
fn probe_bwrap(wrapper: &Path) -> bool {
    // bwrap with --unshare-user requires a usable user namespace; on hosts where
    // /proc/sys/kernel/unprivileged_userns_clone=0 (or otherwise denied) the probe
    // fails with EPERM and we mark the wrapper Unavailable rather than silently
    // falling back to running the command unsandboxed.
    let probe_target = if Path::new("/bin/true").is_file() {
        Path::new("/bin/true")
    } else if Path::new("/usr/bin/true").is_file() {
        Path::new("/usr/bin/true")
    } else {
        return false;
    };
    std::process::Command::new(wrapper)
        .args(["--unshare-user", "--ro-bind", "/", "/", "--"])
        .arg(probe_target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn default_wrapper() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/usr/bin/sandbox-exec")
    }
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/usr/bin/bwrap")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        PathBuf::from("/definitely/missing/lato-sandbox")
    }
}

fn validate_host_obligation(obligation: &SandboxObligation) -> Result<(), SandboxError> {
    match obligation.profile {
        SandboxProfile::Off => Ok(()),
        SandboxProfile::ReadOnly => {
            if obligation.writable_roots.is_empty() {
                Ok(())
            } else {
                Err(SandboxError::unsupported(
                    "read-only sandbox cannot grant writable roots",
                ))
            }
        }
        SandboxProfile::Workspace => {
            for root in &obligation.writable_roots {
                if !writable_root_is_inside_workspace(&obligation.workspace_root, root) {
                    return Err(SandboxError::unsupported(
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

fn map_wrap_error(error: String) -> SandboxError {
    let lower = error.to_ascii_lowercase();
    if lower.contains("unavailable") {
        SandboxError::unavailable(error)
    } else if lower.contains("restricted token") || lower.contains("unsupported") {
        SandboxError::unsupported(error)
    } else {
        SandboxError::preparation_failed(error)
    }
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
        if !probe_bwrap(&wrapper) {
            return Err(format!(
                "sandbox wrapper unavailable: {} cannot set up user namespace (uid map permission denied); refusing to run unsandboxed",
                wrapper.display()
            ));
        }
        let mut args = vec![
            "--die-with-parent".into(),
            "--unshare-user-try".into(),
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
        Ok(SandboxCommand {
            program: wrapper,
            args,
        })
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
