# Lato

Lato is a Public Beta coding agent for terminal-based development workflows. It provides an interactive coding CLI, a headless prompt mode, and an ACP host backed by the same runtime, tool policy, and durable session journal.

> **Beta software:** interfaces and stored formats are versioned and tested, but may still change before a stable release. Back up important work and review tool approvals carefully.

Prebuilt binaries are available for macOS Intel, macOS Apple Silicon, Linux x86-64, Linux ARM64, and Windows x86-64.

## Plan file backup (`.reap`) files

When Lato publishes an approved plan (`plan_draft`), the previous `plan.md` is
preserved as `plan.md.reap-<nonce>` in the same directory — the nonce keeps
concurrent backups collision-free, and the `reap-` suffix makes them easy to
audit with a plain directory scan.

- **When they are created**: only when a previous `plan.md` already exists —
  after a successful publication that replaces it (the old content is moved
  aside, never deleted), or after a failed publication whose target slot ended
  up occupied by another file. The very first publication in a directory
  creates no `.reap` file.
- **Ownership**: created by Lato itself; anything else found under that name
  was placed there outside Lato and is never touched by it.
- **Recovery**: move the file back onto `plan.md` manually if you want to
  roll back to the previous plan.
- **Cleanup**: Lato does NOT reap these backups automatically in this
  release — they accumulate. Deleting a Lato-created backup (a
  `plan.md.reap-<nonce>` file you recognize) is safe; files that you did not
  create under a similar name are outside this mechanism and are never
  touched by Lato. Automatic reaping (count/lifetime caps) is planned for
  spec v1.1 and is tracked as an open item.

## Install a prebuilt binary

On macOS and Linux, the fastest path is the install script. It detects your platform, downloads the archive from the latest release, verifies its SHA256 checksum, and installs to `~/.local/bin` (override with `LATO_INSTALL_DIR`):

```bash
curl -fsSL https://raw.githubusercontent.com/hyzwhu/lato/master/scripts/install.sh | sh
```

To pin a version: `sh install.sh v0.1.0-beta.2`. Then run `lato doctor` to verify the setup.

Windows users (or anyone preferring a manual path): download the archive for your platform and `SHA256SUMS` from the [v0.1.0-beta.2 release](../../releases/tag/v0.1.0-beta.2). Verify the archive before extracting it.

macOS example for Apple Silicon:

```bash
shasum -a 256 -c SHA256SUMS --ignore-missing
tar -xzf lato-0.1.0-beta.2-aarch64-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 lato-0.1.0-beta.2-aarch64-apple-darwin/lato "$HOME/.local/bin/lato"
```

Linux x86-64 example:

```bash
sha256sum --check SHA256SUMS --ignore-missing
tar -xzf lato-0.1.0-beta.2-x86_64-unknown-linux-gnu.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 0755 lato-0.1.0-beta.2-x86_64-unknown-linux-gnu/lato "$HOME/.local/bin/lato"
```

Ensure `$HOME/.local/bin` is on `PATH` before running Lato.

Windows PowerShell example:

```powershell
$archive = "lato-0.1.0-beta.2-x86_64-pc-windows-msvc.zip"
Get-FileHash $archive -Algorithm SHA256
# Compare the printed hash with this archive's entry in SHA256SUMS.
Expand-Archive $archive -DestinationPath .
New-Item -ItemType Directory -Force "$HOME\bin" | Out-Null
Copy-Item ".\lato-0.1.0-beta.2-x86_64-pc-windows-msvc\lato.exe" "$HOME\bin\lato.exe"
```

Add `%USERPROFILE%\bin` to the user `PATH`, then verify the installation:

```text
lato --version
```

## Quick Start

Run `lato` without arguments:

```bash
lato
```

On first launch Lato opens its full-screen interface and guides you through provider/model selection, OAuth or a masked API-key input, folder trust, and sandbox scope. The selected model is saved in `~/.lato/config.json`; credentials are kept separately in the locked credential store. The Ratatui interface includes session navigation, streaming reasoning and responses, live tool-call cards, and approval dialogs.

The interface follows `LC_ALL`, `LC_MESSAGES`, or `LANG` on first launch and supports Chinese and English. Override and persist the language with either form below; `/lang` switches it from inside the TUI.

```bash
lato --lang zh-CN
lato --lang en
```

Interactive commands:

```text
/help  /new  /clear  /compact  /sessions  /rename  /delete  /model  /login  /doctor  /search  /lang  /approve  /status  /permissions  /skills  /skill  /files  /workflows  /exit
```

Tool calls start collapsed, showing their name, status, and duration. Focus the Tool calls panel with Tab, select calls with Up/Down or j/k, and use Enter/Space to toggle details (Left collapses, Right expands). PgUp/PgDn scroll through full arguments and results; Home/End select the first/last call. A scrollbar shows the current position. Incoming calls preserve your place while you are inspecting the panel.

