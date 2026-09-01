use lato_core::{
    ModelCallId, ModelContent, ModelError, ModelMessage, ModelRequest, ModelRole, ModelSelection,
    Retryability, SamplingParameters, SideEffect, ToolCallId, ToolCancellation, ToolChoice,
    ToolConcurrency, ToolDescriptor, ToolIdempotency, ToolLayer, ToolName, ToolSource,
};
use semver::Version;
use serde_json::{Map, Value, json};

#[allow(dead_code, reason = "consumed by LegacyModelPort in the next task")]
pub(crate) fn decode_legacy_request(
    call_id: ModelCallId,
    selection: ModelSelection,
    context: Value,
) -> Result<ModelRequest, ModelError> {
    let object = context
        .as_object()
        .ok_or_else(|| invalid_request("legacy model context must be an object"))?;
    let messages = required_array(object, "messages")?
        .iter()
        .map(decode_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = match object.get("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(tools)) => tools
            .iter()
            .map(decode_tool)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(invalid_request("legacy tools must be an array")),
    };
    let parameters = SamplingParameters {
        temperature: optional_f32(object, "temperature")?,
        max_output_tokens: optional_u64(object, "max_output_tokens")?
            .or(optional_u64(object, "max_tokens")?),
        tool_choice: decode_tool_choice(object.get("tool_choice"))?,
        response_schema: object.get("response_schema").cloned(),
    };

    Ok(ModelRequest {
        call_id,
        selection,
        messages,
        tools,
        parameters,
    })
}

#[allow(dead_code, reason = "consumed by LegacyModelPort in the next task")]
pub(crate) fn encode_legacy_request(request: &ModelRequest) -> Result<Value, ModelError> {
    let messages = request
        .messages
        .iter()
        .map(encode_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = request
        .tools
        .iter()
        .map(encode_tool)
        .collect::<Result<Vec<_>, _>>()?;

    let mut context = Map::from_iter([
        ("messages".into(), Value::Array(messages)),
        ("tools".into(), Value::Array(tools)),
        ("stream".into(), Value::Bool(true)),
    ]);
    if let Some(temperature) = request.parameters.temperature {
        context.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_output_tokens) = request.parameters.max_output_tokens {
        context.insert("max_output_tokens".into(), json!(max_output_tokens));
    }
    if let Some(tool_choice) = &request.parameters.tool_choice {
        context.insert("tool_choice".into(), encode_tool_choice(tool_choice)?);
    }
    if let Some(response_schema) = &request.parameters.response_schema {
        context.insert("response_schema".into(), response_schema.clone());
    }
    Ok(Value::Object(context))
}

fn decode_message(value: &Value) -> Result<ModelMessage, ModelError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_request("legacy message must be an object"))?;
    let role = required_str(object, "role")?;
    let (role, content) = match role {
        "system" => (
            ModelRole::System,
            vec![ModelContent::Text {
                text: required_str(object, "content")?.to_owned(),
            }],
        ),
        "user" => (
            ModelRole::User,
            vec![ModelContent::Text {
                text: required_str(object, "content")?.to_owned(),
            }],
        ),
        "assistant" => (ModelRole::Assistant, decode_assistant_content(object)?),
        "tool" => (
            ModelRole::Tool,
            vec![ModelContent::ToolResult {
                call_id: parse_tool_call_id(required_str(object, "tool_call_id")?)?,
                output: required_str(object, "content")?.to_owned(),
            }],
        ),
        other => {
            return Err(invalid_request(format!(
                "unsupported legacy role '{other}'"
            )));
        }
    };
    Ok(ModelMessage { role, content })
}

fn decode_assistant_content(object: &Map<String, Value>) -> Result<Vec<ModelContent>, ModelError> {
    let mut content = Vec::new();
    match object.get("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            content.push(ModelContent::Text { text: text.clone() });
        }
        None | Some(Value::Null) | Some(Value::String(_)) => {}
        Some(_) => {
            return Err(invalid_request(
                "assistant content must be a string or null",
            ));
        }
    }
    match object.get("tool_calls") {
        None | Some(Value::Null) => {}
        Some(Value::Array(calls)) => {
            for call in calls {
                content.push(decode_tool_call(call)?);
            }
        }
        Some(_) => return Err(invalid_request("assistant tool_calls must be an array")),
    }
    Ok(content)
}

fn decode_tool_call(value: &Value) -> Result<ModelContent, ModelError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_request("legacy tool call must be an object"))?;
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return Err(invalid_request("legacy tool call type must be function"));
    }
    let function = required_object(object, "function")?;
    let arguments_text = required_str(function, "arguments")?;
    let arguments = serde_json::from_str(arguments_text)
        .map_err(|error| invalid_request(format!("invalid legacy tool arguments: {error}")))?;
    Ok(ModelContent::ToolCall {
        call_id: parse_tool_call_id(required_str(object, "id")?)?,
        name: legacy_tool_name(required_str(function, "name")?)?,
        arguments,
    })
}

