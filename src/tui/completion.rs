use super::{
    commands, context,
    i18n::Language,
    state::{AppState, Focus},
};
use std::ops::Range;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub name: String,
    pub description: String,
    pub kind: &'static str,
    pub replacement: String,
    pub range: Range<usize>,
}

impl AppState {
    pub fn candidates(&self) -> Vec<Candidate> {
        if self.focus != Focus::Chat || self.completion_dismissed() {
            return vec![];
        }
        if let Some((range, query)) =
            context::active_reference(self.composer.as_str(), self.composer.cursor())
        {
            let mut files: Vec<_> = self
                .files
                .iter()
                .filter_map(|path| score(path, &query).map(|rank| (rank, path)))
                .collect();
            files.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
            return files
                .into_iter()
                .take(100)
                .map(|(_, path)| Candidate {
                    name: path.clone(),
                    description: String::new(),
                    kind: "file",
                    replacement: format!("{} ", context::reference_token(path)),
                    range: range.clone(),
                })
                .collect();
        }
        let input = self.composer.as_str();
        if let Some(query) = input.strip_prefix("/skill ") {
            if query.chars().any(char::is_whitespace) {
                return vec![];
            }
            return self.skill_candidates(query, 0..input.len(), true);
        }
        if !input.starts_with('/') || input.chars().any(char::is_whitespace) {
            return vec![];
        }
        self.command_candidates(input.trim_start_matches('/'), 0..input.len())
    }

    pub fn command_candidates(&self, query: &str, range: Range<usize>) -> Vec<Candidate> {
        let mut result: Vec<_> = commands::SLASH_COMMANDS
            .iter()
            .filter(|command| !["/language", "/quit"].contains(&command.name) || !query.is_empty())
            .filter_map(|command| {
                score(command.name.trim_start_matches('/'), query)
                    .or_else(|| score(command.description(self.language), query).map(|n| n + 10))
                    .map(|rank| {
                        (
                            rank,
                            Candidate {
                                name: command.name.into(),
                                description: command.description(self.language).into(),
                                kind: "command",
                                replacement: command.name.into(),
                                range: range.clone(),
                            },
                        )
                    })
            })
            .collect();
        result.sort_by_key(|(rank, _)| *rank);
        let mut result: Vec<_> = result.into_iter().map(|(_, candidate)| candidate).collect();
        result.extend(self.skill_candidates(query, range, false));
        result
    }

    fn skill_candidates(&self, query: &str, range: Range<usize>, explicit: bool) -> Vec<Candidate> {
        let mut entries: Vec<_> = self
            .skills
            .iter()
            .filter_map(|skill| {
                score(&skill.qualified_name, query)
                    .or_else(|| score(&skill.description, query).map(|n| n + 10))
                    .map(|rank| (rank, skill))
            })
            .collect();
        entries.sort_by_key(|(rank, _)| *rank);
        entries
            .into_iter()
            .take(100)
            .map(|(_, skill)| Candidate {
                name: format!("/{}", skill.qualified_name),
                description: format!(
                    "{}{} · {}",
                    skill.description,
                    skill
                        .argument_hint
                        .as_ref()
                        .map(|hint| format!(" · {hint}"))
                        .unwrap_or_default(),
                    skill.source
                ),
                kind: "skill",
                replacement: if explicit {
                    format!("/skill {} ", skill.qualified_name)
                } else {
                    format!("/{} ", skill.qualified_name)
                },
                range: range.clone(),
            })
            .collect()
    }

    pub fn completion_hint(&self) -> Option<String> {
        if self.completion_dismissed() || self.focus != Focus::Chat {
            return None;
        }
        if context::active_reference(self.composer.as_str(), self.composer.cursor()).is_some() {
            return Some(if self.files_loading {
                "Indexing files / 正在索引文件".into()
            } else if let Some(error) = &self.files_error {
                format!("{error} · /files retry / 重试")
            } else {
                "No matching files / 没有匹配文件".into()
            });
        }
        if self
            .composer
            .as_str()
            .strip_prefix("/skill ")
            .is_some_and(|query| !query.chars().any(char::is_whitespace))
        {
            return Some(if self.skills_loading {
                "Loading skills / 正在加载技能".into()
            } else if let Some(error) = &self.skills_error {
                format!("{error} · /skills retry / 重试")
            } else if self.skills.is_empty() {
                "No callable skills. Check lato plugin list / 暂无可用技能，请检查插件".into()
            } else {
                "No matching skills / 没有匹配技能".into()
            });
        }
        None
    }
}

pub fn score(text: &str, query: &str) -> Option<usize> {
    let text = text.to_lowercase();
    let query = query.to_lowercase();
    if text.starts_with(&query) {
        return Some(0);
    }
    if text.contains(&query) {
        return Some(1);
    }
    let mut chars = text.chars();
    query
        .chars()
        .all(|wanted| chars.by_ref().any(|ch| ch == wanted))
        .then_some(2)
}

pub fn kind_label(kind: &str, language: Language) -> &str {
    match (kind, language) {
        ("file", Language::ZhCn) => "文件",
        ("skill", Language::ZhCn) => "技能",
        ("command", Language::ZhCn) => "命令",
        _ => kind,
    }
}
