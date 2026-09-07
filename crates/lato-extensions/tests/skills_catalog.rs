use std::{
    fs,
    path::{Path, PathBuf},
};

use lato_core::SkillInvocationOrigin;
use lato_extensions::skills::{
    DiscoveredSkill, MAX_EXPANDED_SKILL_BODY_BYTES, MAX_MODEL_SKILL_LISTING_ENTRIES,
    MAX_SKILL_FILE_BYTES, SkillCatalog, SkillDiscovery, SkillInvokeError,
};

fn skill(plugin: &str, name: &str, source_path: PathBuf, body: &str) -> DiscoveredSkill {
    let plugin_root = source_path
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("/plugins"))
        .to_path_buf();
    let skill_dir = source_path.parent().unwrap().to_path_buf();
    DiscoveredSkill {
        plugin_name: plugin.to_owned(),
        plugin_root,
        skill_dir,
        source_path,
        name: name.to_owned(),
        description: format!("Use {name} safely."),
        has_authored_description: true,
        when_to_use: None,
        argument_hint: None,
        allowed_tools: None,
        user_invocable: true,
        disable_model_invocation: false,
        body: body.to_owned(),
        paths: None,
        license: None,
        compatibility: None,
        metadata: None,
        model: None,
        effort: None,
    }
}

fn catalog(skills: Vec<DiscoveredSkill>) -> std::sync::Arc<SkillCatalog> {
    SkillCatalog::from_discovery(SkillDiscovery {
        generation: 17,
        skills,
        diagnostics: Vec::new(),
    })
}

#[test]
fn qualified_names_do_not_collide() {
    let catalog = catalog(vec![
        skill(
            "alpha",
            "inspect",
            "/plugins/alpha/inspect/SKILL.md".into(),
            "alpha",
        ),
        skill(
            "beta",
            "inspect",
            "/plugins/beta/inspect/SKILL.md".into(),
            "beta",
        ),
    ]);

    assert!(
        catalog
            .invoke(SkillInvocationOrigin::User, "alpha:inspect", None, "s")
            .unwrap()
            .message
            .contains("alpha")
    );
    assert!(
        catalog
            .invoke(SkillInvocationOrigin::User, "beta:inspect", None, "s")
            .unwrap()
            .message
            .contains("beta")
    );
}

#[test]
fn unique_bare_name_resolves() {
    let catalog = catalog(vec![skill(
        "demo",
        "inspect",
        "/plugins/demo/inspect/SKILL.md".into(),
        "body",
    )]);

    let invoked = catalog
        .invoke(SkillInvocationOrigin::User, "inspect", None, "session")
        .unwrap();
    assert_eq!(invoked.qualified_name, "demo:inspect");
}

#[test]
fn ambiguous_bare_name_lists_sorted_candidates() {
    let catalog = catalog(vec![
        skill(
            "zeta",
            "inspect",
            "/plugins/zeta/inspect/SKILL.md".into(),
            "z",
        ),
        skill(
            "alpha",
            "inspect",
            "/plugins/alpha/inspect/SKILL.md".into(),
            "a",
        ),
    ]);

    assert_eq!(
        catalog.invoke(SkillInvocationOrigin::User, "inspect", None, "session"),
        Err(SkillInvokeError::Ambiguous {
            requested: "inspect".into(),
            candidates: vec!["alpha:inspect".into(), "zeta:inspect".into()],
        })
    );
}

#[test]
fn listing_requires_authored_description_or_when_to_use() {
    let mut hidden = skill(
        "demo",
        "hidden",
        "/p/demo/hidden/SKILL.md".into(),
        "secret body prose",
    );
    hidden.has_authored_description = false;
    hidden.description = "secret body prose".into();
    let mut trigger = skill("demo", "trigger", "/p/demo/trigger/SKILL.md".into(), "body");
    trigger.has_authored_description = false;
    trigger.description = "derived body prose".into();
    trigger.when_to_use = Some("When reviewing changes.".into());

    let listing = catalog(vec![hidden, trigger]).render_model_listing();

    assert!(!listing.contains("demo:hidden"));
    assert!(listing.contains("demo:trigger"));
    assert!(listing.contains("When reviewing changes."));
    assert!(!listing.contains("derived body prose"));
    assert!(!listing.contains("/p/demo"));
}