Use Tab/Shift-Tab to move between panels, Up/Down to scroll the focused panel, Enter on a selected session to resume it, Cmd/Ctrl-K to open the command palette, Ctrl-F or `/search` to search, and Ctrl-C to cancel a streaming turn or exit while idle. In the session panel, press `r` to rename the selected session and press `d` twice within three seconds to delete it permanently. `/rename [title]` renames the current session, while `/delete` opens an explicit permanent-delete confirmation. `/model` and `/login` open dialogs inside the TUI. Type to filter providers and models, use arrow keys and Enter to choose, and press Esc to cancel. API keys are masked; OAuth URLs and device codes appear in the dialog. Successful model changes preserve the current session and conversation. `/sessions` opens a filterable session picker that matches titles and IDs; `/doctor` shows the offline diagnostic report in the conversation. Type `/` for grouped command completion (names and descriptions); Ctrl+K opens the same registry in the palette. `@` searches workspace files with gitignore and common build directories excluded, using `rg` or `git`. Tab or Enter inserts a file or command without sending; Esc closes candidates without cancelling a running turn. `/skills` lists user-invokable skills from the current plugin snapshot; `/skill <qualified-name> [args]` runs one. File reads are bounded (64 KiB each, 256 KiB total) and fail closed instead of truncating. The composer grows with the draft up to eight rows; Enter sends, Alt+Enter inserts a newline, and Up/Down move through wrapped lines before history. Streamed reasoning appears as a live card in event order and folds when the answer or a tool starts; F2 expands it. The bottom row keeps context usage, model, and status visible during generation. Finish or cancel a response before changing models or sessions. The workspace starts untrusted. Mutating tool calls display their name and arguments and request approval exactly at the execution boundary. You can instead trust the workspace for the process or use `/approve` to pre-authorize one call. On smaller terminals, side panels collapse automatically so the conversation remains usable.

Sandbox scope and tool approval are separate. The startup sandbox picker defaults to
`workspace`; select `off` explicitly to allow writes outside the current workspace,
including creating sibling projects such as `../abc`. `read-only` denies file writes.
Folder trust enables automatic approval within the selected scope; approving a tool
call does not expand that scope. These choices apply only to the current invocation.
You can also select the scope directly, including when resuming:

```bash
lato --sandbox off
lato --sandbox workspace
lato --sandbox read-only
lato resume s1788336000000-1 --sandbox off
lato resume "Exact session title"
```

`lato resume` accepts an exact session ID or an exact title. An ID match always wins. A unique title resumes immediately. Duplicate titles open a chooser ordered by most recently updated; Escape cancels without starting a session.

`/permissions` and `/status` display the active scope and approval mode. To change
scope, exit and resume with the desired `--sandbox` value. `off` removes Lato's OS
sandbox, while operating-system permissions and existing tool policy checks still
apply. A failed sandboxed command never automatically retries with broader access.

For a one-shot, script-friendly prompt:

```bash
lato -p --model openai/gpt-4.1 --sandbox workspace "inspect this repository and report failing tests"
```

## Sessions and resume

Lato persists accepted turns and complete conversation state under `~/.lato/sessions`. List saved sessions in human or stable JSON form, then resume one interactively:

```bash
lato sessions
lato sessions --json
lato sessions rename s1788336000000-1 "Investigate parser regression"
lato sessions delete s1788336000000-1
# Scripts and other non-interactive callers must confirm explicitly:
lato sessions delete s1788336000000-1 --yes
lato resume s1788336000000-1
```

The first accepted prompt supplies a deterministic local title; manual renames always take priority over automatic titles. Human-readable listing prints the title and durable ID. JSON listing uses schema version 2 and returns `sessionId`, `title`, `titleSource`, `createdAtMs`, and `updatedAtMs` for each session. Delete is permanent: interactive callers must confirm it, while non-interactive callers must pass `--yes`.

Resume uses the currently configured default model and current working directory. It repeats the folder-trust prompt and, unless `--sandbox` is supplied, the sandbox picker. Permissions come from this invocation, not the saved journal. It fails closed instead of creating a replacement when a journal is missing, corrupt, or contains an unresolved side-effect outcome.

In the interactive TUI, `/compact` summarizes a long current conversation and
`/compact <context>` asks the summary to emphasize the supplied context. It uses the
current session model, may make one to three model calls, and never exposes or runs
tools. Ctrl-C cancels active sampling. A successful operation replaces only the
model-visible history through a durable checkpoint; the append-only canonical journal
is retained. Failures before the replacement marker keep the previous history active.
Automatic compaction now starts at 85% context utilization, with a speculative,
non-installing first summary pass beginning at 75%. Tool output that crosses the hard
context boundary is compacted before another provider request. A provider-reported
context overflow may compact and rebuild the interrupted request exactly once, but only
if no output was observed. Oversized compaction input degrades monotonically through
Prepared, Fitted, and Lossy stages under the same three-attempt ceiling; canonical
conversation history is never truncated in place. Deterministic failures suppress
repeated automatic work according to their turn, context, credit, or authentication
lifetime. Manual `/compact` remains available while automatic compaction is suppressed,
and `/status` reports the active suppression mode.

## Plan mode

Plan mode is a session-scoped read-only mode: the model may investigate the
repository and draft an implementation plan, but cannot mutate anything. The
plan lives in one well-known file, `<workspace>/plan.md`, which stays
user-owned — you can edit or delete it at any time, and Lato never touches
`.gitignore` for it.

```bash
# Headless: run a single planning turn. Exits with code 3 only when THIS
# activation actually published a plan through a successful `plan_draft` tool
# event (and it was not approved); a stale plan.md left over from an earlier
# run never satisfies the deliverable — the turn then fails explicitly
# instead. Headless never auto-approves.
lato -p --plan "draft a plan for adding retry logic"
# Re-enter Plan mode on a resumed session; the previous plan.md is loaded as
# the starting draft if it is present and readable.
lato resume s1788336000000-1 --plan
```

In the interactive TUI:

- `/plan` — enter Plan mode (refused while a turn is in flight). Shows the
  state, the plan file path, and the trimmed tool list.
- The model drafts through the dedicated `plan_draft` tool only. While Plan
  mode is active, `write_file`, `search_replace`, `run_terminal_command`,
  subagent/task/workflow spawning, and every MCP/plugin tool are denied at the
  policy layer — including forged direct calls — and read-only MCP tools stay
  denied as well.
