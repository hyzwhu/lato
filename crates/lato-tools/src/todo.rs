pub fn todo_write(items: &[String]) -> serde_json::Value {
    serde_json::json!({"todos": items})
}
