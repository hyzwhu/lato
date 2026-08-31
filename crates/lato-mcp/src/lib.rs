use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginOrigin {
    Cli,
    User,
    Project,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginPackage {
    pub root: PathBuf,
    pub origin: PluginOrigin,
    pub trusted: bool,
    pub skills: Vec<PathBuf>,
    pub hooks_enabled: bool,
    pub mcp_enabled: bool,
}

pub fn discover_plugin(
    root: &Path,
    origin: PluginOrigin,
    project_trusted: bool,
) -> Result<PluginPackage, String> {
    if !root.is_dir() {
        return Err(format!("plugin directory not found: {}", root.display()));
    }
    let trusted = match origin {
        PluginOrigin::Cli | PluginOrigin::User => true,
        PluginOrigin::Project => project_trusted,
    };
    let mut skills = Vec::new();
    collect_skills(&root.join("skills"), &mut skills)?;
    let hooks_present = root.join("hooks").is_dir() || root.join("hooks.json").is_file();
    let mcp_present = root.join(".mcp.json").is_file();
    Ok(PluginPackage {
        root: root.to_path_buf(),
        origin,
        trusted,
        skills,
        hooks_enabled: trusted && hooks_present,
        mcp_enabled: trusted && mcp_present,
    })
}

fn collect_skills(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            let skill = path.join("SKILL.md");
            if skill.is_file() {
                out.push(skill);
            }
        }
    }
    out.sort();
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub description: String,
}

pub fn search_tools(tools: &[McpTool], query: &str) -> Vec<McpTool> {
    let q = query.to_lowercase();
    tools
        .iter()
        .filter(|tool| {
            tool.name.to_lowercase().contains(&q)
                || tool.description.to_lowercase().contains(&q)
                || tool.server.to_lowercase().contains(&q)
        })
        .cloned()
        .collect()
}

pub fn qualified_tool_name(tool: &McpTool) -> String {
    format!("{}__{}", tool.server, tool.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e5_1_project_plugin_executable_parts_require_trust() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("skills/demo")).unwrap();
        std::fs::create_dir_all(d.path().join("hooks")).unwrap();
        std::fs::write(d.path().join("skills/demo/SKILL.md"), "# demo").unwrap();
        std::fs::write(d.path().join(".mcp.json"), "{}").unwrap();
        let untrusted = discover_plugin(d.path(), PluginOrigin::Project, false).unwrap();
        assert_eq!(untrusted.skills.len(), 1);
        assert!(!untrusted.hooks_enabled);
        assert!(!untrusted.mcp_enabled);
        let trusted = discover_plugin(d.path(), PluginOrigin::Project, true).unwrap();
        assert!(trusted.hooks_enabled);
        assert!(trusted.mcp_enabled);
    }

    #[test]
    fn e5_1_cli_and_user_plugins_are_trusted() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".mcp.json"), "{}").unwrap();
        assert!(
            discover_plugin(d.path(), PluginOrigin::Cli, false)
                .unwrap()
                .mcp_enabled
        );
        assert!(
            discover_plugin(d.path(), PluginOrigin::User, false)
                .unwrap()
                .mcp_enabled
        );
    }

    #[test]
    fn e5_1_mcp_progressive_discovery_uses_qualified_names() {
        let tools = vec![McpTool {
            server: "git".into(),
            name: "status".into(),
            description: "repository state".into(),
        }];
        let found = search_tools(&tools, "repo");
        assert_eq!(qualified_tool_name(&found[0]), "git__status");
    }
}