- `/plan submit` — you declare the draft ready; this is the ONLY way the
  session moves to the awaiting-approval state. Model text, markers, plan
  content, and tool calls never advance the state machine.
- `/plan approve` — opens the dedicated plan review: a real scrollable,
  paginated widget over the actual terminal content area (the plan body is
  wrapped to the terminal width and paged by the terminal height). The
  approval control stays locked until the viewport actually shows the final
  row of the plan, and a terminal resize re-locks it until the new bottom is
  reached. Esc closes the review without approving. Approval records a
  session-local plan authorization; it is NOT a policy grant and never
  authorizes a tool call by itself.
- `/plan status` — phase, plan file path, last draft hash, and approval
  record. `/plan exit` — leave Plan mode; the plan file stays on disk.

After approval the session continues in its normal trust mode, and every
ordinary tool call still traverses the normal policy path: it may be allowed,
denied, or require its own approval exactly as before. Every mutation-capable
call re-verifies the plan file hash twice (before policy preparation and
immediately before the grant is consumed). If `plan.md` changed since the
approval, the call is denied with `plan.approval_stale`, the approval is
revoked, and the session returns to the read-only revising state — restoring
identical bytes never revives the old approval; a fresh `/plan submit` plus
`/plan approve` is required.

ACP clients can negotiate the `lato/plan/status` and `lato/plan/approve`
extension methods (plus `lato/plan/enter`, `lato/plan/submit`, and
`lato/plan/exit`); approvals over ACP mirror the TUI confirmation dialog and
run the same state-machine checks. Clients that do not support the extension
keep working unchanged.

## Plugins

Lato discovers plugin roots from repeatable `--plugin-dir` arguments,
`.lato/plugins/*` in the project, and `$LATO_HOME/plugins/*`, in that priority
order. CLI and user plugins are trusted; project plugins activate only for a
trusted folder. A plugin must also be enabled before it is active.

The canonical manifest is `plugin.json`. Component paths are confined to the
plugin root. `lato/plugins/reload` atomically rebuilds the registry and updates
live sessions at the next safe turn boundary.

An enabled, trusted plugin may provide skills under the directory named by its
manifest's `skills` field. Each skill is a directory containing `SKILL.md`.
Only skills with an authored `description` or `when-to-use` are listed to the
model; invoke one by its stable qualified identity, such as
`example-plugin:code-audit`, through the built-in `skill` tool. Bare names work
only when unique. `disable-model-invocation: true` hides and blocks model calls,
while `user-invocable: false` blocks explicit user-origin calls.

Skill bodies are loaded from the frozen plugin snapshot only after invocation.
They may use `$ARGUMENTS`, `$ARGUMENTS[N]`, `$N`, `${SKILL_DIR}`,
`${SESSION_ID}`, and `${LATO_PLUGIN_ROOT}` tokens. An `allowed-tools` list can
only narrow the tools available to the next model step; policy, approval,
sandbox, and child-profile checks still apply, and the enclosing tool catalog
is restored afterward. Omitting `allowed-tools` (or writing it as an empty
list) is treated as "no narrowing": the next step sees the full registered
tool set rather than a tightened surface, so authors who want to restrict
tools must enumerate at least one entry. A reload received during a turn is
staged for the next turn, so descriptions, bodies, and tool scopes never mix
generations.

Use `lato doctor` (or `lato doctor --json`) to inspect project/plugin trust and
configuration. ACP clients can call `lato/plugins/reload`; its response includes
the new generation, active/discovered counts, and bounded diagnostics. The
published snapshot is live in the shared registry by the time the call
returns: sessions that successfully adopted the new generation see it on
their next turn, and newly created sessions bind to it. If one or more live
sessions reject adoption, the response carries `failedSessionIds` alongside
the new generation; those sessions are not rolled back and silently retain
their previous generation until a subsequent `lato/plugins/reload` (or
session restart) advances them, so consumers can rely on a follow-up reload
to converge.

### Plugin hooks

Trusted, enabled plugins may declare `hooks/hooks.json` or an inline `hooks`
object. Phase 6B supports `SessionStart`, `SessionEnd`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `Stop`, `PreCompact`, and `PostCompact`, including
case/separator compatibility aliases. A canonical configuration is:

```json
{"hooks":{"PreToolUse":[{"matcher":"read_file|search","hooks":[{"type":"command","command":"./check","timeout":5},{"type":"http","url":"https://hooks.example.invalid/check"}]}]}}
```

Handlers execute sequentially in stable plugin/group order. Missing, empty, or
`*` matchers match all; simple `a|b` forms are exact alternatives and other
forms are regular expressions. Command hooks receive bounded JSON on stdin,
authentic `LATO_*` identity variables, a one-MiB combined output limit, and
process-tree cleanup on timeout or cancellation. HTTP hooks accept HTTPS only,
disable redirects, resolve and reject private/link-local/CGNAT/unspecified
destinations, and intentionally allow loopback for trusted local hooks. DNS is
checked before the request and the HTTPS client is pinned to those validated
addresses so a later lookup cannot reconnect to a blocked destination.

Failures and timeouts fail open; healthy explicit block decisions are enforced.
Defaults are 5s for observers and `PreToolUse`, 30s for `UserPromptSubmit`, 600s
for `PostToolUse`/`Stop`, and 1500ms for `SessionEnd` (explicit SessionEnd values
cap at 60s). `PreToolUse` may replace the complete argument object, but never
the resolved tool name. Every replacement is passed through schema validation,
PolicyEngine, approval, scope, and sandbox planning again; a hook `allow` grants
no authority. Stop continuation is capped at eight. Reloads apply at turn
boundaries, child sessions materialize hooks only after capability narrowing,
and lifecycle/compaction observer mutations are ignored. Audit records store
hashes and bounded metadata rather than prompt, arguments, output, environment,
or URL credentials.

