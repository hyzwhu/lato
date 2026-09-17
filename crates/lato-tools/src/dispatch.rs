use crate::{
    grep, list_dir, plan_draft, read_file, run_terminal_command_sandboxed, search_replace,
    todo_write, web_fetch, write_file,
};
use lato_workspace::{ApprovalMode, FileLocks, SessionTrust, deny_write};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

pub async fn dispatch(
    locks: &FileLocks,
    trust: &SessionTrust,
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
            let p = arg_path(cwd, &call.arguments, "path").unwrap_or_else(|_| cwd.to_path_buf());
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
        "write_file" | "write" => {
            let p = arg_path(cwd, &call.arguments, "path")?;
            if deny_write(&p) {
                return Err("denied by write policy".into());
            }
            require_mutating_approval(trust)?;
            let contents = call
                .arguments
                .get("contents")
                .or_else(|| call.arguments.get("content"))
                .and_then(|v| v.as_str())
                .ok_or("missing contents")?;
            write_file(locks, &p, contents).await.map(|_| "ok".into())
        }
        "search_replace" => {
            let p = arg_path(cwd, &call.arguments, "path")?;
            if deny_write(&p) {
                return Err("denied by write policy".into());
            }
            require_mutating_approval(trust)?;
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
            require_mutating_approval(trust)?;
            let cmd = call
                .arguments
                .get("cmd")
                .or_else(|| call.arguments.get("command"))
                .and_then(|v| v.as_str())
                .ok_or("missing command")?;
            run_terminal_command_sandboxed(cmd, cwd, trust.sandbox).await
        }
        "web_fetch" => {
            let url = call
                .arguments
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("missing url")?;
            web_fetch(url, 20_000).await
        }
        "plan_draft" => {
            let contents = call
                .arguments
                .get("contents")
                .and_then(|v| v.as_str())
                .ok_or("missing contents")?;
            plan_draft(locks, cwd, contents).await
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

pub fn requires_approval(name: &str) -> bool {
    matches!(
        name.strip_prefix("Lato:").unwrap_or(name),
        "search_replace" | "write_file" | "write" | "run_terminal_command"
    )
}

fn require_mutating_approval(trust: &SessionTrust) -> Result<(), String> {
    match trust.mode {
        ApprovalMode::Always | ApprovalMode::Auto => Ok(()),
        ApprovalMode::Ask if trust.consume_allow_once() => Ok(()),
        ApprovalMode::Ask => Err("permission required before mutating tool execution".into()),
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
    async fn a1_4_ask_mode_mutation_waits_for_explicit_allow_once() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("file.txt"), "before").unwrap();
        let locks = FileLocks::new();
        let trust = SessionTrust::for_interactive(d.path(), true);
        let call = || ToolCall {
            name: "search_replace".into(),
            arguments: serde_json::json!({"path":"file.txt","old":"before","new":"after"}),
        };
        let denied = dispatch(&locks, &trust, d.path(), call())
            .await
            .unwrap_err();
        assert!(denied.contains("permission required"));
        assert_eq!(
            std::fs::read_to_string(d.path().join("file.txt")).unwrap(),
            "before"
        );
        trust.allow_once();
        dispatch(&locks, &trust, d.path(), call()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("file.txt")).unwrap(),
            "after"
        );
    }

    #[tokio::test]
    async fn list_dir_defaults_to_workspace_when_path_is_missing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("file.txt"), "x").unwrap();
        let locks = FileLocks::new();
        let trust = SessionTrust::for_headless_prompt(d.path());
        let output = dispatch(
            &locks,
            &trust,
            d.path(),
            ToolCall {
                name: "list_dir".into(),
                arguments: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        assert!(output.contains("file.txt"), "{output}");
    }

    #[tokio::test]
    async fn write_file_creates_hello_world_and_denies_env() {
        let d = tempfile::tempdir().unwrap();
        let locks = FileLocks::new();
        let trust = SessionTrust::for_headless_prompt(d.path());
        dispatch(
            &locks,
            &trust,
            d.path(),
            ToolCall {
                name: "write_file".into(),
                arguments: serde_json::json!({
                    "path":"hello.go",
                    "contents":"package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"Hello, World!\")\n}\n"
                }),
            },
        )
        .await
        .unwrap();
        assert!(
            std::fs::read_to_string(d.path().join("hello.go"))
                .unwrap()
                .contains("Hello, World!")
        );
        let err = dispatch(
            &locks,
            &trust,
            d.path(),
            ToolCall {
                name: "write_file".into(),
                arguments: serde_json::json!({"path":".env","contents":"A=1"}),
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("denied"));
        assert!(!d.path().join(".env").exists());
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
