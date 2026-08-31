use std::path::Path;

pub async fn run_terminal_command(cmd: &str, cwd: &Path) -> Result<String, String> {
    let mut command;
    #[cfg(windows)]
    {
        command = tokio::process::Command::new(lato_workspace::default_shell());
        command.args(["-NoProfile", "-Command", cmd]);
    }
    #[cfg(not(windows))]
    {
        command = tokio::process::Command::new("bash");
        command.args(["-lc", cmd]);
    }
    let out = command
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let mut s = String::new();
    s.push_str(&String::from_utf8_lossy(&out.stdout));
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    if s.len() > 20_000 {
        s.truncate(20_000);
    }
    if out.status.success() { Ok(s) } else { Err(s) }
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
}
