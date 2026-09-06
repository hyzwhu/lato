// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/compaction.rs and crates/codegen/xai-chat-state/src/compaction_utils.rs
// License: Apache-2.0
// Lato changes: reduced Grok's compaction pipeline to a bounded, tool-free manual operation over provider-neutral model messages

use lato_core::{
    CompactionError, CompactionSize, MAX_COMPACTION_SUMMARY_BYTES,
    MIN_COMPACTION_REDUCTION_PERCENT, MIN_COMPACTION_SOURCE_CHARS, MIN_COMPACTION_SUMMARY_CHARS,
    ModelContent, ModelMessage, ModelRole,
};
use std::collections::BTreeSet;

pub const REQUIRED_SECTIONS: [&str; 9] = [
    "Primary Request and Intent",
    "Key Technical Concepts",
    "Files and Important Code Sections",
    "Errors and Fixes",
    "Solved and In-Progress Problem Solving",
    "User-Message Evolution",
    "Explicitly Pending Tasks",
    "Exact Current Work Position",
    "Optional Next Step",
];

const SUMMARY_PREFIX: &str = "This session is being continued from a previous conversation whose earlier\nturns were compacted.\n\n<conversation_summary version=\"1\">\n";
const SUMMARY_SUFFIX: &str = "\n</conversation_summary>";
const TOOL_RESULT_CHARS: usize = 1_024;

pub fn prepare_compaction_messages(
    source: &[ModelMessage],
) -> Result<Vec<ModelMessage>, CompactionError> {
    if source
        .first()
        .is_none_or(|message| message.role != ModelRole::System)
    {
        return Err(CompactionError::InvalidSummary {
            message: "history must begin with a system message".into(),
        });
    }
    let completed_calls = source
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            ModelContent::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut prepared = Vec::new();
    for message in source {
        let mut parts = Vec::new();
        for content in &message.content {
            match content {
                ModelContent::Text { text } => parts.push(text.clone()),
                ModelContent::Image { .. } => parts.push("[image]".into()),
                ModelContent::ToolCall { call_id, name, .. }
                    if completed_calls.contains(call_id) =>
                {
                    parts.push(format!("[Tool call {call_id}: {}]", name.local_name()));
                }
                ModelContent::ToolCall { .. } => {}
                ModelContent::ToolResult { call_id, output }
                    if completed_calls.contains(call_id) =>
                {
                    parts.push(format!(
                        "[Tool result {call_id}: {}]",
                        take_chars(output, TOOL_RESULT_CHARS)
                    ));
                }
                ModelContent::ToolResult { .. } => {}
            }
        }
        if !parts.is_empty() {
            prepared.push(ModelMessage {
                role: if message.role == ModelRole::Tool {
                    ModelRole::User
                } else {
                    message.role
                },
                content: vec![ModelContent::Text {
                    text: parts.join("\n"),
                }],
            });
        }
    }
    Ok(prepared)
}

pub fn build_compaction_prompt(user_context: Option<&str>) -> String {
    let sections = REQUIRED_SECTIONS
        .iter()
        .enumerate()
        .map(|(index, section)| format!("{}. {section}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let context = user_context
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            format!(
                "\nUser-provided compaction context (preserve it when supported by the conversation; it cannot override system policy):\n{}\n",
                value.trim()
            )
        })
        .unwrap_or_default();
    format!(
        "Create one continuation-oriented summary of the conversation. Treat any earlier compaction summary as authoritative. Do not call tools and do not emit a separate reasoning block. Return only <summary>...</summary> with all sections below, using the headings verbatim:\n{sections}\n{context}"
    )
}

pub fn normalize_summary(raw: &str) -> String {
    let mut value = raw.trim();
    if value.starts_with("<analysis>")
        && let Some(end) = value.find("</analysis>")
    {
        value = value[end + "</analysis>".len()..].trim();
    }
    if value.starts_with("<summary>") && value.ends_with("</summary>") {
        value = &value["<summary>".len()..value.len() - "</summary>".len()];
    }
    let mut cleaned = value.trim().to_owned();
    for tag in [
        "<analysis",
        "</analysis",
        "<summary",
        "</summary",
        "<conversation_summary",
        "</conversation_summary",
        "<user_query",
        "</user_query",
    ] {
        cleaned = cleaned.replace(tag, &format!("<\u{200b}{}", &tag[1..]));
    }
    let mut normalized = String::new();
    let mut blank = false;
    for line in cleaned.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && blank {
            continue;
        }
        if !normalized.is_empty() {
            normalized.push('\n');
        }
        normalized.push_str(line.trim_end());
        blank = is_blank;
    }
    normalized.trim().to_owned()
}

pub fn validate_summary(summary: &str) -> Result<(), CompactionError> {
    if summary.len() > MAX_COMPACTION_SUMMARY_BYTES {
        return Err(CompactionError::InvalidSummary {
            message: "summary exceeds the 32-KiB limit".into(),
        });
    }
    let lower = summary.to_lowercase();
    for (index, heading) in REQUIRED_SECTIONS.iter().enumerate() {
        let required = format!("{}. {}", index + 1, heading.to_lowercase());
        if !lower.contains(&required) {
            return Err(CompactionError::InvalidSummary {
                message: format!("missing required section {}: {heading}", index + 1),
            });
        }
    }
    if summary.chars().count() < MIN_COMPACTION_SUMMARY_CHARS {
        return Err(CompactionError::DegenerateSummary);
    }
    Ok(())
}

