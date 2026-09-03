use crate::StreamPiece;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct CodexEventMapper {
    pending: HashMap<String, PendingCall>,
    emitted_calls: HashSet<String>,
    text_items: HashSet<String>,
    unkeyed_text: bool,
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

    pub fn ensure_complete(&self) -> Result<(), String> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err("Responses stream ended with unfinished tool calls".into())
        }
    }

    fn emit_call(&mut self, call: PendingCall) -> Result<Vec<StreamPiece>, String> {
        if self.emitted_calls.contains(&call.call_id) {
            return Ok(Vec::new());
        }
        let arguments = if call.arguments.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&call.arguments)
                .map_err(|_| format!("invalid tool arguments for {}", call.call_id))?
        };
        self.emitted_calls.insert(call.call_id.clone());
        Ok(vec![StreamPiece::ToolCall {
            id: call.call_id,
            name: call.name,
            arguments,
        }])
    }

    fn complete_item(&mut self, item: &serde_json::Value) -> Result<Vec<StreamPiece>, String> {
        match item.get("type").and_then(|v| v.as_str()) {
            Some("function_call") => {
                let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let pending = self.pending.remove(item_id);
                let call_id = required_string(item, "call_id")?;
                let name = required_string(item, "name")?;
                let arguments = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .or_else(|| pending.map(|call| call.arguments))
                    .ok_or("Responses completed function call is missing arguments")?;
                self.emit_call(PendingCall {
                    call_id,
                    name,
                    arguments,
                })
            }
            Some("message") => {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if self.unkeyed_text || self.text_items.contains(id) {
                    return Ok(Vec::new());
                }
                if !id.is_empty() {
                    self.text_items.insert(id.into());
                }
                Ok(item
                    .get("content")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "output_text")
                    .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                    .map(|text| StreamPiece::Text(text.into()))
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    pub fn accept(&mut self, value: serde_json::Value) -> Result<Vec<StreamPiece>, String> {
        if self.terminal {
            return Ok(Vec::new());
        }
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "response.output_text.delta" => {
                self.started = true;
                if let Some(id) = value.get("item_id").and_then(|v| v.as_str()) {
                    self.text_items.insert(id.into());
                } else {
                    self.unkeyed_text = true;
                }
                let delta = value
                    .get("delta")
                    .and_then(|v| v.as_str())
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
            "response.output_item.added" if value["item"]["type"] == "function_call" => {
                self.started = true;
                let item = &value["item"];
                self.pending.insert(
                    required_string(item, "id")?,
                    PendingCall {
                        call_id: required_string(item, "call_id")?,
                        name: required_string(item, "name")?,
                        arguments: item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .into(),
                    },
                );
                Ok(Vec::new())
            }
            "response.function_call_arguments.delta" => {
                self.started = true;
                let id = required_string(&value, "item_id")?;
                let delta = value
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .ok_or("Codex function argument delta is missing delta")?;
                self.pending
                    .get_mut(&id)
                    .ok_or("Codex function argument delta references an unknown item")?
                    .arguments
                    .push_str(delta);
                Ok(Vec::new())
            }
            "response.function_call_arguments.done" => {
                self.started = true;
                let id = required_string(&value, "item_id")?;
                // Some gateways send the completed item without any added event.
                // In that case wait for its authoritative identity and arguments.
                let Some(mut call) = self.pending.remove(&id) else {
                    return Ok(Vec::new());
                };
                if let Some(raw) = value.get("arguments").and_then(|v| v.as_str()) {
                    call.arguments = raw.into();
                }
                self.emit_call(call)
            }
            "response.output_item.done" => {
                self.started = true;
                self.complete_item(&value["item"])
            }
            "response.completed" | "response.done" => {
                self.started = true;
                self.terminal = true;
                let mut pieces = Vec::new();
                for item in value
                    .pointer("/response/output")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    pieces.extend(self.complete_item(item)?);
                }
                self.ensure_complete()?;
                Ok(pieces)
            }
            "response.incomplete" => {
                self.started = true;
                self.terminal = true;
                let reason = value
                    .pointer("/response/incomplete_details/reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown reason");
                Err(format!("Responses response incomplete: {reason}"))
            }
            "response.failed" | "error" => {
                self.started = true;
                self.terminal = true;
                let message = value
                    .pointer("/response/error/message")
                    .or_else(|| value.pointer("/error/message"))
                    .or_else(|| value.get("message"))
                    .and_then(|v| v.as_str())
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
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
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
