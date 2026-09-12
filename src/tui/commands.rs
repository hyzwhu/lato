use super::i18n::Language;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description_zh: &'static str,
    pub description_en: &'static str,
}

impl SlashCommand {
    pub fn description(self, language: Language) -> &'static str {
        match language {
            Language::ZhCn => self.description_zh,
            Language::En => self.description_en,
        }
    }
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        description_zh: "显示所有命令",
        description_en: "Show all commands",
    },
    SlashCommand {
        name: "/new",
        description_zh: "新建会话",
        description_en: "Start a new session",
    },
    SlashCommand {
        name: "/clear",
        description_zh: "清空当前会话",
        description_en: "Clear this session",
    },
    SlashCommand {
        name: "/compact",
        description_zh: "压缩当前会话上下文",
        description_en: "Compact the current session context",
    },
    SlashCommand {
        name: "/sessions",
        description_zh: "切换会话",
        description_en: "Switch session",
    },
    SlashCommand {
        name: "/rename",
        description_zh: "重命名当前会话",
        description_en: "Rename this session",
    },
    SlashCommand {
        name: "/delete",
        description_zh: "删除当前会话",
        description_en: "Delete this session",
    },
    SlashCommand {
        name: "/model",
        description_zh: "切换模型",
        description_en: "Switch model",
    },
    SlashCommand {
        name: "/login",
        description_zh: "登录或更换凭据",
        description_en: "Log in or replace credentials",
    },
    SlashCommand {
        name: "/doctor",
        description_zh: "运行诊断",
        description_en: "Run diagnostics",
    },
    SlashCommand {
        name: "/search",
        description_zh: "搜索对话",
        description_en: "Search conversation",
    },
    SlashCommand {
        name: "/lang",
        description_zh: "切换语言",
        description_en: "Switch language",
    },
    SlashCommand {
        name: "/language",
        description_zh: "切换语言（别名）",
        description_en: "Switch language (alias)",
    },
    SlashCommand {
        name: "/approve",
        description_zh: "授权下一次工具调用",
        description_en: "Approve next tool call",
    },
    SlashCommand {
        name: "/status",
        description_zh: "显示状态",
        description_en: "Show status",
    },
    SlashCommand {
        name: "/permissions",
        description_zh: "显示权限",
        description_en: "Show permissions",
    },
    SlashCommand {
        name: "/exit",
        description_zh: "退出",
        description_en: "Exit",
    },
    SlashCommand {
        name: "/quit",
        description_zh: "退出（别名）",
        description_en: "Exit (alias)",
    },
    SlashCommand {
        name: "/skills",
        description_zh: "浏览可用技能 · 名称与参数",
        description_en: "Browse skills · names and arguments",
    },
    SlashCommand {
        name: "/skill",
        description_zh: "调用技能 <名称> [参数]",
        description_en: "Run skill <name> [arguments]",
    },
    SlashCommand {
        name: "/files",
        description_zh: "搜索项目文件 · @",
        description_en: "Find project files · @",
    },
    SlashCommand {
        name: "/workflows",
        description_zh: "列出可用工作流",
        description_en: "List available workflows",
    },
];

#[cfg(test)]
pub fn matches(input: &str) -> Vec<&'static SlashCommand> {
    if input.is_empty() || !input.starts_with('/') || input.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let prefix = input.to_ascii_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(&prefix))
        .collect()
}

pub fn help_line() -> String {
    SLASH_COMMANDS
        .iter()
        .map(|command| command.name)
        .collect::<Vec<_>>()
        .join("  ")
}

#[cfg(test)]
mod tests {
    use super::{SLASH_COMMANDS, help_line, matches};

    #[test]
    fn slash_lists_every_command_and_alias() {
        let all = matches("/");
        assert_eq!(all.len(), SLASH_COMMANDS.len());
        assert!(all.iter().any(|command| command.name == "/language"));
        assert!(all.iter().any(|command| command.name == "/quit"));
        assert_eq!(all.len(), SLASH_COMMANDS.len());
    }

    #[test]
    fn slash_filters_case_insensitively_and_stops_at_arguments() {
        assert_eq!(matches("/COM")[0].name, "/compact");
        let names = matches("/MO")
            .into_iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["/model"]);
        assert!(matches("/rename title").is_empty());
        assert!(matches("/unknown").is_empty());
    }

    #[test]
    fn help_is_generated_from_registry_order() {
        assert_eq!(
            help_line(),
            SLASH_COMMANDS
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>()
                .join("  ")
        );
    }
}