fn decode_tool(value: &Value) -> Result<ToolDescriptor, ModelError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_request("legacy tool definition must be an object"))?;
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return Err(invalid_request(
            "legacy tool definition type must be function",
        ));
    }
    let function = required_object(object, "function")?;
    let descriptor = ToolDescriptor {
        name: legacy_tool_name(required_str(function, "name")?)?,
        version: Version::new(1, 0, 0),
        description: required_str(function, "description")?.to_owned(),
        input_schema: function
            .get("parameters")
            .cloned()
            .ok_or_else(|| invalid_request("legacy tool definition missing parameters"))?,
        capabilities: Vec::new(),
        side_effect: SideEffect::None,
        concurrency: ToolConcurrency::Parallel,
        idempotency: ToolIdempotency::Idempotent,
        timeout_ms: 30_000,
        max_output_bytes: 65_536,
        cancellation: ToolCancellation::Cooperative,
        source: ToolSource {
            layer: ToolLayer::Builtin,
            id: "legacy-model-context".into(),
            replacement: None,
        },
    };
    descriptor
        .validate()
        .map_err(|error| invalid_request(format!("invalid legacy tool definition: {error}")))?;
    Ok(descriptor)
}

fn encode_message(message: &ModelMessage) -> Result<Value, ModelError> {
    match message.role {
        ModelRole::System => encode_text_message("system", &message.content),
        ModelRole::User => encode_text_message("user", &message.content),
        ModelRole::Assistant => encode_assistant_message(&message.content),
        ModelRole::Tool => encode_tool_result_message(&message.content),
    }
}

fn encode_text_message(role: &str, content: &[ModelContent]) -> Result<Value, ModelError> {
    let text = joined_text(content)?;
    Ok(json!({"role": role, "content": text}))
}

fn encode_assistant_message(content: &[ModelContent]) -> Result<Value, ModelError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in content {
        match item {
            ModelContent::Text { text: chunk } => text.push_str(chunk),
            ModelContent::ToolCall {
                call_id,
                name,
                arguments,
            } => calls.push(json!({
                "id": call_id.as_str(),
                "type": "function",
                "function": {
                    "name": wire_tool_name(name)?,
                    "arguments": serde_json::to_string(arguments).map_err(|error| {
                        invalid_request(format!("cannot encode tool arguments: {error}"))
                    })?,
                }
            })),
            _ => {
                return Err(invalid_request(
                    "assistant messages may contain only text and tool calls",
                ));
            }
        }
    }
    if calls.is_empty() {
        Ok(json!({"role": "assistant", "content": text}))
    } else {
        Ok(json!({
            "role": "assistant",
            "content": if text.is_empty() { Value::Null } else { Value::String(text) },
            "tool_calls": calls,
        }))
    }
}

fn encode_tool_result_message(content: &[ModelContent]) -> Result<Value, ModelError> {
    let [ModelContent::ToolResult { call_id, output }] = content else {
        return Err(invalid_request(
            "tool messages must contain exactly one tool result",
        ));
    };
    Ok(json!({
        "role": "tool",
        "tool_call_id": call_id.as_str(),
        "content": output,
    }))
}

fn encode_tool(descriptor: &ToolDescriptor) -> Result<Value, ModelError> {
    Ok(json!({
        "type": "function",
        "function": {
            "name": wire_tool_name(&descriptor.name)?,
            "description": descriptor.description,
            "parameters": descriptor.input_schema,
        }
    }))
}

fn joined_text(content: &[ModelContent]) -> Result<String, ModelError> {
    let mut text = String::new();
    for item in content {
        let ModelContent::Text { text: chunk } = item else {
            return Err(invalid_request(
                "system and user messages may contain only text",
            ));
        };
        text.push_str(chunk);
    }
    Ok(text)
}

fn decode_tool_choice(value: Option<&Value>) -> Result<Option<ToolChoice>, ModelError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(choice)) => match choice.as_str() {
            "auto" => Ok(Some(ToolChoice::Auto)),
            "none" => Ok(Some(ToolChoice::None)),
            "required" => Ok(Some(ToolChoice::Required)),
            other => Err(invalid_request(format!(
                "unsupported legacy tool choice '{other}'"
            ))),
        },
        Some(Value::Object(choice)) => {
            let function = required_object(choice, "function")?;
            Ok(Some(ToolChoice::Specific(legacy_tool_name(required_str(
                function, "name",
            )?)?)))
        }
        Some(_) => Err(invalid_request("legacy tool choice has an invalid shape")),
    }
}

fn encode_tool_choice(choice: &ToolChoice) -> Result<Value, ModelError> {
    Ok(match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Specific(name) => json!({
            "type": "function",
            "function": {"name": wire_tool_name(name)?},
        }),
    })
}

fn legacy_tool_name(name: &str) -> Result<ToolName, ModelError> {
    ToolName::parse(format!("legacy:{name}"))
        .map_err(|error| invalid_request(format!("invalid legacy tool name '{name}': {error}")))
}

