#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginReloadRequest {
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginReloadResponse {
    pub generation: u64,
    pub discovered: usize,
    pub active: usize,
    pub diagnostics: Vec<PluginDiagnosticDto>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDiagnosticDto {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_reload_response_is_stable_camel_case_json() {
        let response = PluginReloadResponse {
            generation: 7,
            discovered: 3,
            active: 2,
            diagnostics: vec![PluginDiagnosticDto {
                code: "plugin.untrusted_project".into(),
                plugin_id: Some("project/abcd1234/demo".into()),
                message: "project plugin is not trusted".into(),
            }],
        };
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["generation"], 7);
        assert_eq!(json["diagnostics"][0]["pluginId"], "project/abcd1234/demo");
    }

    #[test]
    fn reload_request_defaults_force_to_false() {
        assert!(
            !serde_json::from_str::<PluginReloadRequest>("{}")
                .unwrap()
                .force
        );
    }
}
