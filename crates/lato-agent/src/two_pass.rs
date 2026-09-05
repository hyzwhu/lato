// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/helpers/session_compact.rs
// License: Apache-2.0
// Lato changes: deterministic provider-neutral two-pass split, cache fingerprint, and prompt builders

use lato_core::{ModelContent, ModelMessage, ModelRole};

pub(crate) const TWO_PASS_SPLIT_PERCENT: u8 = 95;
pub(crate) const PREFIRE_LEAD_PERCENT: u8 = 10;
pub(crate) const MAX_NOTE1_CHARS: usize = 12_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TwoPassSplit {
    pub index: usize,
    pub prefix_tokens: u64,
    pub total_tokens: u64,
}

pub(crate) fn split_for_two_pass(history: &[ModelMessage], percent: u8) -> TwoPassSplit {
    let token_counts = history.iter().map(message_tokens).collect::<Vec<_>>();
    let total_tokens = token_counts
        .iter()
        .copied()
        .fold(0_u64, u64::saturating_add);
    let target = total_tokens
        .saturating_mul(u64::from(percent))
        .div_ceil(100);
    let mut prefix_tokens = 0_u64;
    let mut index = 0;
    while index < history.len() && prefix_tokens < target {
        prefix_tokens = prefix_tokens.saturating_add(token_counts[index]);
        index += 1;
    }
    while index < history.len() && history[index].role == ModelRole::Tool {
        prefix_tokens = prefix_tokens.saturating_add(token_counts[index]);
        index += 1;
    }
    TwoPassSplit {
        index,
        prefix_tokens,
        total_tokens,
    }
}

pub(crate) fn fingerprint_prefix(history: &[ModelMessage], prefix_len: usize) -> u64 {
    // FNV-1a is fixed here deliberately; this ephemeral cache key must still be
    // deterministic across processes and Rust releases for auditable tests.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in serde_json::to_vec(&history[..prefix_len.min(history.len())]).unwrap_or_default() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(crate) fn note_for_pass_two(raw: &str) -> String {
    let trimmed = raw.trim();
    let value = trimmed
        .strip_prefix("<summary>")
        .and_then(|value| value.strip_suffix("</summary>"))
        .unwrap_or(trimmed)
        .trim();
    if value.chars().count() <= MAX_NOTE1_CHARS {
        return value.to_owned();
    }
    let mut note = value.chars().take(MAX_NOTE1_CHARS).collect::<String>();
    note.push_str("\n[NOTE1 truncated]");
    note
}

pub(crate) fn build_pass_one_history(prefix: &[ModelMessage], prompt: &str) -> Vec<ModelMessage> {
    let mut messages = prefix.to_vec();
    messages.push(text(ModelRole::User, prompt));
    messages
}

pub(crate) fn build_pass_two_history(
    prefix: &[ModelMessage],
    tail: &[ModelMessage],
    note1: &str,
    prompt: &str,
) -> Vec<ModelMessage> {
    let mut messages = prefix
        .iter()
        .filter(|message| message.role == ModelRole::System)
        .cloned()
        .collect::<Vec<_>>();
    messages.push(text(
        ModelRole::User,
        format!("<first_pass_summary>\n{note1}\n</first_pass_summary>"),
    ));
    messages.extend_from_slice(tail);
    messages.push(text(
        ModelRole::User,
        format!(
            "Merge the first-pass summary with the remaining recent conversation. The recent conversation wins on conflict.\n\n{prompt}"
        ),
    ));
    messages
}

fn text(role: ModelRole, value: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role,
        content: vec![ModelContent::Text { text: value.into() }],
    }
}

fn message_tokens(message: &ModelMessage) -> u64 {
    serde_json::to_vec(message)
        .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX).div_ceil(4))
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<ModelMessage> {
        (0..20)
            .map(|index| text(ModelRole::User, format!("{index}-{}", "x".repeat(100))))
            .collect()
    }

    #[test]
    fn split_reaches_target_without_severing_tool_tail() {
        let mut history = history();
        history.push(text(ModelRole::Assistant, "tool call"));
        history.push(text(ModelRole::Tool, "result"));
        let split = split_for_two_pass(&history, 95);
        assert!(split.prefix_tokens.saturating_mul(100) >= split.total_tokens * 95);
        assert!(split.index == history.len() || history[split.index].role != ModelRole::Tool);
    }

    #[test]
    fn note_is_unwrapped_and_bounded() {
        assert_eq!(
            note_for_pass_two(&format!("<summary>{}</summary>", "x".repeat(1_001))),
            "x".repeat(1_001)
        );
        assert!(note_for_pass_two(&"x".repeat(13_000)).chars().count() < 12_100);
    }

    #[test]
    fn prefix_fingerprint_ignores_appended_tail_but_not_prefix_mutation() {
        let mut messages = history();
        let prefix_len = 10;
        let original = fingerprint_prefix(&messages, prefix_len);
        messages.push(text(ModelRole::User, "tail"));
        assert_eq!(fingerprint_prefix(&messages, prefix_len), original);
        messages[2] = text(ModelRole::Assistant, "changed");
        assert_ne!(fingerprint_prefix(&messages, prefix_len), original);
    }
}
