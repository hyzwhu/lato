use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use lato_extensions::{
    DiscoveryConfig, PluginConfig, PluginSnapshot, build_snapshot, discover_plugins,
    skills::{
        MAX_SKILL_CANDIDATES, MAX_SKILL_DIRECTORIES_VISITED, MAX_SKILL_FILE_BYTES, discover_skills,
    },
};

struct Fixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    plugins: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("workspace");
        let home = temp.path().join("home");
        let plugins = temp.path().join("plugins");
        fs::create_dir_all(cwd.join(".lato/plugins")).unwrap();
        fs::create_dir_all(home.join("plugins")).unwrap();
        fs::create_dir_all(&plugins).unwrap();
        Self {
            _temp: temp,
            cwd,
            home,
            plugins,
        }
    }

    fn plugin(&self, name: &str) -> PathBuf {
        let root = self.plugins.join(name);
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::write(
            root.join("plugin.json"),
            format!(r#"{{"name":"{name}","skills":"skills"}}"#),
        )
        .unwrap();
        root
    }

    fn snapshot(&self, plugin_root: &Path, active: bool) -> Arc<PluginSnapshot> {
        let discovery = discover_plugins(&DiscoveryConfig {
            cwd: self.cwd.clone(),
            lato_home: self.home.clone(),
            cli_plugin_dirs: vec![plugin_root.to_owned()],
            project_trusted: true,
        });
        let config = if active {
            PluginConfig::default()
        } else {
            PluginConfig {
                enabled: vec![],
                disabled: vec!["demo".into()],
            }
        };
        build_snapshot(11, discovery, &config).unwrap()
    }
}

fn write_skill(root: &Path, relative_dir: &str, content: &str) -> PathBuf {
    let dir = root.join("skills").join(relative_dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SKILL.md");
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn discovers_root_and_nested_skill_in_sorted_order() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    fs::write(
        plugin.join("skills/SKILL.md"),
        "---\nname: root\ndescription: Root.\n---\nbody",
    )
    .unwrap();
    write_skill(
        &plugin,
        "zeta",
        "---\nname: zeta\ndescription: Zeta.\n---\nbody",
    );
    write_skill(
        &plugin,
        "alpha/nested",
        "---\nname: nested\ndescription: Nested.\n---\nbody",
    );

    let result = discover_skills(&fixture.snapshot(&plugin, true));

    assert_eq!(result.generation, 11);
    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        vec!["root", "nested", "zeta"]
    );
    assert!(
        result
            .skills
            .iter()
            .all(|skill| skill.plugin_name == "demo")
    );
}

#[test]
fn normalizes_frontmatter_then_falls_back_to_directory_name() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(
        &plugin,
        "a-first",
        "---\nname: Code Audit\ndescription: Review code safely.\nallowed-tools: read_file, Bash(git diff:*)\n---\nbody",
    );
    write_skill(
        &plugin,
        "z_Fallback_Name",
        "---\nname: 日本語\ndescription: fallback\n---\nbody",
    );

    let result = discover_skills(&fixture.snapshot(&plugin, true));
    let skill = &result.skills[0];
    assert_eq!(skill.name, "code-audit");
    assert_eq!(skill.description, "Review code safely.");
    assert!(skill.has_authored_description);
    assert_eq!(
        skill.allowed_tools.as_deref(),
        Some(&["read_file".into(), "Bash(git diff:*)".into()][..])
    );
    assert!(
        result
            .skills
            .iter()
            .any(|skill| skill.name == "z-fallback-name")
    );
}

#[test]
fn parses_allowed_tools_string_and_list() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(
        &plugin,
        "string",
        "---\ndescription: string\nallowed-tools: Read, Bash(git log --format=%h,%s) Search\n---\nbody",
    );
    write_skill(
        &plugin,
        "list",
        "---\ndescription: list\nallowed-tools:\n  - Read\n  - Bash(git diff:*)\n  - 42\n---\nbody",
    );

    let result = discover_skills(&fixture.snapshot(&plugin, true));
    let string = result
        .skills
        .iter()
        .find(|skill| skill.name == "string")
        .unwrap();
    let list = result
        .skills
        .iter()
        .find(|skill| skill.name == "list")
        .unwrap();
    assert_eq!(
        string.allowed_tools.as_deref(),
        Some(
            &[
                "Read".into(),
                "Bash(git log --format=%h,%s)".into(),
                "Search".into()
            ][..]
        )
    );
    assert_eq!(
        list.allowed_tools.as_deref(),
        Some(&["Read".into(), "Bash(git diff:*)".into()][..])
    );
}

