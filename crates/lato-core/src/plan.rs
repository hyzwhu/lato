//! Plan mode pure contracts (Phase 8B, frozen spec v1.0 §4–§6).
//!
//! Pure types and decisions only: no I/O, no policy-engine or TUI knowledge.
//! The single canonical trim decision lives in [`plan_mode_denial`] and is
//! shared by the model-visible catalog filter and the policy overlay so the
//! two can never drift apart.

use crate::{SideEffect, ToolCapability, ToolLayer, ToolName};

/// Stable policy denial code for every Plan-mode capability-trim denial.
pub const PLAN_MODE_READONLY_CODE: &str = "plan.mode.readonly";

/// Stable code used when a mutation call fails the Plan approval TOCTOU guard.
pub const PLAN_APPROVAL_STALE_CODE: &str = "plan.approval_stale";

/// Hard UTF-8 byte cap for the plan draft written by `plan_draft`.
/// Values greater than this are rejected without truncation (spec §3.1).
pub const PLAN_DRAFT_MAX_BYTES: usize = 131_072;

/// Canonical plan file name, joined onto the immutable workspace root.
pub const PLAN_FILE_NAME: &str = "plan.md";

/// Canonical builtin identity of the sole permitted Plan-mode mutation tool.
pub const PLAN_DRAFT_TOOL_NAME: &str = "builtin:plan_draft";

/// Lifecycle phases of one Plan-mode activation (spec §5).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPhase {
    Inactive,
    Drafting,
    AwaitingApproval,
    Approved,
    Revising,
    Exited,
}

impl PlanPhase {
    /// True while the read-only capability trim applies.
    pub fn plan_mode_active(&self) -> bool {
        matches!(
            self,
            PlanPhase::Drafting
                | PlanPhase::AwaitingApproval
                | PlanPhase::Approved
                | PlanPhase::Revising
        )
    }

    /// True while the plan file is expected to be a readable draft.
    pub fn expects_draft(&self) -> bool {
        matches!(
            self,
            PlanPhase::Drafting
                | PlanPhase::AwaitingApproval
                | PlanPhase::Approved
                | PlanPhase::Revising
        )
    }
}

/// Human-driven (or integrity) transitions between plan phases. The model can
/// never issue any of these: command plumbing only calls them from trusted
/// human command paths, and [`PlanCommand::Stale`] is detected by the guard.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCommand {
    /// `/plan` — start a new activation.
    Enter,
    /// `/plan exit`.
    Exit,
    /// `/plan submit` — the only `Drafting|Revising → AwaitingApproval` edge.
    Submit,
    /// `/plan approve`.
    Approve,
    /// User requests edits instead of approving (normal chat reply).
    Revise,
    /// Integrity edge: the user-owned plan file changed after approval.
    Stale,
}

/// Applies the transition table of spec §5. Returns the next phase, or `None`
/// when the edge is illegal (the caller must refuse and leave state intact).
pub fn plan_transition(from: PlanPhase, command: PlanCommand) -> Option<PlanPhase> {
    match command {
        PlanCommand::Enter => match from {
            PlanPhase::Inactive | PlanPhase::Exited => Some(PlanPhase::Drafting),
            _ => None,
        },
        PlanCommand::Submit => match from {
            PlanPhase::Drafting | PlanPhase::Revising => Some(PlanPhase::AwaitingApproval),
            _ => None,
        },
        PlanCommand::Approve => match from {
            PlanPhase::AwaitingApproval => Some(PlanPhase::Approved),
            _ => None,
        },
        PlanCommand::Revise => match from {
            PlanPhase::Drafting
            | PlanPhase::AwaitingApproval
            | PlanPhase::Approved
            | PlanPhase::Revising => Some(PlanPhase::Revising),
            _ => None,
        },
        PlanCommand::Stale => match from {
            PlanPhase::Approved => Some(PlanPhase::Revising),
            _ => None,
        },
        PlanCommand::Exit => Some(PlanPhase::Exited),
    }
}