#[test]
fn listing_excludes_model_disabled_skills() {
    let mut disabled = skill("demo", "manual", "/p/demo/manual/SKILL.md".into(), "body");
    disabled.disable_model_invocation = true;
    assert!(
        !catalog(vec![disabled])
            .render_model_listing()
            .contains("demo:manual")
    );
}

#[test]
fn user_and_model_invocation_flags_are_independent() {
    let mut model_only = skill(
        "demo",
        "model-only",
        "/p/demo/model/SKILL.md".into(),
        "body",
    );
    model_only.user_invocable = false;
    let mut user_only = skill("demo", "user-only", "/p/demo/user/SKILL.md".into(), "body");
    user_only.disable_model_invocation = true;
    let catalog = catalog(vec![model_only, user_only]);

    assert!(matches!(
        catalog.invoke(SkillInvocationOrigin::User, "demo:model-only", None, "s"),
        Err(SkillInvokeError::UserInvocationDisabled { .. })
    ));
    assert!(
        catalog
            .invoke(SkillInvocationOrigin::Model, "demo:model-only", None, "s")
            .is_ok()
    );
    assert!(
        catalog
            .invoke(SkillInvocationOrigin::User, "demo:user-only", None, "s")
            .is_ok()
    );
    assert!(matches!(
        catalog.invoke(SkillInvocationOrigin::Model, "demo:user-only", None, "s"),
        Err(SkillInvokeError::ModelInvocationDisabled { .. })
    ));
}

#[test]
fn listing_stops_on_complete_entry_boundary() {
    let skills = (0..MAX_MODEL_SKILL_LISTING_ENTRIES + 2)
        .map(|index| {
            skill(
                "demo",
                &format!("skill-{index:03}"),
                format!("/p/demo/skill-{index:03}/SKILL.md").into(),
                "body",
            )
        })
        .collect();
    let catalog = catalog(skills);
    let listing = catalog.render_model_listing();

    assert!(listing.len() <= 64 * 1024);
    assert_eq!(
        listing.matches("<skill ").count(),
        MAX_MODEL_SKILL_LISTING_ENTRIES
    );
    assert_eq!(catalog.omitted_listing_count(), 2);
    assert!(listing.ends_with("</available_skills>"));
}

#[test]
fn listing_stops_at_64_kib_without_partial_entry() {
    let skills = (0..MAX_MODEL_SKILL_LISTING_ENTRIES)
        .map(|index| {
            let mut descriptor = skill(
                "demo",
                &format!("skill-{index:03}"),
                format!("/p/demo/skill-{index:03}/SKILL.md").into(),
                "body",
            );
            descriptor.description = "x".repeat(1024);
            descriptor
        })
        .collect();
    let catalog = catalog(skills);
    let listing = catalog.render_model_listing();

    assert!(listing.len() <= 64 * 1024);
    assert!(catalog.omitted_listing_count() > 0);
    assert_eq!(
        listing.matches("<skill ").count(),
        listing.matches("/>\n").count()
    );
    assert!(listing.ends_with("</available_skills>"));
}

#[test]
fn last_catalog_is_immutable_after_source_edit() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("demo/inspect/SKILL.md");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "old body").unwrap();
    let catalog = catalog(vec![skill("demo", "inspect", source.clone(), "old body")]);

    fs::write(source, "new body").unwrap();

    let invoked = catalog
        .invoke(SkillInvocationOrigin::User, "inspect", None, "s")
        .unwrap();
    assert!(invoked.message.contains("old body"));
    assert!(!invoked.message.contains("new body"));
}

