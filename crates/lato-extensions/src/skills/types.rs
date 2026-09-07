// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/skills/types.rs
// License: Apache-2.0
// Lato changes: models immutable plugin-scoped skill materialization and bounded diagnostics

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillInvocation {
    pub qualified_name: String,
    pub message: String,
    pub allowed_tools: Option<Arc<[String]>>,
    pub body_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SkillInvokeError {
    #[error("skill `{requested}` was not found")]
    NotFound { requested: String },
    #[error("skill `{requested}` is ambiguous; use one of: {candidates:?}")]
    Ambiguous {
        requested: String,
        candidates: Vec<String>,
    },
    #[error("skill `{qualified_name}` cannot be invoked by a user")]
    UserInvocationDisabled { qualified_name: String },
    #[error("skill `{qualified_name}` cannot be invoked by the model")]
    ModelInvocationDisabled { qualified_name: String },
    #[error("expanded skill body exceeds the {limit}-byte limit")]
    ExpansionTooLarge { limit: usize },
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
