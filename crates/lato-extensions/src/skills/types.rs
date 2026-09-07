use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillDiscovery {
    pub generation: u64,
    pub skills: Vec<DiscoveredSkill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredSkill {
    pub plugin_name: String,
    pub plugin_root: PathBuf,
    pub skill_dir: PathBuf,
    pub source_path: PathBuf,
    pub name: String,
    pub description: String,
    pub has_authored_description: bool,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
    pub body: String,
    pub paths: Option<Vec<String>>,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: Option<BTreeMap<String, String>>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillDiagnostic {
    pub code: String,
    pub path: String,
    pub message: String,
}

impl SkillDiagnostic {
    pub fn bounded(code: &str, path: &Path, message: String, max_message_bytes: usize) -> Self {
        Self {
            code: code.to_owned(),
            path: truncate_bytes(path.to_string_lossy().into_owned(), max_message_bytes),
            message: truncate_bytes(message, max_message_bytes),
        }
    }
}

fn truncate_bytes(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}