#[test]
fn applies_all_grok_substitutions_and_xml_escapes_attributes() {
    let path = PathBuf::from("/plugins/demo&co/inspect/SKILL.md");
    let mut descriptor = skill(
        "demo&co",
        "inspect",
        path,
        concat!(
            "$ARGUMENTS / $ARGUMENTS[0] / $ARGUMENTS[1] / $0 / $1 / $29\n",
            "${SKILL_DIR} ${CLAUDE_SKILL_DIR}\n",
            "${SESSION_ID} ${CLAUDE_SESSION_ID}\n",
            "${LATO_PLUGIN_ROOT} ${GROK_PLUGIN_ROOT} ${CLAUDE_PLUGIN_ROOT}\n",
            "$UNKNOWN $$",
        ),
    );
    descriptor.description = "Review <code> & \"tests\".".into();
    let catalog = catalog(vec![descriptor]);

    let invoked = catalog
        .invoke(
            SkillInvocationOrigin::User,
            "demo&co:inspect",
            Some("alpha beta"),
            "session&1",
        )
        .unwrap();

    assert!(
        invoked
            .message
            .contains("alpha beta / alpha / beta / alpha / beta /")
    );
    assert!(
        invoked
            .message
            .contains("/plugins/demo&co/inspect /plugins/demo&co/inspect")
    );
    assert!(invoked.message.contains("session&1 session&1"));
    assert!(
        invoked
            .message
            .contains("/plugins/demo&co /plugins/demo&co /plugins/demo&co")
    );
    assert!(invoked.message.contains("$UNKNOWN $$"));
    assert!(invoked.message.starts_with("<skill name=\"demo&amp;co:inspect\" description=\"Review &lt;code&gt; &amp; &quot;tests&quot;.\" path=\"/plugins/demo&amp;co/inspect/SKILL.md\">"));
    assert!(invoked.message.ends_with("\n</skill>"));
    assert_eq!(invoked.body_hash.len(), 64);
}

#[test]
fn appends_arguments_only_when_no_argument_token_is_consumed() {
    let catalog = catalog(vec![skill(
        "demo",
        "inspect",
        "/plugins/demo/inspect/SKILL.md".into(),
        "At ${SKILL_DIR}: $UNKNOWN",
    )]);
    let invoked = catalog
        .invoke(
            SkillInvocationOrigin::User,
            "inspect",
            Some("alpha beta"),
            "s",
        )
        .unwrap();
    assert!(invoked.message.contains("**ARGUMENTS:** alpha beta"));
}

#[test]
fn unknown_dollar_tokens_remain_unchanged() {
    let catalog = catalog(vec![skill(
        "demo",
        "inspect",
        "/plugins/demo/inspect/SKILL.md".into(),
        "Price: $100, var: ${UNKNOWN}, indexed: $ARGUMENTS[100]",
    )]);

    let invoked = catalog
        .invoke(SkillInvocationOrigin::User, "inspect", None, "s")
        .unwrap();

    assert!(
        invoked
            .message
            .contains("Price: $100, var: ${UNKNOWN}, indexed: $ARGUMENTS[100]")
    );
}

#[test]
fn dollar_amount_does_not_suppress_argument_suffix() {
    let catalog = catalog(vec![skill(
        "demo",
        "inspect",
        "/plugins/demo/inspect/SKILL.md".into(),
        "Price: $100 per unit.",
    )]);

    let invoked = catalog
        .invoke(
            SkillInvocationOrigin::User,
            "inspect",
            Some("deploy staging"),
            "s",
        )
        .unwrap();

    assert!(
        invoked
            .message
            .contains("Price: $100 per unit.\n\n**ARGUMENTS:** deploy staging")
    );
}

#[test]
fn real_argument_substitution_suppresses_suffix() {
    let catalog = catalog(vec![skill(
        "demo",
        "inspect",
        "/plugins/demo/inspect/SKILL.md".into(),
        "Run: $ARGUMENTS (cost: $100)",
    )]);

    let invoked = catalog
        .invoke(SkillInvocationOrigin::User, "inspect", Some("deploy"), "s")
        .unwrap();

    assert!(invoked.message.contains("Run: deploy (cost: $100)"));
    assert!(!invoked.message.contains("**ARGUMENTS:**"));
}