fn wire_tool_name(name: &ToolName) -> Result<&str, ModelError> {
    if name.namespace() != "legacy" {
        return Err(invalid_request(format!(
            "tool '{name}' is not available through the legacy provider adapter"
        )));
    }
    Ok(name.local_name())
}

fn parse_tool_call_id(value: &str) -> Result<ToolCallId, ModelError> {
    ToolCallId::parse(value)
        .map_err(|error| invalid_request(format!("invalid legacy tool call ID: {error}")))
}

fn required_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Vec<Value>, ModelError> {
    object
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_request(format!("legacy context '{key}' must be an array")))
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, ModelError> {
    object
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_request(format!("legacy '{key}' must be an object")))
}

fn required_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, ModelError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_request(format!("legacy '{key}' must be a string")))
}

fn optional_f32(object: &Map<String, Value>, key: &str) -> Result<Option<f32>, ModelError> {
    object
        .get(key)
        .map(|value| {
            value
                .as_f64()
                .map(|number| number as f32)
                .ok_or_else(|| invalid_request(format!("legacy '{key}' must be a number")))
        })
        .transpose()
}

fn optional_u64(object: &Map<String, Value>, key: &str) -> Result<Option<u64>, ModelError> {
    object
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid_request(format!("legacy '{key}' must be an integer")))
        })
        .transpose()
}

fn invalid_request(message: impl Into<String>) -> ModelError {
    ModelError::new("model.invalid_request", message, Retryability::Never)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lato_core::{
        ModelContent, ModelRole, SideEffect, ToolCancellation, ToolConcurrency, ToolIdempotency,
        ToolLayer,
    };
    use serde_json::json;

    fn selection() -> ModelSelection {
        ModelSelection::new("openai", "gpt-test").unwrap()
    }

    fn legacy_context() -> Value {
        json!({
            "messages": [
                {"role": "system", "content": "be exact"},
                {"role": "user", "content": "read it"},
                {
                    "role": "assistant",
                    "content": "checking",
                    "tool_calls": [{
                        "id": "call-7",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "call-7", "content": "contents"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }
            }],
            "tool_choice": "required"
        })
    }

    #[test]
    fn decodes_current_actor_context_without_losing_tool_identity() {
        let request = decode_legacy_request(
            ModelCallId::from("model-call-1"),
            selection(),
            legacy_context(),
        )
        .unwrap();

        assert_eq!(request.selection, selection());
        assert_eq!(request.call_id.as_str(), "model-call-1");
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.messages[0].role, ModelRole::System);
        assert_eq!(request.messages[1].role, ModelRole::User);
        assert_eq!(request.messages[2].role, ModelRole::Assistant);
        assert_eq!(request.messages[2].content.len(), 2);
        assert!(matches!(
            &request.messages[2].content[0],
            ModelContent::Text { text } if text == "checking"
        ));
        assert!(matches!(
            &request.messages[2].content[1],
            ModelContent::ToolCall { call_id, name, arguments }
                if call_id.as_str() == "call-7"
                    && name.as_str() == "legacy:read_file"
                    && arguments == &json!({"path": "a.rs"})
        ));
        assert!(matches!(
            &request.messages[3].content[0],
            ModelContent::ToolResult { call_id, output }
                if call_id.as_str() == "call-7" && output == "contents"
        ));

        let descriptor = &request.tools[0];
        assert_eq!(descriptor.name.as_str(), "legacy:read_file");
        assert_eq!(descriptor.side_effect, SideEffect::None);
        assert_eq!(descriptor.concurrency, ToolConcurrency::Parallel);
        assert_eq!(descriptor.idempotency, ToolIdempotency::Idempotent);
        assert_eq!(descriptor.cancellation, ToolCancellation::Cooperative);
        assert_eq!(descriptor.timeout_ms, 30_000);
        assert_eq!(descriptor.max_output_bytes, 65_536);
        assert_eq!(descriptor.source.layer, ToolLayer::Builtin);
        assert_eq!(descriptor.source.id, "legacy-model-context");
    }

    #[test]
    fn encodes_back_to_the_existing_provider_wire_shape() {
        let request = decode_legacy_request(
            ModelCallId::from("model-call-2"),
            selection(),
            legacy_context(),
        )
        .unwrap();

        let encoded = encode_legacy_request(&request).unwrap();
        assert_eq!(encoded["stream"], true);
        assert_eq!(encoded["tool_choice"], "required");
        assert_eq!(encoded["tools"][0]["function"]["name"], "read_file");
        assert_eq!(
            encoded["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.rs\"}"
        );
        assert_eq!(encoded["messages"][3]["tool_call_id"], "call-7");
    }

    #[test]
    fn malformed_tool_arguments_are_a_typed_invalid_request() {
        let mut context = legacy_context();
        context["messages"][2]["tool_calls"][0]["function"]["arguments"] = json!("{");

        let error = decode_legacy_request(ModelCallId::from("model-call-3"), selection(), context)
            .unwrap_err();

        assert_eq!(error.code, "model.invalid_request");
    }
}