### Plugin MCP

Trusted, enabled plugins may declare MCP servers in `.mcp.json` or an inline
`mcpServers` object. Phase 6C supports **stdio** and **streamable HTTP**
transports only. A canonical configuration is:

```json
{
  "mcpServers": {
    "demo-stdio": {
      "command": "python3",
      "args": ["server.py"],
      "env": { "DEMO": "1" },
      "cwd": "."
    },
    "demo-http": {
      "url": "http://127.0.0.1:9443/mcp",
      "headers": { "Authorization": "Bearer …" },
      "transport": "streamable-http"
    }
  }
}
```

Only descriptors from the turn's frozen `PluginSnapshot` of **trusted and
enabled** plugins are materialized. Untrusted project plugins never start MCP
child processes or HTTP client sessions. Command/`cwd` paths must stay inside
the plugin root; reserved identity environment variables cannot be forged by
server config.

By default the model sees only progressive discovery tools — `search_tool` and
`use_tool` — plus ordinary non-MCP builtins. `search_tool` retrieves qualified
`server__tool` names from the generation-scoped schema cache; `use_tool`
invokes one discovered tool. Optional direct expansion of a small allowlisted
server set into model tool definitions is opt-in and still passes the same
ToolRuntime membrane. Colliding qualified names do not silently overwrite.

Every MCP invocation is a ToolRegistry provider call: PreToolUse →
`prepare_scoped` → PolicyEngine → approval → execute → PostToolUse. There is no
second execution channel around `McpManager`. Streamable HTTP POSTs send
`Accept: application/json, text/event-stream`, advertise
`MCP-Protocol-Version: 2025-03-26` (and then the negotiated version), echo
`Mcp-Session-Id`, and read a JSON body or an SSE JSON-RPC result without waiting
for the stream to end. After initialize the client opens a GET SSE listener
(`405` is ignored; `ping` is answered; other server requests are rejected;
stream EOF reconnects with `Last-Event-ID`).
Shutdown `DELETE`s a session when one was assigned. A session `404` starts a
new session (initialize without `Mcp-Session-Id`) and retries the RPC once;
a second failure marks the server unhealthy. Endpoints are checked after DNS resolution (SSRF), never
follow redirects, allow plain `http` only on loopback, and redact URL
credentials from user-visible errors and journals. Oversized results are
truncated inline and spilled under `.lato/tool-output/`.

Stdio servers run in a fresh process group. Cancel, timeout, crash isolation,
and SessionEnd shut them down under a bounded deadline with process-tree reap.
A single unhealthy server does not take down peer MCP servers or builtin tools.
Plugin reload is generation-paired: in-flight turns keep generation N MCP
resources; the next turn adopts N+1 and retires the previous manager. Child
sessions may only **narrow** parent MCP server/tool allowlists (or drop
`ExtensionInvoke`); they cannot restore a parent-disabled plugin or a
previously removed tool.

Installed-command MCP smoke must use the cargo-installed binary
(`cargo install --path .` → typically `~/.cargo/bin/lato`). Prefer
`LATO_SMOKE_BINARY="$HOME/.cargo/bin/lato"` (or
`LATO_SMOKE_BINARY="$(command -v lato)"` only after confirming PATH order). An
older `~/.local/bin/lato` may shadow PATH and exercise a stale build.

### Plugin Workflows

Workflows are Rhai scripts (`agent`, `parallel`, `phase`, `complete`) run by
`lato workflow run`. Discovery is keep-first: `$LATO_HOME/workflows/*.rhai`,
then `.lato/workflows/*.rhai` in a trusted project, then trusted+enabled plugin
descriptors. Plugin JSON `prompt` / `steps` / `profile` (`explorer`, `worker`,
`reviewer`) compile into sequential `agent()` calls. Qualified plugin ids are
`plugin/workflow`.

```text
lato workflow list [--json] [--plugin-dir PATH]
lato workflow run <id> [--input JSON] [--model provider/model] [--sandbox off|workspace|read-only] [--validate-only] [--agent-budget N] [--plugin-dir PATH]
```

`--model` / `LATO_MODEL` match `-p`; with neither, the run uses the built-in
fake stream. `--validate-only` compiles and walks one canned path without
spawning child sessions or calling a model. `agentBudget` is a logical-agent
cap (default 128, max 1024). Untrusted project plugins and untrusted
`.lato/workflows` contribute nothing. Child sessions may only narrow the parent
allowlist. A run that pauses reports the stable `workflow.paused` error code
(not `workflow.failed`); resuming a paused CLI run is cross-process and is
deferred to a later phase. `doctor` does not list workflows.

### In-session workflow runs (Phase 7B4)

Each ACP/TUI session owns an in-memory `WorkflowManager`: at most 4 active
runs, session-unique display names (`review`, `review-2`, …), and a same-process
journal so `await_user` / `pause` runs replay in place on `resume`. Child
agents inherit the parent session's `ToolApproval`.

- `/workflows` lists saved workflow definitions (keep-first: user → trusted
  project → trusted plugins) with id, name, description, source, budget.
- `/workflow <id> [json-args]` starts a background run and immediately reports
  `{ displayName, runId, status: "active" }`; the internal `runId` (`wf_…`)
  never appears in slash arguments.
- `/workflow runs` opens the live board overlay: display name, status, phase,
  agents used / budget, elapsed, pause message. Hotkeys: `p` pause, `r` resume,
  `x` stop, `Esc`/`q` close. `/workflow pause|resume|stop <displayName>` is
  equivalent. `BudgetLimited` runs resume only with a higher budget.
