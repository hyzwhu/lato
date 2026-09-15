// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-shell/src/session/workflow/host_service.rs
// License: Apache-2.0
// Lato changes: Phase 7B6 builtin template catalog — a deliberately minimal
// closed set (`identity` only) with single-pass `{ident}` substitution; no
// user- or project-supplied template files.

use lato_workflow::HostError;

/// Builtin template bodies. Future Grok-named templates extend this table in
/// their own phase rather than being guessed here.
fn template_body(name: &str) -> Option<&'static str> {
    match name {
        "identity" => Some("{text}"),
        _ => None,
    }
}

/// Render the named builtin template with `vars` (a JSON object).
pub fn render_template(name: &str, vars: &serde_json::Value) -> Result<String, HostError> {
    let Some(body) = template_body(name) else {
        return Err(HostError::Failed(format!("unknown template: {name}")));
    };
    Ok(render_body(body, vars))
}

/// Non-overlapping scan for `{ident}` placeholders where `ident` matches
/// `[A-Za-z_][A-Za-z0-9_]*`. Replacements are never re-scanned.
fn render_body(body: &str, vars: &serde_json::Value) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(brace) = rest.find('{') {
        out.push_str(&rest[..brace]);
        let after = &rest[brace + 1..];
        match placeholder_len(after) {
            Some(ident_len) => {
                let ident = &after[..ident_len];
                out.push_str(&lookup(vars, ident));
                rest = &after[ident_len + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Length of an identifier directly followed by `}` at the start of `text`.
fn placeholder_len(text: &str) -> Option<usize> {
    let mut ident_len = 0usize;
    for (index, ch) in text.char_indices() {
        let ok = if ident_len == 0 {
            ch == '_' || ch.is_ascii_alphabetic()
        } else if ch == '}' {
            return Some(index);
        } else {
            ch == '_' || ch.is_ascii_alphanumeric()
        };
        if !ok {
            return None;
        }
        ident_len += ch.len_utf8();
    }
    None
}

fn lookup(vars: &serde_json::Value, ident: &str) -> String {
    match vars.get(ident) {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_template_returns_the_text_variable() {
        assert_eq!(
            render_template("identity", &json!({"text": "hi"})).unwrap(),
            "hi"
        );
    }

    #[test]
    fn unknown_templates_fail_with_stable_message() {
        assert!(matches!(
            render_template("nope", &json!({})),
            Err(HostError::Failed(message)) if message == "unknown template: nope"
        ));
        assert!(matches!(
            render_template("", &json!({})),
            Err(HostError::Failed(message)) if message == "unknown template: "
        ));
    }

    #[test]
    fn substitution_rules() {
        let vars = json!({
            "text": "hello",
            "count": 3,
            "flag": true,
            "nested": {"a": [1, 2]},
            "blank": null
        });
        let body =
            "{text}|{count}|{flag}|{nested}|{blank}|{missing}|{9bad}|{bad ident}|{}|{unclosed";
        assert_eq!(
            render_body(body, &vars),
            "hello|3|true|{\"a\":[1,2]}|null||{9bad}|{bad ident}|{}|{unclosed"
        );
    }

    #[test]
    fn replacements_are_not_rescanned() {
        let vars = json!({"text": "{count}", "count": "5"});
        assert_eq!(render_body("{text}", &vars), "{count}");
    }

    #[test]
    fn brace_without_ident_is_literal() {
        let vars = json!({"a": "A"});
        assert_eq!(render_body("x{ y} {a} }{", &vars), "x{ y} A }{");
    }
}
