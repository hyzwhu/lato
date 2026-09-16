#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <agentfield-checkout>" >&2
  exit 2
fi

upstream=$1
fixture=docs/superpowers/fixtures/agentfield-v0.1.138-contract.json
pinned=0aba9d6de1ef2c473070fc329ac7ac63e5d096b9

[[ -d "$upstream/.git" ]]
[[ "$(git -C "$upstream" rev-parse HEAD)" == "$pinned" ]]
jq -e --arg commit "$pinned" '.source.commit == $commit' "$fixture" >/dev/null

while IFS= read -r path; do
  [[ -f "$upstream/$path" ]] || {
    echo "missing pinned provenance path: $path" >&2
    exit 1
  }
done < <(jq -r '.source.files[]' "$fixture")

execute=$upstream/control-plane/internal/handlers/execute.go
cancel=$upstream/control-plane/internal/handlers/execute_cancel.go

for tag in execution_id run_id workflow_id status target type created_at enqueued_at webhook_registered webhook_error approval_request_id approval_status approval_request_url; do
  rg -F "json:\"$tag" "$execute" >/dev/null || {
    echo "missing pinned Go JSON tag: $tag" >&2
    exit 1
  }
done

if rg -F 'json:"approval_url' "$execute" >/dev/null; then
  echo "unexpected approval_url Go tag in pinned handler" >&2
  exit 1
fi

rg -U '"error":[[:space:]]+"invalid_state"' "$cancel" >/dev/null
jq -e '.cancel.terminal_conflict_contract.stable_consumed.error == "invalid_state"' "$fixture" >/dev/null
jq -e '.cancel.terminal_conflict_contract.dynamic_ignored_but_type_checked == ["message"]' "$fixture" >/dev/null

rg -U 'WorkflowID:[[:space:]]+plan\.exec\.RunID' "$execute" >/dev/null
rg -U 'EnqueuedAt:[[:space:]]+createdAt' "$execute" >/dev/null
jq -e '
  .async_start.success_envelope.workflow_id == .async_start.success_envelope.run_id and
  .async_start.success_envelope.enqueued_at == .async_start.success_envelope.created_at and
  .async_start.invariants.workflow_id_equals_run_id.validated == true and
  .async_start.invariants.enqueued_at_equals_created_at_for_normal_queue.validated == true
' "$fixture" >/dev/null

echo "AgentField pinned contract fixture is consistent with $pinned"
