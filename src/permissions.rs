//! Session sandbox selection is independent of tool approval and folder trust.
use crate::{
    args::SandboxArg,
    tui::{dialog::Interaction, i18n::Language},
};
use lato_workspace::{ApprovalMode, SandboxProfile, SessionTrust};
use std::path::Path;

pub fn profile(arg: SandboxArg) -> SandboxProfile {
    match arg {
        SandboxArg::Off => SandboxProfile::Off,
        SandboxArg::Workspace => SandboxProfile::Workspace,
        SandboxArg::ReadOnly => SandboxProfile::ReadOnly,
    }
}

pub fn interactive_trust(cwd: &Path, trusted: bool, sandbox: SandboxArg) -> SessionTrust {
    let mut trust = if trusted {
        SessionTrust::for_interactive_auto(cwd)
    } else {
        SessionTrust::for_interactive(cwd, false)
    };
    trust.sandbox = profile(sandbox);
    trust
}

pub async fn select_sandbox(
    ui: &Interaction,
    language: Language,
    requested: Option<SandboxArg>,
) -> Result<SandboxArg, String> {
    if let Some(profile) = requested {
        return Ok(profile);
    }
    let choices = match language {
        Language::ZhCn => [
            "workspace — 仅允许写入当前工作区（默认）",
            "read-only — 只读，禁止写入文件",
            "off — 关闭沙箱，允许写入工作区外（包括上层目录）",
        ],
        Language::En => [
            "workspace — Write only inside this workspace (default)",
            "read-only — Read files; deny file writes",
            "off — No sandbox; allow writes outside the workspace",
        ],
    }
    .map(String::from);
    let selected = ui
        .choose(
            match language {
                Language::ZhCn => "选择本次会话的沙箱范围（审批不会扩大此范围）",
                Language::En => {
                    "Choose sandbox scope for this session (approval does not expand it)"
                }
            },
            &choices,
        )
        .await?;
    match choices.iter().position(|choice| *choice == selected) {
        Some(0) => Ok(SandboxArg::Workspace),
        Some(1) => Ok(SandboxArg::ReadOnly),
        Some(2) => Ok(SandboxArg::Off),
        _ => Err("invalid sandbox selection".into()),
    }
}

pub fn describe(trust: &SessionTrust, language: Language) -> String {
    let scope = match (language, trust.sandbox) {
        (Language::ZhCn, SandboxProfile::Off) => "off · 允许写入工作区外，受操作系统权限限制",
        (Language::ZhCn, SandboxProfile::Workspace) => "workspace · 仅允许写入当前工作区",
        (Language::ZhCn, SandboxProfile::ReadOnly) => "read-only · 禁止写入文件",
        (Language::En, SandboxProfile::Off) => {
            "off · Writes outside workspace allowed, subject to OS permissions"
        }
        (Language::En, SandboxProfile::Workspace) => {
            "workspace · Writes limited to current workspace"
        }
        (Language::En, SandboxProfile::ReadOnly) => "read-only · File writes denied",
    };
    let approval = match (language, trust.mode) {
        (Language::ZhCn, ApprovalMode::Ask) => "逐次审批修改和命令；审批不会扩大沙箱范围",
        (Language::ZhCn, _) => "自动审批；审批不会扩大沙箱范围",
        (Language::En, ApprovalMode::Ask) => {
            "Ask before mutations and commands; approval does not expand sandbox scope"
        }
        (Language::En, _) => "Automatic approval; approval does not expand sandbox scope",
    };
    match language {
        Language::ZhCn => format!("沙箱: {scope}\n审批: {approval}"),
        Language::En => format!("Sandbox: {scope}\nApproval: {approval}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_selection_does_not_change_approval_or_project_trust() {
        let dir = tempfile::tempdir().unwrap();
        for sandbox in [SandboxArg::Off, SandboxArg::Workspace, SandboxArg::ReadOnly] {
            for trusted in [true, false] {
                let trust = interactive_trust(dir.path(), trusted, sandbox);
                assert_eq!(trust.sandbox, profile(sandbox));
                assert_eq!(trust.cwd_trusted(), trusted);
                assert_eq!(
                    trust.mode,
                    if trusted {
                        ApprovalMode::Auto
                    } else {
                        ApprovalMode::Ask
                    }
                );
                assert!(trust.persist_trust);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_sandbox_is_enforced_by_real_tools_even_after_approval() {
        use lato_agent::{ApprovalRequest, PromptKind, SessionActor, ToolApproval};
        use lato_ai::{FakeModelStream, StreamPiece};
        use lato_workspace::FileLocks;
        use serde_json::json;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct Approve(AtomicUsize);
        #[async_trait::async_trait]
        impl ToolApproval for Approve {
            async fn approve(&self, _: &ApprovalRequest) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
        }

        for sandbox in [SandboxArg::Off, SandboxArg::Workspace, SandboxArg::ReadOnly] {
            for trusted in [true, false] {
                let root = tempfile::tempdir().unwrap();
                let cwd = root.path().join("workspace");
                std::fs::create_dir(&cwd).unwrap();
                let approval = Arc::new(Approve(AtomicUsize::new(0)));
                let stream = Arc::new(FakeModelStream::new(vec![
                    vec![StreamPiece::ToolCall {
                        id: "shell-sibling".into(),
                        name: "run_terminal_command".into(),
                        arguments: json!({"command": "mkdir -p ../abc && printf created > ../abc/marker"}),
                    }],
                    vec![StreamPiece::ToolCall {
                        id: "write-inside".into(),
                        name: "write_file".into(),
                        arguments: json!({"path": "inside.txt", "content": "inside"}),
                    }],
                    vec![StreamPiece::Text("done".into())],
                ]));
                let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
                let mut actor = SessionActor::new(
                    stream,
                    Arc::new(FileLocks::new()),
                    interactive_trust(&cwd, trusted, sandbox),
                    cwd.clone(),
                )
                .with_interactive_events(
                    events,
                    "permission-test".into(),
                    Some(approval.clone()),
                );
                actor
                    .prompt(PromptKind::Start, "Run the test operation".into())
                    .await
                    .unwrap();
                assert_eq!(
                    root.path().join("abc/marker").exists(),
                    sandbox == SandboxArg::Off,
                    "sibling write: {sandbox:?}, trusted={trusted}"
                );
                assert_eq!(
                    cwd.join("inside.txt").exists(),
                    sandbox != SandboxArg::ReadOnly,
                    "workspace write: {sandbox:?}, trusted={trusted}"
                );
                assert_eq!(approval.0.load(Ordering::SeqCst) > 0, !trusted);
            }
        }
    }
}
