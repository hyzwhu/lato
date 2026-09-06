// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs
// License: Apache-2.0
// Lato changes: provider-neutral, whole-unit compaction input fitting with a bounded lossy fallback

use crate::{message_text, prepare_compaction_messages};
use lato_core::{CompactionError, ModelContent, ModelMessage, ModelRole};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionInputStage {
    Prepared,
    Fitted,
    Lossy,
}

impl CompactionInputStage {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Prepared => Some(Self::Fitted),
            Self::Fitted => Some(Self::Lossy),
            Self::Lossy => None,
        }
    }
}

pub fn prepare_compaction_input(
    source: &[ModelMessage],
    stage: CompactionInputStage,
    input_budget_tokens: u64,
) -> Result<Vec<ModelMessage>, CompactionError> {
    match stage {
        CompactionInputStage::Prepared => prepare_compaction_messages(source),
        CompactionInputStage::Fitted => {
            let fitted = fit_whole_units(source.to_vec(), input_budget_tokens.saturating_mul(4))?;
            prepare_compaction_messages(&fitted)
        }
        CompactionInputStage::Lossy => {
            let fitted =
                fit_whole_units(lossy_source(source), input_budget_tokens.saturating_mul(4))?;
            prepare_compaction_messages(&fitted)
        }
    }
}

fn lossy_source(source: &[ModelMessage]) -> Vec<ModelMessage> {
    source
        .iter()
        .map(|message| ModelMessage {
            role: message.role,
            content: message
                .content
                .iter()
                .map(|content| match content {
                    ModelContent::ToolResult { call_id, output } => ModelContent::ToolResult {
                        call_id: call_id.clone(),
                        output: format!("[tool result retained; {} bytes omitted]", output.len()),
                    },
                    other => other.clone(),
                })
                .collect(),
        })
        .collect()
}

fn fit_whole_units(
    prepared: Vec<ModelMessage>,
    body_budget_bytes: u64,
) -> Result<Vec<ModelMessage>, CompactionError> {
    let Some(system) = prepared.first().cloned() else {
        return Err(CompactionError::InvalidSummary {
            message: "history must begin with a system message".into(),
        });
    };
    let body = &prepared[1..];
    if body.is_empty() {
        return Ok(vec![system]);
    }

    let units = complete_units(body);
    let latest_user = body
        .iter()
        .rposition(|message| {
            message.role == ModelRole::User
                && !message_text(message).contains("<conversation_summary version=\"1\">")
        })
        .unwrap_or(body.len() - 1);
    let prior_summary = body.iter().rposition(|message| {
        message_text(message).contains("<conversation_summary version=\"1\">")
    });
    let mut selected = BTreeSet::new();
    selected.insert(unit_containing(&units, latest_user));
    if let Some(index) = prior_summary {
        selected.insert(unit_containing(&units, index));
    }

    for unit_index in (0..units.len()).rev() {
        let mut candidate = selected.clone();
        candidate.insert(unit_index);
        if units_bytes(body, &units, &candidate) <= body_budget_bytes {
            selected = candidate;
        }
    }

    let mut fitted = selected_messages(body, &units, &selected);
    if serialized_bytes(&fitted) > body_budget_bytes {
        // Mandatory anchors may be larger than the window. Keep their identity and
        // safely trim text on UTF-8 boundaries, newest first.
        truncate_to_budget(&mut fitted, body_budget_bytes);
    }
    let mut result = vec![system];
    result.extend(fitted);
    Ok(result)
}

fn complete_units(body: &[ModelMessage]) -> Vec<std::ops::Range<usize>> {
    let mut units = Vec::new();
    let mut index = 0;
    while index < body.len() {
        let start = index;
        index += 1;
        if body[start].role == ModelRole::Assistant {
            while index < body.len() && body[index].role == ModelRole::Tool {
                index += 1;
            }
        }
        units.push(start..index);
    }
    units
}

fn unit_containing(units: &[std::ops::Range<usize>], message_index: usize) -> usize {
    units
        .iter()
        .position(|unit| unit.contains(&message_index))
        .unwrap_or(units.len().saturating_sub(1))
}

fn selected_messages(
    body: &[ModelMessage],
    units: &[std::ops::Range<usize>],
    selected: &BTreeSet<usize>,
) -> Vec<ModelMessage> {
    selected
        .iter()
        .flat_map(|index| units[*index].clone())
        .map(|index| body[index].clone())
        .collect()
}