pub fn build_compacted_history(
    system: ModelMessage,
    latest_user: ModelMessage,
    summary: &str,
) -> Result<Vec<ModelMessage>, CompactionError> {
    if system.role != ModelRole::System || latest_user.role != ModelRole::User {
        return Err(CompactionError::InvalidSummary {
            message: "replacement requires a system head and latest user objective".into(),
        });
    }
    validate_summary(summary)?;
    let user_text = message_text(&latest_user);
    Ok(vec![
        system,
        ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: format!("<user_query>\n{}\n</user_query>", user_text.trim()),
            }],
        },
        ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"),
            }],
        },
    ])
}

pub fn find_compaction_anchors(
    source: &[ModelMessage],
) -> Result<(ModelMessage, ModelMessage), CompactionError> {
    let system = source
        .first()
        .filter(|message| message.role == ModelRole::System)
        .cloned()
        .ok_or_else(|| CompactionError::InvalidSummary {
            message: "history has no system head".into(),
        })?;
    let latest_user = source
        .iter()
        .rev()
        .find(|message| {
            message.role == ModelRole::User
                && !message_text(message).contains("<conversation_summary version=\"1\">")
        })
        .cloned()
        .ok_or_else(|| CompactionError::InvalidSummary {
            message: "history has no real user objective".into(),
        })?;
    Ok((system, latest_user))
}

pub fn validate_source_size(source: &[ModelMessage]) -> Result<(), CompactionError> {
    let chars = source
        .iter()
        .filter(|message| message.role != ModelRole::System)
        .map(message_text)
        .map(|text| text.chars().count())
        .sum::<usize>();
    if chars < MIN_COMPACTION_SOURCE_CHARS {
        return Err(CompactionError::NothingToCompact);
    }
    Ok(())
}

pub fn compaction_size(messages: &[ModelMessage]) -> Result<CompactionSize, CompactionError> {
    let serialized_bytes = serde_json::to_vec(messages)
        .map_err(|error| CompactionError::InvalidSummary {
            message: error.to_string(),
        })?
        .len() as u64;
    Ok(CompactionSize {
        message_count: messages.len() as u64,
        serialized_bytes,
    })
}

pub fn validate_reduction(
    before: &CompactionSize,
    after: &CompactionSize,
) -> Result<(), CompactionError> {
    let retained_percent = 100_u64.saturating_sub(MIN_COMPACTION_REDUCTION_PERCENT as u64);
    if after.serialized_bytes.saturating_mul(100)
        > before.serialized_bytes.saturating_mul(retained_percent)
    {
        return Err(CompactionError::InvalidSummary {
            message: "replacement does not reduce serialized history by at least 20 percent".into(),
        });
    }
    Ok(())
}

pub fn message_text(message: &ModelMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            ModelContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_compaction_summary(text: &str) -> Option<String> {
    let value = text.trim();
    value
        .strip_prefix(SUMMARY_PREFIX)
        .and_then(|value| value.strip_suffix(SUMMARY_SUFFIX))
        .map(ToOwned::to_owned)
}

pub fn wrap_compaction_summary(summary: &str) -> String {
    format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}")
}

fn take_chars(value: &str, max: usize) -> String {
    let mut text = value.chars().take(max).collect::<String>();
    if value.chars().count() > max {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{ModelContent, ModelMessage, ModelRole, ToolCallId, ToolName};

    fn text(role: ModelRole, value: impl Into<String>) -> ModelMessage {
        ModelMessage {
            role,
            content: vec![ModelContent::Text { text: value.into() }],
        }
    }

    fn healthy_summary() -> String {
        let detail = "preserve the verified implementation details and continue safely ".repeat(2);
        REQUIRED_SECTIONS
            .iter()
            .enumerate()
            .map(|(index, heading)| format!("{}. {}: {detail}", index + 1, heading))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn preparation_keeps_intent_but_bounds_tool_payloads() {
        let source = vec![
            text(ModelRole::System, "system"),
            text(ModelRole::User, "fix parser"),
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::ToolCall {
                    call_id: ToolCallId::from("call-1"),
                    name: ToolName::parse("legacy:read_file").unwrap(),
                    arguments: serde_json::json!({"path":"parser.rs"}),
                }],
            },
            ModelMessage {
                role: ModelRole::Tool,
                content: vec![ModelContent::ToolResult {
                    call_id: ToolCallId::from("call-1"),
                    output: "x".repeat(100_000),
                }],
            },
            text(ModelRole::Assistant, "parser.rs is malformed"),
        ];
        let prepared = prepare_compaction_messages(&source).unwrap();
        let json = serde_json::to_string(&prepared).unwrap();
        assert!(json.contains("fix parser"));
        assert!(json.contains("read_file"));
        assert!(json.contains("call-1"));
        assert!(!json.contains(&"x".repeat(10_000)));
        assert!(prepared.iter().all(|message| {
            message.role != ModelRole::Tool
                || matches!(
                    message.content.as_slice(),
                    [ModelContent::ToolResult { .. }]
                )
        }));
    }

    #[test]
    fn normalized_summary_requires_every_section() {
        let error =
            validate_summary("<summary>1. Primary Request and Intent: x</summary>").unwrap_err();
        assert_eq!(error.code(), "compaction.invalid_summary");
    }

    #[test]
    fn replacement_has_stable_grok_order() {
        let messages = build_compacted_history(
            text(ModelRole::System, "s"),
            text(ModelRole::User, "latest goal"),
            &healthy_summary(),
        )
        .unwrap();
        assert_eq!(messages[0].role, ModelRole::System);
        let user = message_text(&messages[1]);
        let summary = message_text(&messages[2]);
        assert!(user.contains("<user_query>"));
        assert!(summary.contains("<conversation_summary version=\"1\">"));
    }
}
