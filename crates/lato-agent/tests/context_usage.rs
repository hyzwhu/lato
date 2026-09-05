use lato_agent::{
    ContextTracker, HistoryItem, SwitchCompaction, decide_switch_compaction,
    estimate_history_tokens, has_model_authored_history,
};
use lato_ai::{FakeModelStream, ModelCallReport, ModelMetadata, adapt_model_endpoint};
use lato_core::ModelUsage;
use std::sync::Arc;

fn model(family: &str, context_window: u64) -> ModelMetadata {
    ModelMetadata {
        context_window: Some(context_window),
        model_family: Some(family.to_owned()),
    }
}

#[test]
fn history_estimate_counts_text_and_serialized_tool_payloads() {
    let history = vec![
        HistoryItem::User("12345678".into()),
        HistoryItem::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path":"abcdef"}),
        },
        HistoryItem::ToolResult {
            id: "c1".into(),
            output: "12345678".into(),
        },
    ];

    assert_eq!(estimate_history_tokens(&history), 11);
}

#[test]
fn model_authored_history_excludes_system_user_and_tool_results() {
    assert!(!has_model_authored_history(&[
        HistoryItem::System("system".into()),
        HistoryItem::User("user".into()),
        HistoryItem::ToolResult {
            id: "c1".into(),
            output: "result".into(),
        },
    ]));
    assert!(has_model_authored_history(&[HistoryItem::AssistantText(
        "answer".into(),
    )]));
}

#[test]
fn same_family_shrink_defers_only_when_new_threshold_is_reached() {
    assert_eq!(
        decide_switch_compaction(&model("a", 2_000), &model("a", 1_000), 850, true, 85),
        SwitchCompaction::BeforeNextSample
    );
    assert_eq!(
        decide_switch_compaction(&model("a", 2_000), &model("a", 1_000), 849, true, 85),
        SwitchCompaction::None
    );
}

#[test]
fn cross_family_with_assistant_history_compacts_immediately() {
    assert_eq!(
        decide_switch_compaction(&model("a", 2_000), &model("b", 3_000), 10, true, 85),
        SwitchCompaction::Immediate
    );
    assert_eq!(
        decide_switch_compaction(&model("a", 2_000), &model("b", 3_000), 10, false, 85),
        SwitchCompaction::None
    );
}

#[test]
fn tracker_ignores_usage_from_an_obsolete_model_generation() {
    let endpoint = adapt_model_endpoint(
        "p",
        "m",
        model("a", 1_000),
        Arc::new(FakeModelStream::new(Vec::new())),
    )
    .unwrap();
    let history = vec![HistoryItem::User("12345678".into())];
    let report = ModelCallReport {
        usage: Some(ModelUsage {
            input_tokens: Some(80),
            output_tokens: Some(20),
            reasoning_tokens: None,
            cached_input_tokens: None,
        }),
        generation: 1,
    };
    let mut tracker = ContextTracker::default();

    assert!(!tracker.observe(&history, &report, 2));
    assert_eq!(
        tracker
            .measure(&history, &endpoint.port)
            .estimated_input_tokens,
        2
    );
    assert!(tracker.observe(&history, &report, 1));
    assert_eq!(
        tracker
            .measure(&history, &endpoint.port)
            .estimated_input_tokens,
        100
    );
}

#[test]
fn pending_model_switch_check_is_consumed_once() {
    let mut tracker = ContextTracker::default();
    assert!(!tracker.take_model_switch_check());
    tracker.mark_model_switch_check();
    assert!(tracker.take_model_switch_check());
    assert!(!tracker.take_model_switch_check());
}
