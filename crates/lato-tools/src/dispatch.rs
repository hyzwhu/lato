use crate::{grep, list_dir, read_file, run_terminal_command, search_replace, todo_write};
use lato_workspace::{FileLocks, SessionTrust, deny_write};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

pub async fn dispatch(
    locks: &FileLocks,
    _trust: &SessionTrust,
    cwd: &Path,
    call: ToolCall,
) -> Result<String, String> {
    let name = call.name.strip_prefix("Lato:").unwrap_or(&call.name);
    match name {
        "read_file" => {
            let p = arg_path(cwd, &call.arguments, "path")?;
            read_file(
                &p,
                val_usize(&call.arguments, "offset"),
                val_usize(&call.arguments, "limit"),
            )
            .await
        }
        "list_dir" => {
            let p = arg_path(cwd, &call.arguments, "path")?;
            Ok(serde_json::to_string(&list_dir(&p).await?).unwrap())
        }
        "grep" => {
            let root = arg_path(cwd, &call.arguments, "path")
                .or_else(|_| arg_path(cwd, &call.arguments, "root"))?;
            let pat = call
                .arguments
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or("missing pattern")?;
            Ok(serde_json::to_string(&grep(&root, pat)?).unwrap())
        }
        "search_replace" => {
            let p = arg_path(cwd, &call.arguments, "path")?;
            if deny_write(&p) {
                return Err("denied by write policy".into());
            }
            let old = call
                .arguments
                .get("old")
                .or_else(|| call.arguments.get("oldText"))
                .and_then(|v| v.as_str())
                .ok_or("missing old")?;
            let new = call
                .arguments
                .get("new")
                .or_else(|| call.arguments.get("newText"))
                .and_then(|v| v.as_str())
                .ok_or("missing new")?;
            search_replace(locks, &p, old, new)
                .await
                .map(|_| "ok".into())
        }
        "run_terminal_command" => {
            let cmd = call
                .arguments
                .get("cmd")
                .or_else(|| call.arguments.get("command"))
                .and_then(|v| v.as_str())
                .ok_or("missing command")?;
            run_terminal_command(cmd, cwd).await
        }
        "todo_write" => {
            let items: Vec<String> = call
                .arguments
                .get("items")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(ToString::to_string))
                        .collect()
                })
                .unwrap_or_default();
            Ok(todo_write(&items).to_string())
        }
        _ => Err(format!("unknown tool {name}")),
    }
}

fn arg_path(cwd: &Path, v: &Value, key: &str) -> Result<PathBuf, String> {
    let raw = v
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing {key}"))?;
    let p = PathBuf::from(raw);
    Ok(if p.is_absolute() { p } else { cwd.join(p) })
}
fn val_usize(v: &Value, key: &str) -> Option<usize> {
    v.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a0_3_dispatch_denies_env() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(".env");
        std::fs::write(&p, "A=1").unwrap();
        let locks = FileLocks::new();
        let trust = SessionTrust::for_headless_prompt(d.path());
        let err = dispatch(
            &locks,
            &trust,
            d.path(),
            ToolCall {
                name: "search_replace".into(),
                arguments: serde_json::json!({"path":".env","old":"1","new":"2"}),
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("denied"));
        assert_eq!(std::fs::read_to_string(p).unwrap(), "A=1");
    }

    #[tokio::test]
    async fn d1_6_host_read_and_edit_do_not_enter_shell_sandbox() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("source.txt"), "before").unwrap();
        let locks = FileLocks::new();
        let trust = SessionTrust::for_headless_prompt(d.path());
        let read = dispatch(
            &locks,
            &trust,
            d.path(),
            ToolCall {
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"source.txt"}),
            },
        )
        .await
        .unwrap();
        assert_eq!(read, "before");
        dispatch(
            &locks,
            &trust,
            d.path(),
            ToolCall {
                name: "search_replace".into(),
                arguments: serde_json::json!({"path":"source.txt","old":"before","new":"after"}),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("source.txt")).unwrap(),
            "after"
        );
    }
}