#[test]
fn uses_body_prose_only_as_fallback() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(
        &plugin,
        "inspect",
        "---\nwhen-to-use: During review\n---\n# Inspect\n\nReview the implementation safely.\n\n- Ignore this list.",
    );

    let result = discover_skills(&fixture.snapshot(&plugin, true));
    let skill = &result.skills[0];
    assert_eq!(skill.description, "Review the implementation safely.");
    assert!(!skill.has_authored_description);
    assert_eq!(skill.when_to_use.as_deref(), Some("During review"));
    assert!(skill.body.contains("# Inspect"));
}

#[test]
fn body_fallback_obeys_markdown_block_semantics() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(
        &plugin,
        "markdown",
        concat!(
            "---\nwhen-to-use: During review\n---\n",
            "```markdown\n# Not a title\nFake prose.\n```\n\n",
            "![image prose](image.png)\n\n",
            "- list prose\n\n> quote prose\n\n",
            "| table | prose |\n| --- | --- |\n| no | no |\n\n",
            "Real [linked](https://example.invalid) `inline` prose.\n",
        ),
    );

    let result = discover_skills(&fixture.snapshot(&plugin, true));

    assert_eq!(result.skills[0].description, "Real linked inline prose.");
}

#[test]
fn malformed_optional_values_are_diagnosed_and_dropped() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(
        &plugin,
        "malformed-optionals",
        concat!(
            "---\n",
            "description: valid\n",
            "argument-hint: [not, scalar]\n",
            "allowed-tools: [Read, 42, Bash(git diff:*)]\n",
            "metadata:\n  valid: value\n  invalid: 42\n  7: bad-key\n",
            "license: {not: scalar}\n",
            "user-invocable: [true]\n",
            "---\nbody\n",
        ),
    );

    let result = discover_skills(&fixture.snapshot(&plugin, true));
    let skill = &result.skills[0];
    assert_eq!(skill.argument_hint, None);
    assert_eq!(
        skill.allowed_tools.as_deref(),
        Some(&["Read".into(), "Bash(git diff:*)".into()][..])
    );
    assert_eq!(
        skill
            .metadata
            .as_ref()
            .and_then(|values| values.get("valid")),
        Some(&"value".to_owned())
    );
    assert_eq!(skill.license, None);
    assert!(skill.user_invocable);
    for code in [
        "skill.invalid_argument_hint",
        "skill.invalid_allowed_tool",
        "skill.invalid_metadata_entry",
        "skill.invalid_license",
        "skill.invalid_user_invocable",
    ] {
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == code),
            "missing diagnostic {code}"
        );
    }
}

#[test]
fn sorts_by_canonical_path_before_plugin_identity() {
    let fixture = Fixture::new();
    let alpha_path = fixture.plugins.join("a-path");
    let zeta_path = fixture.plugins.join("z-path");
    for (root, name, skill) in [
        (&alpha_path, "zeta-plugin", "alpha-skill"),
        (&zeta_path, "alpha-plugin", "zeta-skill"),
    ] {
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::write(
            root.join("plugin.json"),
            format!(r#"{{"name":"{name}","skills":"skills"}}"#),
        )
        .unwrap();
        write_skill(root, skill, "---\ndescription: sorted\n---\nbody");
    }
    let discovery = discover_plugins(&DiscoveryConfig {
        cwd: fixture.cwd.clone(),
        lato_home: fixture.home.clone(),
        cli_plugin_dirs: vec![zeta_path, alpha_path],
        project_trusted: true,
    });
    let snapshot = build_snapshot(11, discovery, &PluginConfig::default()).unwrap();

    let result = discover_skills(&snapshot);

    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| skill.plugin_name.as_str())
            .collect::<Vec<_>>(),
        vec!["zeta-plugin", "alpha-plugin"]
    );
}

#[test]
fn same_canonical_skill_is_retained_for_each_plugin_identity() {
    let fixture = Fixture::new();
    let outer = fixture.plugins.join("outer");
    let inner = outer.join("nested");
    fs::create_dir_all(inner.join("shared/skill")).unwrap();
    fs::write(
        outer.join("plugin.json"),
        r#"{"name":"outer","skills":"nested/shared"}"#,
    )
    .unwrap();
    fs::write(
        inner.join("plugin.json"),
        r#"{"name":"inner","skills":"shared"}"#,
    )
    .unwrap();
    fs::write(
        inner.join("shared/skill/SKILL.md"),
        "---\ndescription: shared\n---\nbody",
    )
    .unwrap();
    let discovery = discover_plugins(&DiscoveryConfig {
        cwd: fixture.cwd.clone(),
        lato_home: fixture.home.clone(),
        cli_plugin_dirs: vec![outer, inner],
        project_trusted: true,
    });
    let snapshot = build_snapshot(11, discovery, &PluginConfig::default()).unwrap();

    let result = discover_skills(&snapshot);

    assert_eq!(result.skills.len(), 2);
    assert_eq!(result.skills[0].source_path, result.skills[1].source_path);
    assert_ne!(result.skills[0].plugin_name, result.skills[1].plugin_name);
}