#[test]
fn shorthand_candidates_follow_grok_multi_digit_and_digit_boundary_rules() {
    let catalog = catalog(vec![skill(
        "demo",
        "inspect",
        "/plugins/demo/inspect/SKILL.md".into(),
        "$0|$1|$12|$13|$100|$1tail|$12tail",
    )]);
    let args = "zero one two three four five six seven eight nine ten eleven twelve";

    let invoked = catalog
        .invoke(SkillInvocationOrigin::User, "inspect", Some(args), "s")
        .unwrap();

    assert!(
        invoked
            .message
            .contains("zero|one|twelve||$100|onetail|twelvetail")
    );
    assert!(!invoked.message.contains("**ARGUMENTS:**"));
}

#[test]
fn repeated_unterminated_index_tokens_near_file_limit_remain_bounded() {
    let token = "$ARGUMENTS[";
    let body = token.repeat((MAX_SKILL_FILE_BYTES - 1) / token.len());
    let catalog = catalog(vec![skill(
        "demo",
        "malformed",
        "/plugins/demo/malformed/SKILL.md".into(),
        &body,
    )]);

    assert!(matches!(
        catalog.invoke(
            SkillInvocationOrigin::User,
            "malformed",
            Some("argument"),
            "s"
        ),
        Err(SkillInvokeError::ExpansionTooLarge { limit }) if limit == MAX_EXPANDED_SKILL_BODY_BYTES
    ));
}

#[test]
fn body_hash_is_sha256_of_expanded_body_and_changes_with_arguments() {
    let catalog = catalog(vec![skill(
        "demo",
        "hash",
        "/plugins/demo/hash/SKILL.md".into(),
        "$ARGUMENTS",
    )]);

    let alpha = catalog
        .invoke(SkillInvocationOrigin::User, "hash", Some("alpha"), "s")
        .unwrap();
    let beta = catalog
        .invoke(SkillInvocationOrigin::User, "hash", Some("beta"), "s")
        .unwrap();

    assert_eq!(
        alpha.body_hash,
        "8ed3f6ad685b959ead7022518e1af76cd816f8e8ec7ccdda1ed4018e8f2223f8"
    );
    assert_eq!(
        beta.body_hash,
        "f44e64e75f3948e9f73f8dfa94721c4ce8cbb4f265c4790c702b2d41cfbf2753"
    );
    assert_ne!(alpha.body_hash, beta.body_hash);
}

#[test]
fn rejects_expansion_over_128_kib() {
    let body = "$ARGUMENTS".repeat(MAX_EXPANDED_SKILL_BODY_BYTES / "$ARGUMENTS".len());
    let catalog = catalog(vec![skill(
        "demo",
        "huge",
        "/p/demo/huge/SKILL.md".into(),
        &body,
    )]);

    assert!(matches!(
        catalog.invoke(
            SkillInvocationOrigin::User,
            "huge",
            Some("01234567890"),
            "s"
        ),
        Err(SkillInvokeError::ExpansionTooLarge { limit }) if limit == MAX_EXPANDED_SKILL_BODY_BYTES
    ));
}

#[test]
fn duplicate_qualified_name_preserves_first_canonical_path_and_diagnoses() {
    let first = skill("demo", "inspect", "/a/inspect/SKILL.md".into(), "first");
    let second = skill("demo", "inspect", "/z/inspect/SKILL.md".into(), "second");
    let catalog = catalog(vec![second, first]);

    let invoked = catalog
        .invoke(SkillInvocationOrigin::User, "demo:inspect", None, "s")
        .unwrap();
    assert!(invoked.message.contains("first"));
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "skill.qualified_collision")
    );
}

#[test]
fn catalog_diagnostics_are_bounded() {
    let skills = (0..140)
        .map(|index| {
            skill(
                "demo",
                "inspect",
                format!("/{index:03}/inspect/SKILL.md").into(),
                "body",
            )
        })
        .collect();

    assert_eq!(catalog(skills).diagnostics().len(), 128);
}
