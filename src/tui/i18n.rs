use clap::ValueEnum;

#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, serde::Deserialize, serde::Serialize,
)]
pub enum Language {
    #[value(name = "zh-CN", alias = "zh")]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[default]
    #[value(name = "en")]
    #[serde(rename = "en")]
    En,
}

impl Language {
    pub fn resolve(cli: Option<Self>, persisted: Option<Self>, system: Option<&str>) -> Self {
        cli.or(persisted).unwrap_or_else(|| {
            if system
                .unwrap_or_default()
                .to_ascii_lowercase()
                .starts_with("zh")
            {
                Self::ZhCn
            } else {
                Self::En
            }
        })
    }

    pub fn system() -> Option<String> {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextKey {
    WorkingDirectory,
    Model,
    ApiStatus,
    Online,
    StartPrompt,
    Sessions,
    NewSession,
    SwitchSession,
    ClearConversation,
    ToolCalls,
    ToolControls,
    WaitingTools,
    MessagePlaceholder,
    Responding,
    Stop,
    Path,
    SwitchPanel,
    Command,
    Cancel,
    Search,
    Thinking,
    Running,
    Done,
    Failed,
    ReturnValue,
    Resize,
    CommandPalette,
    SwitchLanguage,
    SwitchModel,
    Login,
    Status,
    ApproveOnce,
    Exit,
    Approve,
    Deny,
    Error,
    Ready,
    WelcomeHint,
}

pub fn tr(language: Language, key: TextKey) -> &'static str {
    use Language::{En, ZhCn};
    use TextKey::*;
    match (language, key) {
        (ZhCn, WorkingDirectory) => "工作目录",
        (En, WorkingDirectory) => "Working directory",
        (ZhCn, Model) => "模型名称",
        (En, Model) => "Model",
        (ZhCn, ApiStatus) => "API 状态",
        (En, ApiStatus) => "API status",
        (ZhCn, Online) => "在线",
        (En, Online) => "Online",
        (ZhCn, StartPrompt) => "在此输入提示以开始…",
        (En, StartPrompt) => "Enter a prompt to start…",
        (ZhCn, Sessions) => "会话",
        (En, Sessions) => "Sessions",
        (ZhCn, NewSession) => "新建会话",
        (En, NewSession) => "New session",
        (ZhCn, SwitchSession) => "切换会话",
        (En, SwitchSession) => "Switch session",
        (ZhCn, ClearConversation) => "清空当前会话",
        (En, ClearConversation) => "Clear conversation",
        (ZhCn, ToolControls) => "↑↓选择 Enter折叠 PgUp/Dn滚动 Tab切换",
        (En, ToolControls) => "↑↓ select Enter fold PgUp/Dn scroll Tab panel",
        (ZhCn, ToolCalls) => "工具调用",
        (En, ToolCalls) => "Tool calls",
        (ZhCn, WaitingTools) => "等待工具调用…",
        (En, WaitingTools) => "Waiting for tool calls…",
        (ZhCn, MessagePlaceholder) => "输入消息…",
        (En, MessagePlaceholder) => "Type a message…",
        (ZhCn, Responding) => "响应中",
        (En, Responding) => "Responding",
        (ZhCn, Stop) => "停止",
        (En, Stop) => "Stop",
        (ZhCn, Path) => "路径",
        (En, Path) => "Path",
        (ZhCn, SwitchPanel) => "切换面板",
        (En, SwitchPanel) => "Switch panel",
        (ZhCn, Command) => "命令",
        (En, Command) => "Command",
        (ZhCn, Cancel) => "取消",
        (En, Cancel) => "Cancel",
        (ZhCn, Search) => "搜索",
        (En, Search) => "Search",
        (ZhCn, Thinking) => "思考中",
        (En, Thinking) => "Thinking",
        (ZhCn, Running) => "运行中",
        (En, Running) => "Running",
        (ZhCn, Done) => "完成",
        (En, Done) => "Done",
        (ZhCn, Failed) => "失败",
        (En, Failed) => "Failed",
        (ZhCn, ReturnValue) => "返回",
        (En, ReturnValue) => "Result",
        (ZhCn, Resize) => "终端窗口太小，请调整尺寸",
        (En, Resize) => "Terminal is too small; resize to continue",
        (ZhCn, CommandPalette) => "命令面板",
        (En, CommandPalette) => "Command palette",
        (ZhCn, SwitchLanguage) => "切换语言",
        (En, SwitchLanguage) => "Switch language",
        (ZhCn, SwitchModel) => "切换模型",
        (En, SwitchModel) => "Switch model",
        (ZhCn, Login) => "登录或更换凭据",
        (En, Login) => "Log in or replace credential",
        (ZhCn, Status) => "状态",
        (En, Status) => "Status",
        (ZhCn, ApproveOnce) => "授权下一次工具调用",
        (En, ApproveOnce) => "Approve next tool call",
        (ZhCn, Exit) => "退出",
        (En, Exit) => "Exit",
        (ZhCn, Approve) => "允许",
        (En, Approve) => "Approve",
        (ZhCn, Deny) => "拒绝",
        (En, Deny) => "Deny",
        (ZhCn, Error) => "错误",
        (En, Error) => "Error",
        (ZhCn, Ready) => "就绪",
        (En, Ready) => "Ready",
        (ZhCn, WelcomeHint) => "Enter 开始会话",
        (En, WelcomeHint) => "Press Enter to start",
    }
}

#[cfg(test)]
mod tests {
    use super::Language;

    #[test]
    fn locale_precedence_is_override_persisted_system_fallback() {
        assert_eq!(
            Language::resolve(Some(Language::ZhCn), Some(Language::En), Some("en_US")),
            Language::ZhCn
        );
        assert_eq!(
            Language::resolve(None, Some(Language::ZhCn), Some("en_US")),
            Language::ZhCn
        );
        assert_eq!(
            Language::resolve(None, None, Some("zh_CN.UTF-8")),
            Language::ZhCn
        );
        assert_eq!(Language::resolve(None, None, Some("fr_FR")), Language::En);
    }
}
