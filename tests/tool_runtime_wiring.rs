#[test]
fn actor_uses_the_tool_runtime_for_definitions_and_execution() {
    let actor = include_str!("../crates/lato-agent/src/actor.rs");
    assert!(
        actor.contains("pub fn new_with_tool_runtime("),
        "actor must expose runtime injection"
    );
    assert!(
        !actor.contains("dispatch("),
        "actor must not dispatch tools directly"
    );
    assert!(
        !actor.contains("v1_tool_definitions("),
        "actor must not own a second tool list"
    );
    assert!(actor.contains("tool_runtime.model_definitions()"));
    assert!(actor.contains("tool_runtime.prepare("));
    assert!(actor.contains("tool_runtime.execute("));
}

#[test]
fn legacy_dispatch_is_only_called_by_the_builtin_adapter_in_production() {
    let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
    let mut call_sites = Vec::new();
    collect_dispatch_call_sites(&source_root, &mut call_sites);
    assert_eq!(
        call_sites,
        vec!["crates/lato-tools/src/builtin_adapter.rs"],
        "legacy dispatch calls outside the compatibility adapter: {call_sites:?}"
    );
}

fn collect_dispatch_call_sites(path: &std::path::Path, call_sites: &mut Vec<String>) {
    if path.is_dir() {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            collect_dispatch_call_sites(&entry, call_sites);
        }
        return;
    }
    if path.extension().and_then(std::ffi::OsStr::to_str) != Some("rs")
        || path.ends_with("lato-tools/src/dispatch.rs")
        || !path.to_string_lossy().contains("/src/")
    {
        return;
    }

    let source = std::fs::read_to_string(path).unwrap();
    if contains_bare_call(&source, "dispatch") {
        let relative = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('/')
            .to_owned();
        call_sites.push(relative);
    }
}

fn contains_bare_call(source: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    source.match_indices(&needle).any(|(index, _)| {
        source[..index]
            .chars()
            .next_back()
            .is_none_or(|character| !(character.is_alphanumeric() || character == '_'))
    })
}
