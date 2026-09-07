use lato_agent::{
    BuiltinProfileName, ContextPackageBuilder, ContextPackageLimits, ContextReference,
};
use lato_core::{BudgetAmount, ToolCapability, WorkspaceIntent};

#[test]
fn built_in_profiles_are_closed_and_resolve_expected_authority() {
    let explorer = BuiltinProfileName::try_from("explorer").unwrap().resolve();
    assert_eq!(explorer.workspace, WorkspaceIntent::SharedReadOnly);
    assert_eq!(
        explorer.capabilities,
        vec![ToolCapability::FileRead, ToolCapability::NetworkRead]
    );
    assert!(BuiltinProfileName::try_from("custom").is_err());
}

#[test]
fn worker_cannot_recover_capability_absent_from_parent() {
    let worker = BuiltinProfileName::Worker.resolve();
    let effective = worker
        .effective_capabilities(&[ToolCapability::FileRead, ToolCapability::FileWrite], None)
        .unwrap();
    assert_eq!(
        effective,
        vec![ToolCapability::FileRead, ToolCapability::FileWrite]
    );
    assert!(!effective.contains(&ToolCapability::ProcessSpawn));
}

#[test]
fn context_package_is_bounded_and_contains_only_selected_context() {
    let limits = ContextPackageLimits {
        max_bytes: 1_024,
        max_constraints: 1,
        max_references: 1,
        max_summary_bytes: 32,
    };
    let package = ContextPackageBuilder::new(limits)
        .task("inspect parser")
        .profile_instructions("read only")
        .constraints(vec!["do not write".into(), "discarded".into()])
        .parent_summary("relevant state; unrelated parent turn canary is not supplied")
        .references(vec![
            ContextReference {
                id: "parser".into(),
                summary: "parser source".into(),
                location: Some("src/parser.rs".into()),
            },
            ContextReference {
                id: "discarded".into(),
                summary: "not selected after cap".into(),
                location: None,
            },
        ])
        .workspace_root("/tmp/workspace")
        .remaining_budget(BudgetAmount {
            total_tokens: 100,
            ..BudgetAmount::ZERO
        })
        .build()
        .unwrap();

    assert!(package.encoded_len() <= limits.max_bytes);
    assert_eq!(package.constraints, vec!["do not write"]);
    assert_eq!(package.references.len(), 1);
    assert!(package.parent_summary.unwrap().len() <= limits.max_summary_bytes);
}
