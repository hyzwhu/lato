//! MCP JSON-RPC envelopes used by the bounded runtime.
//!
//! Task 2 covers initialize + notifications/initialized. tools/list and
//! tools/call envelopes are shaped here for later tasks but are not wired
//! into ToolRuntime from this crate.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::McpError;

/// Streamable HTTP was introduced in 2025-03-26; stdio servers may still
/// negotiate an older version in `InitializeResult`.
pub const PROTOCOL_VERSION: &str = "2025-03-26";
pub const CLIENT_NAME: &str = "lato";
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct JsonRpcResponse {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcErrorObject>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server_info: ServerInfo,
    pub capabilities: Value,
}

pub fn request(id: u64, method: impl Into<String>, params: Option<Value>) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0",
        id,
        method: method.into(),
        params,
    }
}

pub fn notification(method: impl Into<String>, params: Option<Value>) -> JsonRpcNotification {
    JsonRpcNotification {
        jsonrpc: "2.0",
        method: method.into(),
        params,
    }
}

pub fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": CLIENT_NAME,
            "version": CLIENT_VERSION,
        }
    })
}

pub fn parse_response_value(value: Value) -> Result<Value, McpError> {
    let response: JsonRpcResponse =
        serde_json::from_value(value).map_err(|error| McpError::protocol(error.to_string()))?;
    if let Some(error) = response.error {
        return Err(McpError::rpc(error.code, error.message));
    }
    Ok(response.result.unwrap_or(Value::Null))
}

pub fn parse_initialize_result(result: Value) -> Result<InitializeResult, McpError> {
    let object = result
        .as_object()
        .ok_or_else(|| McpError::protocol("initialize result must be an object"))?;
    let protocol_version = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION)
        .to_owned();
    let server_info = parse_server_info(object.get("serverInfo"))?;
    let capabilities = object
        .get("capabilities")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    Ok(InitializeResult {
        protocol_version,
        server_info,
        capabilities,
    })
}

fn parse_server_info(value: Option<&Value>) -> Result<ServerInfo, McpError> {
    let Some(object) = value.and_then(Value::as_object) else {
        return Ok(ServerInfo {
            name: "unknown".into(),
            version: "0".into(),
        });
    };
    Ok(ServerInfo {
        name: object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        version: object
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("0")
            .to_owned(),
    })
}

pub fn tools_call_params(name: &str, arguments: Value) -> Value {
    serde_json::json!({
        "name": name,
        "arguments": arguments,
    })
}

pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, McpError> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|error| McpError::protocol(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}
