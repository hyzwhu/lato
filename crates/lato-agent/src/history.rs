#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum HistoryItem {
    System(String),
    User(String),
    AssistantText(String),
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        id: String,
        output: String,
    },
    CompactionSummary(String),
}