fn units_bytes(
    body: &[ModelMessage],
    units: &[std::ops::Range<usize>],
    selected: &BTreeSet<usize>,
) -> u64 {
    serialized_bytes(&selected_messages(body, units, selected))
}

fn serialized_bytes(messages: &[ModelMessage]) -> u64 {
    serde_json::to_vec(messages)
        .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX)
}

fn truncate_to_budget(messages: &mut [ModelMessage], budget: u64) {
    while serialized_bytes(messages) > budget {
        let Some((message_index, content_index, text_len)) = messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(message_index, message)| {
                message
                    .content
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(content_index, content)| match content {
                        ModelContent::Text { text } if !text.is_empty() => {
                            Some((message_index, content_index, text.len()))
                        }
                        _ => None,
                    })
            })
        else {
            break;
        };
        let excess = serialized_bytes(messages).saturating_sub(budget);
        let keep = text_len.saturating_sub(usize::try_from(excess.max(1)).unwrap_or(usize::MAX));
        let ModelContent::Text { text } = &mut messages[message_index].content[content_index]
        else {
            unreachable!();
        };
        let boundary = floor_char_boundary(text, keep);
        let dropped = text.len().saturating_sub(boundary);
        let marker = format!("\n[... truncated {dropped} bytes to fit the compaction window ...]");
        if boundary == 0 && marker.len() >= text.len() {
            text.clear();
        } else {
            text.truncate(boundary);
            text.push_str(&marker);
        }
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{ToolCallId, ToolName};

    fn text(role: ModelRole, value: impl Into<String>) -> ModelMessage {
        ModelMessage {
            role,
            content: vec![ModelContent::Text { text: value.into() }],
        }
    }

    #[test]
    fn stages_only_advance_toward_lossy() {
        assert_eq!(
            CompactionInputStage::Prepared.next(),
            Some(CompactionInputStage::Fitted)
        );
        assert_eq!(
            CompactionInputStage::Fitted.next(),
            Some(CompactionInputStage::Lossy)
        );
        assert_eq!(CompactionInputStage::Lossy.next(), None);
    }

    #[test]
    fn fitted_input_preserves_anchors_and_complete_tool_units() {
        let source = vec![
            text(ModelRole::System, "system"),
            text(ModelRole::User, "old objective"),
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::ToolCall {
                    call_id: ToolCallId::from("c1"),
                    name: ToolName::parse("legacy:read_file").unwrap(),
                    arguments: serde_json::json!({"path":"a"}),
                }],
            },
            ModelMessage {
                role: ModelRole::Tool,
                content: vec![ModelContent::ToolResult {
                    call_id: ToolCallId::from("c1"),
                    output: "你".repeat(8_000),
                }],
            },
            text(ModelRole::User, "latest objective"),
        ];
        let fitted = prepare_compaction_input(&source, CompactionInputStage::Fitted, 250).unwrap();
        assert_eq!(fitted.first().unwrap().role, ModelRole::System);
        assert!(
            fitted
                .iter()
                .any(|message| message_text(message).contains("latest objective"))
        );
        let all = fitted
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            all.contains("[Tool call c1:"),
            all.contains("[Tool result c1:")
        );
        assert!(!fitted.iter().any(|message| message.role == ModelRole::Tool));
    }

    #[test]
    fn lossy_keeps_prior_summary_and_latest_objective() {
        let source = vec![
            text(ModelRole::System, "system"),
            text(
                ModelRole::User,
                "<conversation_summary version=\"1\">prior</conversation_summary>",
            ),
            text(ModelRole::User, "old ".repeat(2_000)),
            text(ModelRole::User, "latest objective"),
        ];
        let fitted = prepare_compaction_input(&source, CompactionInputStage::Lossy, 250).unwrap();
        let all = fitted
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("prior"));
        assert!(all.contains("latest objective"));
    }

    #[test]
    fn tiny_and_zero_budgets_are_utf8_safe() {
        let source = vec![
            text(ModelRole::System, "system"),
            text(ModelRole::User, "目标".repeat(100)),
        ];
        for budget in [0, 1, 8] {
            let fitted =
                prepare_compaction_input(&source, CompactionInputStage::Fitted, budget).unwrap();
            assert_eq!(fitted[0].role, ModelRole::System);
            assert!(serde_json::to_string(&fitted).is_ok());
        }
    }
}