- Progress arrives as `session/update` notifications
  (`sessionUpdate: "lato/workflow"`); the main prompt turn is never occupied by
  a workflow. Every run journals under
  `$LATO_HOME/sessions/<sessionId>/workflows/<runId>/` (`run.json`,
  `script.rhai`, `journal.jsonl`). After `session/resume` the board rebuilds
  from disk with the original display names, and paused / blocked / failed /
  cancelled runs resume in-session (`/workflow resume`); `budget_limited`
  still needs a higher budget. Runs that were still active at process exit
  are restored as terminal `interrupted` and are not resumable;
  `session/close` also marks still-active runs `interrupted` while paused
  runs stay on disk. `CLI lato workflow resume|pause|stop` does not exist (no
  resident process; cross-process resume is session-bound).
- Workflow scripts get live host helpers: `write_scratch_file(name, body)` /
  `read_scratch_file(name)` (single-component names; 1 MiB per file, 8 MiB and
  64 files per run; failure codes are stable, e.g. `scratch byte quota
  exceeded`), the builtin `render_template` catalog (`identity`), and
  `git_diff_since(commit)` on the session cwd. In-session scratch files live
  under the run directory (`…/workflows/<runId>/scratch/`) and survive
  `session/resume`; `fork_context` stays unsupported.

### Model-visible workflow tool (Phase 7B7, spec v1.2)

The main-session model catalog carries exactly one builtin tool `workflow`
(`builtin:workflow`, v1.2.0) so the model can drive named workflows without
slash commands. It is a thin, session-bound adapter over the same
`WorkflowManager` the board and ACP use — no second turn loop, manager, or ACP
self-call — and subagent catalogs never include it.

- `{"action":"list"}` returns the trust/plugin snapshot's named scripts
  (keep-first order, at most 64 entries, `truncated` flag) with
  `id` / `name` / `description` / `source` / `agentBudget` and a content
  `revision` — the lowercase SHA-256 of the canonical JSON
  `{id, source, script, declaredAgentBudget}`. Never script bodies or disk
  paths.
- `{"action":"start","name":"<qualified id>","revision":"<64-hex>","agentBudget":N,"args":{…}}`
  must carry the listed qualified id, revision, and an explicit budget; the
  wrong combination is rejected by the schema (`oneOf`) before any approval is
  requested. The pre-policy fingerprint binds these exact canonical arguments.
  At invoke time the workflow is re-resolved and its revision is
  constant-time-compared: any mismatch returns `workflow.catalog_changed`
  with zero side effects (the one-shot grant was already consumed and is
  never restored). On match the background run launches through the same
  manager and the tool returns its initial snapshot immediately.
- `{"action":"status","run":"<runId|displayName>"}` reports one real run, or
  the bounded recent-run list without `run`. Model-facing `status` is
  normalized to `active` / `paused` / `completed` / `interrupted`, while
  `detailStatus` stays lossless (`user_paused`, `budget_limited`, `failed`,
  …). Runs restored after a crash report `interrupted`, never `active`.
- Authorization strictly follows the existing `PolicyMode`: `Ask` requests
  human approval (the summary shows the resolved workflow id, source,
  effective budget, and args digest; a rejection yields the stable
  `policy.approval_denied` with zero side effects); `Auto` / `Always`
  automatically issue one-shot grants. `PolicyDecision::Deny` is a decision
  result, not a fourth mode (e.g. an illegal sandbox obligation denies with
  `sandbox.unsupported`); policy rejection codes are never rewritten into
  `workflow.*` errors.
- Stable error codes: `workflow.invalid_arguments`, `workflow.not_found`,
  `workflow.duplicate_name` (retry with the qualified id),
  `workflow.catalog_changed` (re-list then retry),
  `workflow.unavailable`, `workflow.too_many_active_runs` (4 active per
  session), `workflow.persistence_failed`, `workflow.run_not_found`,
  `workflow.output_too_large` (64 KiB output ceiling).
- Boundaries: the tool is external-mutation, so `list` / `status` / `start`
  all pass the ordinary policy membrane; model-initiated `pause` / `resume`
  / `stop` do not exist (user-only, via TUI/ACP); internal workflow actions
  keep their own sandbox/trust/approval checks; output is entry-bounded and
  JSON-encoded.

## Doctor

```bash
lato doctor
lato doctor --json
lato doctor --strict
lato doctor --live
```

Default `lato doctor` is offline: it does not contact providers or submit a prompt completion. It reports binary/platform, Lato home, config parsing, selected-model catalog presence, credential presence (not values), ToolCatalog construction, a fixed PolicyEngine self-test, sandbox readiness, and project/plugin trust. `--json` prints a `schema_version: 1` report on stdout. Warnings keep exit status 0; errors return 1. `--strict` upgrades warnings to failure. `--live` is the only Doctor mode allowed to use the network; it runs a bounded catalog/connectivity probe and does not submit an ordinary prompt completion. When an AgentField control plane is configured, the offline report additionally covers its configuration state (disabled / unconfigured / invalid); `--live` performs one explicit opt-in discovery probe through the frozen network policy (Phase 7C1.1) and classifies the result as ok / unreachable (`agentfield.unavailable`) / authentication rejected (`agentfield.unauthorized`) / protocol mismatch (`agentfield.remote_protocol`). Disabled, unconfigured, and unresolvable-credential states make zero network requests.

## AgentField adapter (Phase 7C1 + 7C1.1: offline contract, production transport)

