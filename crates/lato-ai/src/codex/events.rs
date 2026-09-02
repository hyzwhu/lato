use crate::StreamPiece;
use std::collections::HashMap;

#[derive(Default)]
pub struct CodexEventMapper {
    pending: HashMap<String, PendingCall>,
    started: bool,
    terminal: bool,
}

#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl CodexEventMapper {
    pub fn started(&self) -> bool {
        self.started
    }

    pub fn terminal(&self) -> bool {
        self.terminal
    }

    pub fn accept(&mut self, value: serde_json::Value) -> Result<Vec<StreamPiece>, String> {
        let event_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        match event_type {
            "response.output_text.delta" => {
                self.started = true;
                let delta = value
                    .get("delta")
                    .and_then(|value| value.as_str())
                    .ok_or("Codex text delta is missing delta")?;
                Ok((!delta.is_empty())
                    .then(|| StreamPiece::Text(delta.into()))
                    .into_iter()
                    .collect())
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                self.started = true;
                Ok(Vec::new())
            }
            "response.output_item.added"
                if value.pointer("/item/type").and_then(|value| value.as_str())
                    == Some("function_call") =>
            {
                self.started = true;
                let item = &value["item"];
                let item_id = required_string(item, "id")?;
                self.pending.insert(
                    item_id,
                    PendingCall {
                        call_id: required_string(item, "call_id")?,
                        name: required_string(item, "name")?,
                        arguments: item
                            .get("arguments")
                            .and_then(|value| value.as_str())
                            .unwrap_or("")
                            .into(),
                    },
                );
                Ok(Vec::new())
            }
            "response.function_call_arguments.delta" => {
                self.started = true;
                let item_id = required_string(&value, "item_id")?;
                let delta = required_string(&value, "delta")?;
                self.pending
                    .get_mut(&item_id)
                    .ok_or("Codex function argument delta references an unknown item")?
                    .arguments
                    .push_str(&delta);
                Ok(Vec::new())
            }
            "response.function_call_arguments.done" => {
                self.started = true;
                let item_id = required_string(&value, "item_id")?;
                let call = self
                    .pending
                    .remove(&item_id)
                    .ok_or("Codex completed an unknown function call")?;
                let raw = value
                    .get("arguments")
                    .and_then(|value| value.as_str())
                    .unwrap_or(&call.arguments);
                let arguments = if raw.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(raw)
                        .map_err(|_| format!("invalid tool arguments for {}", call.call_id))?
                };
                Ok(vec![StreamPiece::ToolCall {
                    id: call.call_id,
                    name: call.name,
                    arguments,
                }])
            }
            "response.output_item.done"
                if value.pointer("/item/type").and_then(|value| value.as_str())
                    == Some("function_call") =>
            {
                self.started = true;
                let item = &value["item"];
                let item_id = required_string(item, "id")?;
                let Some(call) = self.pending.remove(&item_id) else {
                    return Ok(Vec::new());
                };
                let raw = item
                    .get("arguments")
                    .and_then(|value| value.as_str())
                    .unwrap_or(&call.arguments);
                let arguments = serde_json::from_str(raw)
                    .map_err(|_| format!("invalid tool arguments for {}", call.call_id))?;
                Ok(vec![StreamPiece::ToolCall {
                    id: call.call_id,
                    name: call.name,
                    arguments,
                }])
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                self.started = true;
                self.terminal = true;
                Ok(Vec::new())
            }
            "response.failed" | "error" => {
                self.started = true;
                self.terminal = true;
                let message = value
                    .pointer("/response/error/message")
                    .or_else(|| value.pointer("/error/message"))
                    .or_else(|| value.get("message"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("OpenAI Codex request failed");
                Err(message.into())
            }
            _ => Ok(Vec::new()),
        }
    }
}

fn required_string(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Codex event is missing {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_text_and_emits_function_call_exactly_once() {
        let mut mapper = CodexEventMapper::default();
        assert_eq!(
            mapper
                .accept(serde_json::json!({"type":"response.output_text.delta","delta":"hello"}))
                .unwrap()
                .len(),
            1
        );
        mapper
            .accept(serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"grep","arguments":""}}))
            .unwrap();
        mapper
            .accept(serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"{\"pattern\":"}))
            .unwrap();
        mapper
            .accept(serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"\"TODO\"}"}))
            .unwrap();
        let pieces = mapper
            .accept(serde_json::json!({"type":"response.function_call_arguments.done","item_id":"item-1"}))
            .unwrap();
        assert!(
            matches!(&pieces[0], StreamPiece::ToolCall { id, name, arguments }
            if id == "call-1" && name == "grep" && arguments["pattern"] == "TODO")
        );
        assert!(mapper.started());
    }

    #[test]
    fn rejects_invalid_completed_function_arguments() {
        let mut mapper = CodexEventMapper::default();
        mapper
            .accept(serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"grep"}}))
            .unwrap();
        assert!(mapper
            .accept(serde_json::json!({"type":"response.function_call_arguments.done","item_id":"item-1","arguments":"{"}))
            .unwrap_err()
            .contains("call-1"));
    }
}
