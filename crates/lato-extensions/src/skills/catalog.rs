// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138:crates/codegen/xai-grok-tools/src/implementations/skills/skill.rs
// License: Apache-2.0
// Lato changes: immutable plugin-qualified catalog, bounded model listing, and snapshot-backed invocation

use std::{collections::BTreeMap, fmt::Write as _, path::Path, sync::Arc};

use lato_core::SkillInvocationOrigin;
use sha2::{Digest, Sha256};

use super::{
    DiscoveredSkill, MAX_EXPANDED_SKILL_BODY_BYTES, MAX_MODEL_SKILL_LISTING_BYTES,
    MAX_MODEL_SKILL_LISTING_ENTRIES, SkillDiagnostic, SkillDiscovery, SkillInvocation,
    SkillInvokeError,
};

const MAX_DIAGNOSTICS: usize = 128;
const MAX_DIAGNOSTIC_BYTES: usize = 512;
const LISTING_OPEN: &str = "<available_skills>\n";
const LISTING_CLOSE: &str = "</available_skills>";

#[derive(Clone, Debug)]
pub struct SkillCatalog {
    generation: u64,
    by_qualified: BTreeMap<String, Arc<DiscoveredSkill>>,
    by_bare: BTreeMap<String, Arc<[String]>>,
    diagnostics: Arc<[SkillDiagnostic]>,
    model_listing: Arc<str>,
    omitted_listing_count: usize,
}

