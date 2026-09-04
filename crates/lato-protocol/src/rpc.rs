#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcReq {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0", "id": id, "result": result})
}

pub fn err(
    id: Option<serde_json::Value>,
    code: i64,
    message: impl AsRef<str>,
) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0", "id": id, "error": {"code": code, "message": message.as_ref()}})
}

pub fn err_with_data(
    id: Option<serde_json::Value>,
    code: i64,
    message: impl AsRef<str>,
    data: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0", "id": id, "error": {"code": code, "message": message.as_ref(), "data": data}})
}
