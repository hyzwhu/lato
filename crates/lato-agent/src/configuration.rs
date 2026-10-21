//! Trusted, application-owned configuration for a host's model-facing surface.

use std::{collections::HashSet, io::Read, path::Path};

use lato_core::{ModelContent, ModelMessage, ModelRole, ToolName, journal_request_hash};
use serde::{Deserialize, Serialize};

const MAX_CONFIGURATION_BYTES: u64 = 128 * 1024;
const MARKER: &str = "Lato application configuration v1: ";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    #[default]
    Coding,
    General,
}

/// Selects an application identity and an exact catalog of callable tools.
/// This narrows tool access; existing approvals and server authorization still apply.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentConfiguration {
    #[serde(default)]
    pub mode: AgentMode,
    #[serde(default)]
    pub instructions: String,
    /// Canonical names, e.g. `business:lookup_customer`. `Some([])` disables all tools.
    /// General mode requires this field; coding mode defaults to the existing catalog.
    #[serde(default)]
    pub tools: Option<Vec<ToolName>>,
    /// Defaults to true for coding and false for general mode.
    #[serde(default)]
    pub workspace_instructions: Option<bool>,
}

impl AgentConfiguration {
    pub fn from_home(home: &Path) -> Result<Self, String> {
        Self::load(Some(home))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.instructions.len() > 64 * 1024 {
            return Err("agent.invalid_configuration: instructions exceed 64 KiB".into());
        }
        if self.mode == AgentMode::General && self.tools.is_none() {
            return Err(
                "agent.invalid_configuration: general mode requires an explicit tools list".into(),
            );
        }
        if let Some(tools) = &self.tools {
            if tools.len() > 256 {
                return Err("agent.invalid_configuration: at most 256 tools are allowed".into());
            }
            let mut seen = HashSet::new();
            for tool in tools {
                // ToolName's transparent serde representation does not validate itself.
                ToolName::parse(tool.as_str()).map_err(|_| {
                    "agent.invalid_configuration: tools must use valid canonical names".to_owned()
                })?;
                if !seen.insert(tool) {
                    return Err("agent.invalid_configuration: duplicate tool name".into());
                }
                // These managers create independent sessions. Application inheritance
                // must be implemented before a configured catalog can safely expose them.
                if matches!(
                    tool.as_str(),
                    "builtin:spawn"
                        | "builtin:send"
                        | "builtin:wait"
                        | "builtin:cancel"
                        | "builtin:inspect"
                        | "builtin:agentfield"
                ) {
                    return Err(
                        "agent.invalid_configuration: configured delegation is not supported yet"
                            .into(),
                    );
                }
            }
        }
        // A custom identity without a catalog could otherwise delegate to a default
        // coding child and lose its application instructions.
        if !self.is_legacy_default() && self.tools.is_none() {
            return Err(
                "agent.invalid_configuration: a custom agent requires an explicit tools list"
                    .into(),
            );
        }
        Ok(())
    }

    /// Loads a bounded local JSON file. Absence is handled by the host, not this method.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        if !std::fs::metadata(path)
            .map_err(|e| format!("agent.configuration_read: {e}"))?
            .is_file()
        {
            return Err("agent.invalid_configuration: configuration must be a regular file".into());
        }
        let file =
            std::fs::File::open(path).map_err(|e| format!("agent.configuration_read: {e}"))?;
        if !file
            .metadata()
            .map_err(|e| format!("agent.configuration_read: {e}"))?
            .is_file()
        {
            return Err("agent.invalid_configuration: configuration must be a regular file".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIGURATION_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("agent.configuration_read: {e}"))?;
        if bytes.len() as u64 > MAX_CONFIGURATION_BYTES {
            return Err("agent.invalid_configuration: configuration exceeds 128 KiB".into());
        }
        let value: Self = serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "agent.invalid_configuration: invalid agent JSON at line {}, column {}",
                e.line(),
                e.column()
            )
        })?;
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn load(home: Option<&Path>) -> Result<Self, String> {
        let Some(home) = home.filter(|p| !p.as_os_str().is_empty()) else {
            return Ok(Self::default());
        };
        let path = home.join("agent.json");
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("agent.configuration_read: {e}")),
            Ok(_) => Self::from_file(&path),
        }
    }

    pub(crate) fn is_legacy_default(&self) -> bool {
        self == &Self::default()
    }

    pub(crate) fn marker(&self) -> String {
        format!(
            "{MARKER}{}",
            journal_request_hash("agent_configuration_v1", &serde_json::json!(self))
        )
    }

    pub(crate) fn loads_workspace_instructions(&self) -> bool {
        self.workspace_instructions
            .unwrap_or(self.mode == AgentMode::Coding)
    }

    pub(crate) fn verify_replay(&self, messages: &[ModelMessage]) -> Result<(), String> {
        if messages.is_empty() {
            return Ok(());
        }
        let first_line = messages
            .iter()
            .find(|m| m.role == ModelRole::System)
            .and_then(|m| m.content.first())
            .and_then(|c| match c {
                ModelContent::Text { text } => text.lines().next(),
                _ => None,
            });
        let matches = if self.is_legacy_default() {
            !first_line.is_some_and(|line| line.starts_with(MARKER))
        } else {
            first_line == Some(self.marker().as_str())
        };
        if matches {
            Ok(())
        } else {
            Err("agent.configuration_mismatch: resume requires the original application configuration; restore agent.json or create a new session".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_strict_general_configuration() {
        AgentConfiguration::default().validate().unwrap();
        for value in [
            serde_json::json!({"mode":"general"}),
            serde_json::json!({"mode":"general", "tools":["shell"]}),
            serde_json::json!({"mode":"general", "tools":["builtin:spawn"]}),
            serde_json::json!({"mode":"general", "tools":["biz:read", "biz:read"]}),
            serde_json::json!({"instructions":"custom"}),
        ] {
            let config: AgentConfiguration = serde_json::from_value(value).unwrap();
            assert!(config.validate().is_err());
        }
        let general: AgentConfiguration =
            serde_json::from_value(serde_json::json!({"mode":"general","tools":[]})).unwrap();
        general.validate().unwrap();
        assert!(!general.loads_workspace_instructions());
        assert!(
            serde_json::from_value::<AgentConfiguration>(serde_json::json!({"toolz":[]})).is_err()
        );
    }

    #[test]
    fn invalid_files_fail_without_echoing_instructions() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("agent.json");
        std::fs::write(&path, br#"{"mode":"SECRET_PRIVATE_INSTRUCTION"}"#).unwrap();
        let error = AgentConfiguration::from_file(&path).unwrap_err();
        assert!(!error.contains("SECRET_PRIVATE_INSTRUCTION"));
        std::fs::write(&path, vec![b' '; MAX_CONFIGURATION_BYTES as usize + 1]).unwrap();
        assert!(
            AgentConfiguration::from_file(&path)
                .unwrap_err()
                .contains("128 KiB")
        );
    }
}
