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
            fit_whole_units(source.to_vec(), input_budget_tokens.saturating_mul(4))
        }
        CompactionInputStage::Lossy => {
            fit_whole_units(lossy_source(source), input_budget_tokens.saturating_mul(4))
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
    source: Vec<ModelMessage>,
    body_budget_bytes: u64,
) -> Result<Vec<ModelMessage>, CompactionError> {
    let Some(system) = source.first().cloned() else {
        return Err(CompactionError::InvalidSummary {
            message: "history must begin with a system message".into(),
        });
    };
    let body = &source[1..];
    if body.is_empty() {
        return prepare_compaction_messages(&[system]);
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
        if prepared_units_bytes(&system, body, &units, &candidate)? <= body_budget_bytes {
            selected = candidate;
        }
    }

    let mut selected_source = vec![system];
    selected_source.extend(selected_messages(body, &units, &selected));
    let mut prepared = prepare_compaction_messages(&selected_source)?;
    if serialized_bytes(&prepared[1..]) > body_budget_bytes {
        // Mandatory anchors may be larger than the window. Keep their identity and
        // safely trim text on UTF-8 boundaries, newest first.
        truncate_to_budget(&mut prepared[1..], body_budget_bytes)?;
    }
    Ok(prepared)
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

fn prepared_units_bytes(
    system: &ModelMessage,
    body: &[ModelMessage],
    units: &[std::ops::Range<usize>],
    selected: &BTreeSet<usize>,
) -> Result<u64, CompactionError> {
    let mut candidate = vec![system.clone()];
    candidate.extend(selected_messages(body, units, selected));
    let prepared = prepare_compaction_messages(&candidate)?;
    Ok(serialized_bytes(&prepared[1..]))
}

fn serialized_bytes(messages: &[ModelMessage]) -> u64 {
    serde_json::to_vec(messages)
        .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX)
}

fn truncate_to_budget(messages: &mut [ModelMessage], budget: u64) -> Result<(), CompactionError> {
    loop {
        let current_size = serialized_bytes(messages);
        if current_size <= budget {
            return Ok(());
        }
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
            return Err(CompactionError::InputTooLarge);
        };
        let excess = current_size.saturating_sub(budget);
        // Reserve enough room for the truncation marker and its JSON escaping. If
        // the estimate is ever insufficient, the strict-progress check below
        // clears this field instead of looping on an unchanged serialized size.
        let keep = text_len
            .saturating_sub(usize::try_from(excess.saturating_add(128)).unwrap_or(usize::MAX));
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
        if serialized_bytes(messages) >= current_size {
            let ModelContent::Text { text } = &mut messages[message_index].content[content_index]
            else {
                unreachable!();
            };
            text.clear();
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
                    output: "x".repeat(100_000),
                }],
            },
            text(ModelRole::User, "latest objective"),
        ];
        for (budget, expect_pair) in [(500, true), (100, false)] {
            let fitted =
                prepare_compaction_input(&source, CompactionInputStage::Fitted, budget).unwrap();
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
            assert_eq!(all.contains("[Tool call c1:"), expect_pair, "{budget}");
            assert_eq!(all.contains("[Tool result c1:"), expect_pair, "{budget}");
            assert!(!fitted.iter().any(|message| message.role == ModelRole::Tool));
            assert!(serialized_bytes(&fitted[1..]) <= budget * 4);
        }
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
            match prepare_compaction_input(&source, CompactionInputStage::Fitted, budget) {
                Ok(fitted) => {
                    assert_eq!(fitted[0].role, ModelRole::System);
                    assert!(serde_json::to_string(&fitted).is_ok());
                    assert!(serialized_bytes(&fitted[1..]) <= budget * 4);
                }
                Err(error) => assert_eq!(error.code(), "compaction.input_too_large"),
            }
        }
    }

    #[test]
    fn moderate_budget_truncation_terminates_and_respects_the_body_limit() {
        let source = vec![
            text(ModelRole::System, "system"),
            text(ModelRole::User, "mandatory-ascii-anchor".repeat(1_000)),
        ];
        let budget = 300;
        let fitted =
            prepare_compaction_input(&source, CompactionInputStage::Fitted, budget).unwrap();
        assert_eq!(fitted.first().unwrap().role, ModelRole::System);
        assert!(serialized_bytes(&fitted[1..]) <= budget * 4);
    }
}
