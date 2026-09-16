# Lato Module & Wiring Ledger

A running record of notable modules, wiring points, and cross-crate contracts
added by each phase. Newest entries first. This complements the design specs
in `docs/superpowers/specs/`.

## Phase 7B7 — Model-visible workflow tool (2026-09-16)

- `crates/lato-agent/src/workflow/tool.rs` — session-bound `WorkflowTool`
  (`builtin:workflow`, wire name `workflow`) with `list` / `start` / `status`,
  the §4.1 input schema, conditional argument validation, stable
  `workflow.*` error codes, §5 status normalization (lossless
  `detailStatus`), and 64-entry / 64 KiB bounded output.
- `SessionWorkflowHandle` — construction-safe pairing between the main-session
  tool runtime and the `WorkflowManager` mounted by
  `AcpHost::attach_workflow_manager`; installed via
  `host.rs` right after mount, `None` → every action fails closed with
  `workflow.unavailable`. No global map, no session back-reference, no ACP
  self-call. The manager's catalog snapshot is kept fresh by
  `RuntimeSession::stage_plugin_snapshot` / `finish_turn` and
  `AcpHost::attach_session_plugins`.
- Approval contract: `lato_core::Tool::approval_detail` (optional tool-provided
  detail) is copied into `lato_core::PolicyRequest::detail` by
  `lato_tools::prepare_scoped`, rendered by the `lato-policy` approval summary,
  and bound into the approval fingerprint. For `start` it records a resolution
  baseline (id/source/script/budget/args) that is re-verified at invoke time —
  a catalog change between approval and execution returns
  `workflow.catalog_changed` and starts nothing.
- Registration: `lato_tools::builtin_tool_runtime_with_subagents_and_mcp_extra`
  appends caller-provided tools after builtins/task/MCP behind the same
  capability ceiling. Only `AcpHost::make_runtime_session` (main session) uses
  it; subagent and headless catalogs never receive the tool
  (`crates/lato-agent/tests/workflow_tool.rs` guards this).
- Reuses unchanged: registry keep-first discovery + trust rules
  (`workflow/registry.rs`), 4-active cap and journal persistence/rollback
  (`workflow/manager.rs`), `session/update` broadcast (`lato/workflow`), and
  the 7B5 cross-process restore semantics.

## Phase 7B5 — Cross-process workflow resume (2026-09-15)

- `workflow/persist.rs` — `run.json` / `script.rhai` / `journal.jsonl` under
  `$LATO_HOME/sessions/<sessionId>/workflows/<runId>/`; tracker status changes
  mirror to disk; launch failures roll back fully.
- `WorkflowManager::new(workflows_dir)` restores paused-family, blocked,
  failed, cancelled runs; active-at-exit restores as terminal `interrupted`.

## Phase 7B4 — In-session workflow runs (2026-09-14)

- `WorkflowManager` per session (`workflow/manager.rs`, `workflow/tracker.rs`):
  4-active cap, unique display names, pause/resume/stop, `session/update`
  broadcast (`sessionUpdate: "lato/workflow"`), same-process replay journals.
- Registry (`workflow/registry.rs`): keep-first user → trusted project →
  trusted+enabled plugin discovery; short-name shadowing; qualified
  `plugin/workflow` ids; `clamp_agent_budget` (default 128, max 1024).