Lato acts as a client of an organization-deployed [AgentField](https://github.com/Agent-Field/agentfield) control plane. **Phase 7C1 (v1.2.1)** delivered the offline contract foundation; **Phase 7C1.1** adds the production HTTPS transport behind a single policy-enforcing network boundary. Phase 7C1 delivered:

- the configuration model (enabled/baseUrl/credential reference/capability allowlist with alias, count, and size limits) and pure base-URL syntax checks;
- strict pinned-contract wire types (`v0.1.138`, commit `0aba9d6de1ef2c473070fc329ac7ac63e5d096b9`, fixture SHA-256 `fd524cab…3ae8`) with injectable fake-transport contract tests;
- the discovery colon→dot execute-target derivation contract;
- a 30-second health/discovery snapshot state machine (fake client + controllable clock only);
- static `lato doctor` checks for configuration shape, credential-reference resolvability, and pinned-version configuration.

**Phase 7C2 adds the model tool**: the main-session `agentfield` tool (four actions: `list` / `start` / `status` / `cancel`) is registered when the adapter is enabled AND the config validates AND the credential resolves — unconfigured / disabled / invalid means zero registration and zero network.

**Phase 7C3 journal 与跨进程恢复（frozen）:**

- **持久 journal 事件**：每次 AgentField run 的启动意图（`agentfield_run_intent_recorded`）、远端执行绑定（`agentfield_execution_bound`）、状态观察（`agentfield_status_observed`）、取消请求（`agentfield_cancel_requested`）与终态（`agentfield_run_terminal`）都会通过会话 journal 持久化（agentfield envelope 使用 journal schema version 2）。事件只包含关联信息与 ≤8 KiB 的脱敏有界摘要——绝无 token、凭据、原始输入、完整远端输出或 transcript。
- **远端在 Lato 退出后继续运行**：session close / 进程退出只关闭本地 client，绝不隐式取消远端 execution。重启后 `lato resume` 仍能看到本会话的 run；已绑定 execution id 的非终态 run 在首次显式 `status` 时惰性查询一次（不创建新 execution），查询失败显示 `agentfield.unavailable` 并保留最后已知状态。
- **人工对账**：启动意图已持久化但执行 id 未绑定（响应丢失/断线/中断）的 run 恢复为永久本地终态 `outcome_unknown`，自动 retry/query/reconcile 次数恒为 0。此时只能由人工到 AgentField 控制面按时间、target 与审计记录核对，除非接受重复执行风险，否则不要重新 start。
- **配置消失**：`agentfield` 配置被删除、adapter disabled 或 credential 缺失时，历史 run 与 journal 一律保留、绝不改写；已启用会话内配置被冻结（旧 revision 不能用于新 start）。重新启用后 resume 会照常恢复 run。
- **Retention**：每 session 内存最多保留 32 个 run，只淘汰最老终态（按提交顺序，必要时以 local run id 稳定 tie-break），非终态 run 永不被静默淘汰；journal 历史事件不因内存淘汰被删除，重放为 O(events) 且受 store 的 64 MiB / 100k 记录上限约束。
- **Schema migration 与不可降级**：不含 7C 事件的旧 journal 完全兼容。首次写入 agentfield 事件后，旧二进制会在该记录处显式 fail closed（reader-version 门禁），因此**对这些 session 二进制降级不安全**——必须保留新 reader，或先完成导出/迁移；尚未产生 7C 事件的 session 仍可按既有路径回滚。`lato doctor` 的 `agentfield_journal` 检查会报告磁盘上哪些 session 已含 7C 事件。

Migration and rollback: set `enabled: false` (or delete the `agentfield` section) to deactivate the adapter; unconfigured/disabled states make no network requests and resolve no credentials, and all existing task/workflow behavior is untouched (tool catalog and goldens unchanged). Sessions without 7C3 AgentField journal events can always roll back to an older binary; sessions WITH such events cannot (see above). Run `lato doctor` (offline) after config changes; `lato doctor --live` runs the opt-in AgentField discovery probe.**Phase 7C1.1 network boundary (frozen):** production code obtains its AgentField client only through one policy-enforcing factory (`production_agentfield_client` → `ReqwestTransport::connect`); no public constructor accepts a raw client, an arbitrary resolver, a pinned address set, or disabled verification. Every request: origin re-checked (scheme/lowercase host/effective port; userinfo/query/fragment/zone-id/IDNA/trailing-dot hosts refused), DNS re-resolved with the full answer classified (empty/oversized/duplicate/mixed answers fail closed; production HTTPS dials public addresses only, explicit-development plain HTTP dials loopback only; IPv4-mapped IPv6 obeys the IPv4 policy), and the connection pinned to the just-validated address set (TLS keeps verifying the original host name — no SNI bypass). Redirects are never followed (any 3xx is a stable rejection), proxies are disabled, TLS is rustls with default verification, and connect/idle/total timeouts plus response header and body caps (the body cap applies to the decompressed stream) bound every request. No automatic retries. The arbitrary-resolver test seam exists only under `#[cfg(test)]`.

Configuration lives in the top-level `agentfield` section of `~/.lato/config.json`. The `enabled`, `baseUrl`, and `credential` keys are required — omitting any of them is a parse error reported by `lato doctor`; `allowLoopbackHttp` and `capabilities` are optional and default to `false` / empty; unknown keys are rejected. Set `"enabled": false` to disable the adapter:

```json
{
  "agentfield": {
    "enabled": true,
    "baseUrl": "https://agents.example.internal",
    "credential": "agentfield:primary",
    "capabilities": {
      "contract-review": {
        "target": "legal-agent.review_contract",
        "description": "Review one contract and return structured findings",
        "inputSchema": { "type": "object", "additionalProperties": false },
        "risk": "remote_read",
        "timeoutSeconds": 900,
        "maxOutputBytes": 65536
      }
    }
  }
}
```

Contract guarantees: `baseUrl` accepts only an absolute HTTP(S) URL without userinfo/query/fragment (plain HTTP only for loopback hosts under the explicit `allowLoopbackHttp` development flag); `credential` is a reference (`agentfield:<key>` resolved from the Lato credential store or `LATO_AGENTFIELD_CREDENTIAL`), and the token value never appears in logs, errors, doctor output, or `Debug` formatting; envelopes are strictly decoded against the pinned contract with unknown fields ignored and any missing/mistyped required field failing closed; discovery targets must satisfy the colon→dot derivation contract; the pinned async-start endpoint has no idempotency key and Lato adds no retry guarantees. Dual-policy note (spec §8): every AgentField action passes Lato's policy/approval membrane first; an AgentField-side PASS never upgrades Lato permissions.


## Headless CLI smoke test

The default phase fixture is offline and does not require a credential:

```bash
LATO_HOME=$(mktemp -d) lato -p "reply with hi only"
# hi
```

The Phase 5 release gate also contains a command-level task smoke that drives the
real `lato` executable through a parent `spawn` → child worker → `inspect` → `wait`
chain, validates the persisted journal, and checks that temporary worktrees are
cleaned up. Run it against the just-installed binary with:

```bash
cargo install --path .
# Prefer ~/.cargo/bin/lato; an older ~/.local/bin/lato may shadow PATH.
LATO_SMOKE_BINARY="${LATO_SMOKE_BINARY:-$HOME/.cargo/bin/lato}" \
  cargo test --test phase5_command_smoke -- --nocapture
# Phase 6C MCP smoke (same binary pin):
# LATO_SMOKE_BINARY="$LATO_SMOKE_BINARY" \
#   cargo test --test phase6c_mcp_smoke -- --nocapture
```

Use a real catalog model by selecting `provider/model`. Credentials resolve in this order: runtime override, persisted OAuth, persisted API key, then provider environment variables.

Built-in China-region providers include:

| Provider | Protocol/base URL | Credential |
|---|---|---|
| `minimax-cn` | Anthropic Messages, `https://api.minimaxi.com/anthropic` | `MINIMAX_CN_API_KEY` |
| `minimax` | Anthropic Messages, `https://api.minimax.io/anthropic` | `MINIMAX_API_KEY` |
| `zai-coding-cn` | OpenAI Completions, `https://open.bigmodel.cn/api/coding/paas/v4` | `ZAI_CODING_CN_API_KEY` |
| `zai` | OpenAI Completions, `https://api.z.ai/api/coding/paas/v4` | `ZAI_API_KEY` |
| `sensenova` | OpenAI-compatible, `https://token.sensenova.cn/v1` | `SENSENOVA_API_KEY` bearer token |

The first four definitions are Rust translations of the reference TypeScript provider factories. Their static catalog is overlaid by the reference-compatible remote catalog endpoint `/api/models/providers/{provider}` and persisted per provider in `~/.lato/models-store.json` with `checked_at`, `last_modified`, and `etag`; fresh cached catalogs are restored without network access. Lato does not blindly append `/models` to these providers. SenseNova is not defined by the reference registry and remains an explicit compatibility provider based on its OpenAI SDK configuration. Lato sends the supplied token to `https://token.sensenova.cn/v1`, discovers models from `/models`, and samples from `/chat/completions`. This token API key is distinct from the legacy platform Access Key ID/Secret pair.

```bash
export LATO_HOME="$HOME/.lato"
lato login openai --api-key "$OPENAI_API_KEY"
lato -p --model openai/gpt-4.1 --sandbox workspace "inspect this repository and fix the tests"
```

Custom OpenAI-compatible endpoints are read from `$LATO_HOME/models.json`:

```json
{
  "models": [
    {
      "provider": "local",
      "id": "qwen",
      "api": "openai-completions",
      "base_url": "http://127.0.0.1:8080/v1",
      "env": "LOCAL_API_KEY"
    }
  ]
}
```

## Build and test from source

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo install --path .
```

Windows compile gate from macOS/Linux with the Rust target installed:

```bash
cargo check --workspace --target x86_64-pc-windows-gnu
```

## ACP stdio

```bash
lato acp
```

Each input and output is one JSON-RPC object per line. `session/load` is intentionally unsupported; use `session/resume`.

## OAuth

Only `kimi-coding` and `openai-codex` advertise OAuth. Other providers reject OAuth rather than silently changing credential channels.

```bash
lato login kimi-coding --oauth
lato login openai-codex --oauth
lato login openai-codex --oauth --device-auth
lato -p --model openai-codex/codex-mini-latest "reply with hi only"
```

`openai-codex` uses ChatGPT subscription authentication, not an OpenAI Platform API key. Browser login is the default: Lato listens once on loopback port `1455` for the OAuth callback. Use `--device-auth` on a headless machine and enter the displayed code at the verification URL. A ChatGPT plan with Codex access is required.

Lato owns this login independently. It stores the access token, refresh token, expiry, and ChatGPT account identity in `$LATO_HOME/auth.json` (normally `~/.lato/auth.json`) and refreshes them itself. It does not read or modify Pi's or the Codex CLI's credential files.

Model calls use `https://chatgpt.com/backend-api/codex/responses`, prefer a session-affine WebSocket, and reuse healthy connections. If WebSocket setup fails before any response event, Lato falls back to an incrementally parsed SSE request with a zstd-compressed body. It never replays a request after model output has begun, preventing duplicated text or tool calls.

## Public Beta limitations

- Homebrew, Scoop, and other package-manager channels are not maintained yet.
- LIVE provider validation is reported separately and may be unavailable when repository credentials are not configured. Offline protocol and localhost end-to-end tests still run in ordinary CI.
- `~/.lato` may contain prompts, paths, tool arguments, tool results, credentials, and policy metadata. Treat the entire directory as sensitive local data.

## Runtime architecture

Lato's headless, interactive, and stdio clients share the ACP host. ACP sessions submit typed
commands to `lato-runtime` and consume versioned events from it. `lato-core` owns provider-independent
IDs, errors, commands, events, and session invariants; `lato-runtime` owns cancellation, steering,
replacement, event ordering, and the `TurnDriver` boundary.

The existing model/tool loop currently runs behind `LegacyTurnDriver`, a compatibility adapter around
`SessionActor`. This keeps provider, tool, approval, and transcript behavior stable while Phase 2
moves those capabilities behind dedicated ports.

Phase 2A adds provider-independent `ModelPort` and `Tool` contracts in `lato-core`, plus a layered,
fail-closed `ToolCatalog` in `lato-tools`. Phase 2B-1 now routes every configured CLI and ACP HTTP
provider through `ModelPort`. A bidirectional adapter keeps `SessionActor` on its legacy JSON/channel
contract temporarily, while the existing HTTP authentication, request builders, retry policy, and SSE
parsers remain the protocol authority.

`ModelPort` is the canonical provider extension boundary. New providers should implement it directly;
`LegacyModelPort` and `ModelPortStreamAdapter` exist only to migrate the current providers and actor
independently. A later actor-native phase removes the redundant legacy encode/decode round trip without
changing the core request, event, capability, cancellation, or typed-error contracts.

### Canonical session journal

Sessions backed by `LATO_HOME` persist canonical records at
`$LATO_HOME/sessions/<session-id>/events.jsonl`. The journal contains accepted turn input, complete
conversation items, policy decisions, tool lifecycle boundaries, and turn/session terminals. Model
and reasoning deltas remain live-only events: they are never replayed as authoritative state.

Journal appends are acknowledged before related visible state is broadcast. A mutating or
non-idempotent tool is synchronously recorded as prepared before invocation and synchronously
recorded as completed afterward. If recovery finds a prepared call without a completion, Lato
reports `journal.incomplete_side_effect` (an unknown outcome) and does not automatically invoke the
tool again.

Replay is bounded to 64 MiB and 100,000 records per session. It validates schema, session identity,
dense sequence numbers, unique record IDs, and tool request hashes. Only an interrupted final JSONL
record is repaired; a complete malformed or structurally divergent record fails closed. On Unix,
journal files use mode `0600`, session directories use `0700`, and symlink journal targets are
rejected. Journals may contain prompts, tool arguments/results, paths, and policy metadata, so the
entire Lato home should be treated as sensitive local data.

Legacy `$LATO_HOME/sessions/<session-id>.jsonl` transcripts are imported lazily and atomically on
first resume. The original transcript remains unchanged for compatibility; once both formats exist,
the canonical journal wins.

Phase 4B follows Grok Build's source-stream/derived-history split. The append-only
`events.jsonl` remains the authority, while `history.jsonl` and `history.meta.json`
materialize the current model-visible conversation. Ordinary records reach the canonical
journal before the derived projection. Missing or stale projection files are rebuilt after
the complete journal passes validation; damaged projection files are quarantined before
rebuild. Logical history replacement publishes a private compaction checkpoint before its
marker is synchronized to the canonical journal. A missing or mismatched referenced
checkpoint, canonical corruption, or an unresolved side effect still fails closed. Phase 4B
does not rotate, truncate, archive, or delete canonical journal records, and the Phase 4A
64 MiB/100,000-record bounds remain in force.

Phase 4C1 adds Grok Build-style manual context compaction on top of that storage
boundary. `CompactionRequested` is durable before sampling becomes visible. The
summary request uses the current model with `tools: []` and `tool_choice: none`, has a
three-attempt ceiling, and must satisfy section, size, and reduction checks. A successful
checkpoint marker becomes authoritative even if publishing `history.jsonl` or its
metadata fails; replay rebuilds those derived files before the new history is installed
in memory. If reconciliation cannot prove which checkpoint is authoritative, the
session fails closed.

Phase 4C2/4C3 add provider-informed context accounting and bounded automatic recovery.
Every provider boundary is checked before sampling; speculative prefire output is
ephemeral and never becomes a transcript or journal record. Only a validated final
summary is installed through the same checkpoint-before-marker protocol, so restart
recovery sees either the old logical history or the committed compacted history, never
an intermediate two-pass note.

Copied or structurally derived upstream code is pinned in
[`docs/superpowers/reference/lato-upstream-sources.md`](docs/superpowers/reference/lato-upstream-sources.md).

## Security

- Headless `-p` uses process-local workspace trust and `approval_mode=always`; deny rules still apply.
- Interactive sessions default to `ask` and mutating tools require an explicit approval token.
- Approvals are bound to an exact-call fingerprint (canonical tool identity, arguments, capabilities, side effect, sandbox obligation, and call IDs). Grants are short-lived, single-use, and cannot be replayed.
- Shell sandbox profiles are `off`, `workspace`, and `read-only`. A missing or unusable wrapper fails closed and never degrades into unsandboxed process execution.
- File reads/edits execute in the host; only shell commands enter the OS sandbox.
- Project plugin hooks and MCP processes remain disabled until the project is trusted.
- `web_fetch` rejects loopback, private, link-local, and non-HTTP(S) destinations.
- Doctor output is redacted; credential values and seeded secrets must not appear in human or JSON reports.

See `docs/superpowers/specs/2026-08-31-lato-acceptance.md` for the authoritative acceptance matrix. LIVE provider and subscription tests require real credentials and are not run in PR CI.