#[cfg(unix)]
#[test]
fn rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    let outside = fixture._temp.path().join("outside");
    write_skill(&outside, "escape", "---\ndescription: escaped\n---\nbody");
    symlink(outside.join("skills/escape"), plugin.join("skills/escape")).unwrap();

    let result = discover_skills(&fixture.snapshot(&plugin, true));
    assert!(result.skills.is_empty());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "skill.path_escape")
    );
}

#[cfg(unix)]
#[test]
fn never_recurses_into_an_external_symlink_directory_tree() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    let outside = fixture._temp.path().join("wide-outside");
    for index in 0..64 {
        write_skill(
            &outside,
            &format!("branch-{index:03}"),
            "---\ndescription: escaped\n---\nbody",
        );
    }
    symlink(outside.join("skills"), plugin.join("skills/external")).unwrap();

    let result = discover_skills(&fixture.snapshot(&plugin, true));

    assert!(result.skills.is_empty());
    assert_eq!(
        result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "skill.path_escape")
            .count(),
        1,
        "one directory-level diagnostic proves the wide target was not traversed"
    );
}

#[test]
fn candidate_limit_is_bounded_and_deterministic() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    for index in 0..=MAX_SKILL_CANDIDATES {
        write_skill(
            &plugin,
            &format!("candidate-{index:04}"),
            "---\ndescription: candidate\n---\nbody",
        );
    }

    let result = discover_skills(&fixture.snapshot(&plugin, true));

    assert_eq!(result.skills.len(), MAX_SKILL_CANDIDATES);
    assert_eq!(result.skills[0].name, "candidate-0000");
    assert_eq!(
        result.skills.last().unwrap().name,
        format!("candidate-{:04}", MAX_SKILL_CANDIDATES - 1)
    );
    assert_eq!(
        result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "skill.candidate_limit")
            .count(),
        1
    );
}

#[test]
fn directory_visit_limit_is_bounded_and_deterministic() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    for index in 0..MAX_SKILL_DIRECTORIES_VISITED {
        fs::create_dir_all(plugin.join(format!("skills/directory-{index:04}"))).unwrap();
    }

    let result = discover_skills(&fixture.snapshot(&plugin, true));

    let diagnostics = result
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "skill.directory_limit")
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 1);
    assert!(
        diagnostics[0].path.ends_with("skills/directory-2047"),
        "first excluded directory must be stable: {}",
        diagnostics[0].path
    );
}

#[test]
fn caps_walk_at_depth_five() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(
        &plugin,
        "one/two/three/four/five/six",
        "---\ndescription: included\n---\nbody",
    );
    write_skill(
        &plugin,
        "one/two/three/four/five/six/seven",
        "---\ndescription: excluded\n---\nbody",
    );

    let result = discover_skills(&fixture.snapshot(&plugin, true));
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "six");
}

#[test]
fn isolates_oversized_and_malformed_candidates() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(&plugin, "good", "---\ndescription: good\n---\nGood body.");
    let oversized = write_skill(&plugin, "oversized", "");
    fs::write(&oversized, vec![b'x'; MAX_SKILL_FILE_BYTES + 1]).unwrap();
    let malformed = write_skill(&plugin, "malformed", "");
    fs::write(&malformed, [0xff, 0xfe, 0xfd]).unwrap();

    let result = discover_skills(&fixture.snapshot(&plugin, true));
    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].name, "good");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "skill.file_too_large")
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "skill.invalid_utf8")
    );
}

#[test]
fn inactive_plugins_contribute_nothing() {
    let fixture = Fixture::new();
    let plugin = fixture.plugin("demo");
    write_skill(&plugin, "hidden", "---\ndescription: hidden\n---\nbody");

    let result = discover_skills(&fixture.snapshot(&plugin, false));
    assert_eq!(result.generation, 11);
    assert!(result.skills.is_empty());
    assert!(result.diagnostics.is_empty());
}