impl SkillCatalog {
    pub fn from_discovery(discovery: SkillDiscovery) -> Arc<Self> {
        let mut diagnostics = discovery.diagnostics;
        diagnostics.truncate(MAX_DIAGNOSTICS);

        let mut skills = discovery.skills;
        skills.sort_by(|left, right| {
            qualified_name(left)
                .cmp(&qualified_name(right))
                .then_with(|| left.source_path.cmp(&right.source_path))
        });

        let mut by_qualified = BTreeMap::<String, Arc<DiscoveredSkill>>::new();
        for descriptor in skills {
            let qualified = qualified_name(&descriptor);
            if let Some(first) = by_qualified.get(&qualified) {
                push_diagnostic(
                    &mut diagnostics,
                    "skill.qualified_collision",
                    &descriptor.source_path,
                    format!(
                        "duplicate skill `{qualified}` ignored; first canonical path is `{}`",
                        first.source_path.display()
                    ),
                );
                continue;
            }
            by_qualified.insert(qualified, Arc::new(descriptor));
        }

        let mut bare = BTreeMap::<String, Vec<String>>::new();
        for (qualified, descriptor) in &by_qualified {
            bare.entry(descriptor.name.clone())
                .or_default()
                .push(qualified.clone());
        }
        let by_bare = bare
            .into_iter()
            .map(|(name, mut qualified)| {
                qualified.sort();
                (name, Arc::from(qualified))
            })
            .collect();

        let (model_listing, omitted_listing_count) = render_listing(&by_qualified);
        Arc::new(Self {
            generation: discovery.generation,
            by_qualified,
            by_bare,
            diagnostics: Arc::from(diagnostics),
            model_listing: Arc::from(model_listing),
            omitted_listing_count,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    pub fn omitted_listing_count(&self) -> usize {
        self.omitted_listing_count
    }

    pub fn render_model_listing(&self) -> String {
        self.model_listing.to_string()
    }

    pub fn invoke(
        &self,
        origin: SkillInvocationOrigin,
        name: &str,
        args: Option<&str>,
        session_id: &str,
    ) -> Result<SkillInvocation, SkillInvokeError> {
        let requested = name.trim();
        let (qualified, descriptor) = self.resolve(requested)?;
        match origin {
            SkillInvocationOrigin::User if !descriptor.user_invocable => {
                return Err(SkillInvokeError::UserInvocationDisabled {
                    qualified_name: qualified.to_owned(),
                });
            }
            SkillInvocationOrigin::Model if descriptor.disable_model_invocation => {
                return Err(SkillInvokeError::ModelInvocationDisabled {
                    qualified_name: qualified.to_owned(),
                });
            }
            SkillInvocationOrigin::Model | SkillInvocationOrigin::User => {}
        }

        let expanded_body = expand_body(descriptor, args, session_id)?;
        let body_hash = hex_sha256(expanded_body.as_bytes());
        let message = format!(
            "<skill name=\"{}\" description=\"{}\" path=\"{}\">\n{}\n</skill>",
            xml_escape(qualified),
            xml_escape(&descriptor.description),
            xml_escape(&descriptor.source_path.to_string_lossy()),
            expanded_body,
        );
        Ok(SkillInvocation {
            qualified_name: qualified.to_owned(),
            message,
            allowed_tools: descriptor
                .allowed_tools
                .as_ref()
                .map(|tools| Arc::from(tools.clone())),
            body_hash,
        })
    }

    fn resolve(&self, requested: &str) -> Result<(&str, &Arc<DiscoveredSkill>), SkillInvokeError> {
        if requested.contains(':') {
            return self
                .by_qualified
                .get_key_value(requested)
                .map(|(qualified, descriptor)| (qualified.as_str(), descriptor))
                .ok_or_else(|| SkillInvokeError::NotFound {
                    requested: requested.to_owned(),
                });
        }

        let candidates = self
            .by_bare
            .get(requested)
            .ok_or_else(|| SkillInvokeError::NotFound {
                requested: requested.to_owned(),
            })?;
        if candidates.len() != 1 {
            return Err(SkillInvokeError::Ambiguous {
                requested: requested.to_owned(),
                candidates: candidates.to_vec(),
            });
        }
        let qualified = &candidates[0];
        Ok((qualified, &self.by_qualified[qualified]))
    }
}

fn qualified_name(skill: &DiscoveredSkill) -> String {
    format!("{}:{}", skill.plugin_name, skill.name)
}

fn render_listing(skills: &BTreeMap<String, Arc<DiscoveredSkill>>) -> (String, usize) {
    let eligible = skills
        .iter()
        .filter(|(_, skill)| {
            !skill.disable_model_invocation
                && (skill.has_authored_description || skill.when_to_use.is_some())
        })
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return (String::new(), 0);
    }

    let mut listing = String::from(LISTING_OPEN);
    let mut included = 0;
    for (qualified, skill) in &eligible {
        if included == MAX_MODEL_SKILL_LISTING_ENTRIES {
            break;
        }
        let mut entry = format!("<skill name=\"{}\"", xml_escape(qualified));
        if skill.has_authored_description {
            let _ = write!(entry, " description=\"{}\"", xml_escape(&skill.description));
        }
        if let Some(when_to_use) = &skill.when_to_use {
            let _ = write!(entry, " when-to-use=\"{}\"", xml_escape(when_to_use));
        }
        let _ = writeln!(entry, " user-invocable=\"{}\"/>", skill.user_invocable);
        if listing.len() + entry.len() + LISTING_CLOSE.len() > MAX_MODEL_SKILL_LISTING_BYTES {
            break;
        }
        listing.push_str(&entry);
        included += 1;
    }
    listing.push_str(LISTING_CLOSE);
    (listing, eligible.len().saturating_sub(included))
}

fn expand_body(
    descriptor: &DiscoveredSkill,
    args: Option<&str>,
    session_id: &str,
) -> Result<String, SkillInvokeError> {
    let arguments = args.unwrap_or("");
    let argv = arguments.split_whitespace().collect::<Vec<_>>();
    let skill_dir = descriptor.skill_dir.to_string_lossy();
    let plugin_root = descriptor.plugin_root.to_string_lossy();
    let body = descriptor.body.as_str();
    // Match Grok's bounded candidate set: indexes through the current argv
    // plus a small missing-position window are substitutions. Larger digit
    // sequences (notably currency such as `$100`) remain ordinary text.
    let argument_candidate_end = argv.len().max(1).saturating_add(20);
    let mut expanded = String::with_capacity(body.len().min(MAX_EXPANDED_SKILL_BODY_BYTES));
    let mut index = 0;
    let mut consumed_argument_token = false;

    while index < body.len() {
        let rest = &body[index..];
        let replacement = [
            ("${CLAUDE_SKILL_DIR}", skill_dir.as_ref()),
            ("${CLAUDE_SESSION_ID}", session_id),
            ("${CLAUDE_PLUGIN_ROOT}", plugin_root.as_ref()),
            ("${GROK_PLUGIN_ROOT}", plugin_root.as_ref()),
            ("${LATO_PLUGIN_ROOT}", plugin_root.as_ref()),
            ("${SKILL_DIR}", skill_dir.as_ref()),
            ("${SESSION_ID}", session_id),
        ]
        .into_iter()
        .find(|(token, _)| rest.starts_with(token));
        if let Some((token, value)) = replacement {
            push_expanded(&mut expanded, value)?;
            index += token.len();
            continue;
        }

        if let Some(after) = rest.strip_prefix("$ARGUMENTS[") {
            if let Some(end) = after.find(']') {
                let token_len = "$ARGUMENTS[".len() + end + 1;
                let argument_index = (end > 0
                    && after[..end].bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| after[..end].parse::<usize>().ok())
                .flatten();
                if let Some(argument_index) =
                    argument_index.filter(|value| *value < argument_candidate_end)
                {
                    push_expanded(
                        &mut expanded,
                        argv.get(argument_index).copied().unwrap_or(""),
                    )?;
                    consumed_argument_token = true;
                } else {
                    push_expanded(&mut expanded, &rest[..token_len])?;
                }
                index += token_len;
                continue;
            }
            // An unterminated indexed-looking token is unknown, so preserve
            // its dollar sign and let the ordinary character path copy the
            // remainder without treating `$ARGUMENTS` as a full-args token.
            push_expanded(&mut expanded, "$")?;
            index += 1;
            continue;
        }
        if rest.starts_with("$ARGUMENTS") {
            push_expanded(&mut expanded, arguments)?;
            index += "$ARGUMENTS".len();
            consumed_argument_token = true;
            continue;
        }
        if let Some(after) = rest.strip_prefix('$') {
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 {
                let argument_index = after[..digits].parse::<usize>().ok();
                if let Some(argument_index) =
                    argument_index.filter(|value| *value < argument_candidate_end)
                {
                    push_expanded(
                        &mut expanded,
                        argv.get(argument_index).copied().unwrap_or(""),
                    )?;
                    consumed_argument_token = true;
                } else {
                    push_expanded(&mut expanded, &rest[..1 + digits])?;
                }
                index += 1 + digits;
                continue;
            }
        }

        let character = rest.chars().next().expect("index is within body");
        push_expanded(&mut expanded, character.encode_utf8(&mut [0; 4]))?;
        index += character.len_utf8();
    }

    if !consumed_argument_token && !arguments.is_empty() {
        push_expanded(&mut expanded, "\n\n**ARGUMENTS:** ")?;
        push_expanded(&mut expanded, arguments)?;
    }
    Ok(expanded)
}

fn push_expanded(out: &mut String, value: &str) -> Result<(), SkillInvokeError> {
    if out.len().saturating_add(value.len()) > MAX_EXPANDED_SKILL_BODY_BYTES {
        return Err(SkillInvokeError::ExpansionTooLarge {
            limit: MAX_EXPANDED_SKILL_BODY_BYTES,
        });
    }
    out.push_str(value);
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn push_diagnostic(
    out: &mut Vec<SkillDiagnostic>,
    code: &'static str,
    path: &Path,
    message: impl std::fmt::Display,
) {
    if out.len() == MAX_DIAGNOSTICS {
        return;
    }
    out.push(SkillDiagnostic::bounded(
        code,
        path,
        message.to_string(),
        MAX_DIAGNOSTIC_BYTES,
    ));
}
