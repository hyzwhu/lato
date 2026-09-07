// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-agent/src/plugins/trust.rs
// License: Apache-2.0
// Lato changes: reduces persistent per-plugin trust to the existing workspace folder-trust verdict

use crate::PluginScope;

pub fn source_is_trusted(scope: PluginScope, project_trusted: bool) -> bool {
    match scope {
        PluginScope::CliOverride | PluginScope::User => true,
        PluginScope::Project => project_trusted,
    }
}
