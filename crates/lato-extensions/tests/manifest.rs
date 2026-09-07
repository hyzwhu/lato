use std::{fs, path::Path};

use lato_extensions::{ManifestError, ManifestLoadResult, load_manifest, name_from_dirname};

#[test]
fn canonical_manifest_is_forward_compatible_and_resolves_components() {
    let root = fixture_plugin(
        r#"{
          "name":"demo-plugin",
          "futureField":true,
          "skills":["skills","extra-skills"],
          "hooks":"hooks/hooks.json",
          "mcpServers":{"demo":{"command":"demo"}}
        }"#,
    );
    create_dir(root.path(), "skills");
    create_dir(root.path(), "extra-skills");
    create_file(root.path(), "hooks/hooks.json", "{}");
    let ManifestLoadResult::Found(manifest) = load_manifest(root.path()).unwrap() else {
        panic!("manifest must be found");
    };
    assert_eq!(manifest.name, "demo-plugin");
    assert_eq!(manifest.skill_dirs(root.path()).len(), 2);
    assert!(manifest.hooks_path(root.path()).is_some());
    assert!(manifest.inline_mcp_servers().is_some());
}

#[test]
fn inline_hooks_and_mcp_are_preserved_but_have_no_file_path() {
    let root = fixture_plugin(
        r#"{"name":"inline","hooks":{"SessionStart":[]},"mcpServers":{"demo":{"command":"demo"}}}"#,
    );
    let ManifestLoadResult::Found(manifest) = load_manifest(root.path()).unwrap() else {
        panic!("manifest must be found");
    };
    assert!(manifest.inline_hooks().is_some());
    assert!(manifest.inline_mcp_servers().is_some());
    assert!(manifest.hooks_path(root.path()).is_none());
    assert!(manifest.mcp_config_path(root.path()).is_none());
}

#[test]
fn rejects_invalid_names_and_malformed_json() {
    for name in ["", "UPPER", "-leading", "trailing-", "has space"] {
        let root = fixture_plugin(&format!(r#"{{"name":"{name}"}}"#));
        assert!(matches!(
            load_manifest(root.path()),
            Err(ManifestError::InvalidName { .. })
        ));
    }
    let root = fixture_plugin("{");
    assert!(matches!(
        load_manifest(root.path()),
        Err(ManifestError::ParseError { .. })
    ));
}

#[test]
fn absolute_parent_missing_and_excess_component_paths_are_excluded() {
    let paths = (0..70)
        .map(|index| format!(r#""skill-{index}""#))
        .collect::<Vec<_>>()
        .join(",");
    let root = fixture_plugin(&format!(
        r#"{{"name":"bounded","skills":["/absolute","../parent","missing",{paths}]}}"#
    ));
    for index in 0..70 {
        create_dir(root.path(), &format!("skill-{index}"));
    }
    let ManifestLoadResult::Found(manifest) = load_manifest(root.path()).unwrap() else {
        panic!("manifest must be found");
    };
    assert_eq!(manifest.skill_dirs(root.path()).len(), 61);
}

#[test]
fn convention_requires_a_non_empty_recognized_component() {
    let root = tempfile::tempdir().unwrap();
    create_dir(root.path(), "skills");
    assert!(matches!(
        load_manifest(root.path()).unwrap(),
        ManifestLoadResult::Convention(_)
    ));

    let empty = tempfile::tempdir().unwrap();
    assert_eq!(
        load_manifest(empty.path()).unwrap(),
        ManifestLoadResult::NotFound
    );
    create_file(empty.path(), "hooks/hooks.json", "{}");
    assert!(matches!(
        load_manifest(empty.path()).unwrap(),
        ManifestLoadResult::Convention(_)
    ));
}

#[test]
fn dirname_names_are_sanitized_and_bounded() {
    assert_eq!(
        name_from_dirname(Path::new("Example_Plugin")),
        Some("example-plugin".into())
    );
    assert_eq!(name_from_dirname(Path::new("___")), None);
    assert_eq!(name_from_dirname(Path::new(&"a".repeat(65))), None);
}

#[cfg(unix)]
#[test]
fn component_symlink_escaping_root_is_excluded() {
    let root = fixture_plugin(r#"{"name":"demo","skills":"outside"}"#);
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();
    let ManifestLoadResult::Found(manifest) = load_manifest(root.path()).unwrap() else {
        panic!("manifest must be found");
    };
    assert!(manifest.skill_dirs(root.path()).is_empty());
}

fn fixture_plugin(manifest: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("plugin.json"), manifest).unwrap();
    root
}

fn create_dir(root: &Path, relative: &str) {
    fs::create_dir_all(root.join(relative)).unwrap();
}

fn create_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}