/// The single canonical Plan-mode trim decision (correction #3: one source for
/// the model catalog filter and the policy overlay).
///
/// Returns `None` when the tool is allowed as-is, or the stable denial code
/// when Plan mode must deny it. Fail-closed: anything not explicitly
/// allow-listed is denied, including read-only MCP/plugin tools and unknown
/// builtin names.
pub fn plan_mode_denial(
    tool: &ToolName,
    capabilities: &[ToolCapability],
    side_effect: SideEffect,
    layer: ToolLayer,
) -> Option<&'static str> {
    // Third-party and user-extension tools are denied in Phase 8B even when
    // they report read-only effects: their descriptors cannot be trusted yet
    // (spec §4). MCP tools resolve to their own namespaces; a plugin or user
    // override always carries a non-builtin layer.
    if layer != ToolLayer::Builtin {
        return Some(PLAN_MODE_READONLY_CODE);
    }
    // The bounded plan draft writer is the sole permitted mutation path and is
    // only reachable under its exact canonical builtin identity, so it cannot
    // be arrived at through a `write_file` alias.
    if tool.as_str() == PLAN_DRAFT_TOOL_NAME {
        return None;
    }
    let mutation_capability = capabilities.iter().any(|capability| {
        matches!(
            capability,
            ToolCapability::FileWrite
                | ToolCapability::ProcessSpawn
                | ToolCapability::NetworkWrite
                | ToolCapability::TaskControl
                | ToolCapability::ExtensionInvoke
        )
    });
    let mutation_effect = matches!(
        side_effect,
        SideEffect::WorkspaceMutation | SideEffect::ExternalMutation
    );
    if mutation_capability || mutation_effect {
        return Some(PLAN_MODE_READONLY_CODE);
    }
    // Explicit read-only allowlist; every other name (including unknown or
    // alias-resolved tools) fails closed.
    match tool.local_name() {
        "read_file" | "grep" | "list_dir" | "todo_write" | "web_search" | "web_fetch" => None,
        _ => Some(PLAN_MODE_READONLY_CODE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolName;

    fn denial_for(
        id: &str,
        capabilities: &[ToolCapability],
        side_effect: SideEffect,
        layer: ToolLayer,
    ) -> Option<&'static str> {
        plan_mode_denial(
            &ToolName::parse(id).unwrap(),
            capabilities,
            side_effect,
            layer,
        )
    }

    #[test]
    fn plan_draft_is_the_sole_mutation_exception() {
        assert_eq!(
            denial_for(
                "builtin:plan_draft",
                &[ToolCapability::FileWrite],
                SideEffect::WorkspaceMutation,
                ToolLayer::Builtin
            ),
            None
        );
        // A same-named tool from another layer never gets the exception.
        assert_eq!(
            denial_for(
                "mcp:plan_draft",
                &[ToolCapability::FileWrite],
                SideEffect::WorkspaceMutation,
                ToolLayer::User
            ),
            Some(PLAN_MODE_READONLY_CODE)
        );
    }

    #[test]
    fn read_only_builtin_allowlist_passes() {
        for id in [
            "builtin:read_file",
            "builtin:grep",
            "builtin:list_dir",
            "builtin:todo_write",
            "builtin:web_search",
            "builtin:web_fetch",
        ] {
            assert_eq!(
                denial_for(
                    id,
                    &[ToolCapability::FileRead],
                    SideEffect::ReadOnly,
                    ToolLayer::Builtin
                ),
                None,
                "{id} must be allowed"
            );
        }
    }

    #[test]
    fn mutation_and_spawn_tools_are_denied() {
        for (id, capabilities, side_effect) in [
            (
                "builtin:write_file",
                vec![ToolCapability::FileWrite],
                SideEffect::WorkspaceMutation,
            ),
            (
                "builtin:search_replace",
                vec![ToolCapability::FileWrite],
                SideEffect::WorkspaceMutation,
            ),
            (
                "builtin:run_terminal_command",
                vec![ToolCapability::ProcessSpawn],
                SideEffect::WorkspaceMutation,
            ),
            (
                "builtin:spawn",
                vec![ToolCapability::TaskControl],
                SideEffect::None,
            ),
            (
                "builtin:use_tool",
                vec![ToolCapability::ExtensionInvoke],
                SideEffect::ReadOnly,
            ),
        ] {
            assert_eq!(
                denial_for(id, &capabilities, side_effect, ToolLayer::Builtin),
                Some(PLAN_MODE_READONLY_CODE),
                "{id} must be denied"
            );
        }
    }

    #[test]
    fn read_only_mcp_and_extension_tools_fail_closed() {
        assert_eq!(
            denial_for(
                "mcp:query",
                &[],
                SideEffect::ReadOnly,
                ToolLayer::TrustedProject
            ),
            Some(PLAN_MODE_READONLY_CODE)
        );
        assert_eq!(
            denial_for(
                "builtin:some_unknown_tool",
                &[],
                SideEffect::None,
                ToolLayer::Builtin
            ),
            Some(PLAN_MODE_READONLY_CODE)
        );
    }

    #[test]
    fn transition_table_matches_frozen_spec() {
        assert_eq!(
            plan_transition(PlanPhase::Inactive, PlanCommand::Enter),
            Some(PlanPhase::Drafting)
        );
        assert_eq!(
            plan_transition(PlanPhase::Exited, PlanCommand::Enter),
            Some(PlanPhase::Drafting)
        );
        assert_eq!(
            plan_transition(PlanPhase::Approved, PlanCommand::Enter),
            None
        );
        assert_eq!(
            plan_transition(PlanPhase::Drafting, PlanCommand::Submit),
            Some(PlanPhase::AwaitingApproval)
        );
        assert_eq!(
            plan_transition(PlanPhase::Revising, PlanCommand::Submit),
            Some(PlanPhase::AwaitingApproval)
        );
        assert_eq!(
            plan_transition(PlanPhase::AwaitingApproval, PlanCommand::Submit),
            None
        );
        assert_eq!(
            plan_transition(PlanPhase::AwaitingApproval, PlanCommand::Approve),
            Some(PlanPhase::Approved)
        );
        assert_eq!(
            plan_transition(PlanPhase::Drafting, PlanCommand::Approve),
            None
        );
        assert_eq!(
            plan_transition(PlanPhase::AwaitingApproval, PlanCommand::Revise),
            Some(PlanPhase::Revising)
        );
        assert_eq!(
            plan_transition(PlanPhase::Approved, PlanCommand::Revise),
            Some(PlanPhase::Revising)
        );
        assert_eq!(
            plan_transition(PlanPhase::Approved, PlanCommand::Stale),
            Some(PlanPhase::Revising)
        );
        assert_eq!(
            plan_transition(PlanPhase::Drafting, PlanCommand::Stale),
            None
        );
        for from in [
            PlanPhase::Inactive,
            PlanPhase::Drafting,
            PlanPhase::AwaitingApproval,
            PlanPhase::Approved,
            PlanPhase::Revising,
            PlanPhase::Exited,
        ] {
            assert_eq!(
                plan_transition(from, PlanCommand::Exit),
                Some(PlanPhase::Exited)
            );
        }
        // The model can never reach AwaitingApproval or Approved on its own:
        // the only edges into them are Submit and Approve, both human commands.
    }

    #[test]
    fn phase_helpers_reflect_overlay_scope() {
        assert!(!PlanPhase::Inactive.plan_mode_active());
        assert!(!PlanPhase::Exited.plan_mode_active());
        for phase in [
            PlanPhase::Drafting,
            PlanPhase::AwaitingApproval,
            PlanPhase::Approved,
            PlanPhase::Revising,
        ] {
            assert!(phase.plan_mode_active());
            assert!(phase.expects_draft());
        }
    }
}
