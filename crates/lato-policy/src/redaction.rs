use regex::Regex;
use std::sync::OnceLock;

const REDACTED: &str = "[REDACTED]";

pub fn redact_text(text: &str, known_secrets: &[&str]) -> String {
    let mut output = text.to_string();
    for secret in known_secrets {
        if !secret.is_empty() {
            output = output.replace(secret, REDACTED);
        }
    }
    output = bearer_re()
        .replace_all(&output, format!("Bearer {REDACTED}"))
        .into_owned();
    assignment_re()
        .replace_all(&output, |captures: &regex::Captures| {
            format!("{}{REDACTED}", &captures[1])
        })
        .into_owned()
}

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bbearer\s+\S+").expect("bearer redaction regex"))
}

fn assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(\b(?:api[_-]?key|token|secret|password|authorization|access[_-]?key|private[_-]?key|refresh[_-]?token|client[_-]?secret)\s*[=:]\s*)(?:"[^"]*"|'[^']*'|\S+)"#,
        )
        .expect("assignment redaction regex")
    })
}
