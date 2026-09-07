# Lato Phase 6B: Skills and Hooks Design

Date: 2026-09-07

Status: proposed for implementation review

## 1. Purpose

Phase 6B ports Grok Build's skill discovery and lifecycle-hook behavior onto
the immutable plugin runtime delivered by Phase 6A. It gives trusted, enabled
plugins two bounded extension surfaces:

- `SKILL.md` files that contribute compact model-visible descriptions and can
  be invoked explicitly through a built-in tool;
- command and HTTP hooks around session, prompt, tool, stop, and compaction
  lifecycle events.

These extensions do not bypass Lato's authority model. Every skill-derived
tool restriction and every hook-rewritten tool request is narrowed again by
the frozen session snapshot, `PolicyEngine`, approval state, sandbox, and
subagent profile before execution.

Phase 6B consumes only the trusted and enabled component descriptors already
present in the active Phase 6A `PluginSnapshot`. It does not independently scan
plugin roots and does not start MCP servers.

## 2. Upstream baseline and porting policy

The behavioral baseline is Grok Build commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138`, especially:

- `crates/codegen/xai-grok-hooks/src/config.rs`;
- `crates/codegen/xai-grok-hooks/src/event.rs`;
- `crates/codegen/xai-grok-hooks/src/result.rs`;
- `crates/codegen/xai-grok-hooks/src/discovery.rs`;
- `crates/codegen/xai-grok-hooks/src/dispatcher.rs`;
- `crates/codegen/xai-grok-hooks/src/runner/command.rs`;
- `crates/codegen/xai-grok-tools/src/implementations/skills/types.rs`;
- `crates/codegen/xai-grok-tools/src/implementations/skills/discovery.rs`;
- `crates/codegen/xai-grok-tools/src/implementations/skills/skill.rs`;
- `crates/codegen/xai-grok-agent/src/prompt/skills.rs`;
- the shell session hook seams in `acp_session_impl/tool_calls.rs`,
  `stop_gate.rs`, and `run_loop.rs`.

Substantially derived production files retain upstream source and license
headers and are recorded in
`docs/superpowers/reference/lato-upstream-sources.md`.

Lato keeps Grok's observable semantics while adapting ownership boundaries:

- extension parsing and execution live in `lato-extensions`;
- lifecycle orchestration remains in `lato-agent`;
- canonical journal contracts live in `lato-protocol`;
- Lato's existing policy, approval, sandbox, and immutable-turn mechanisms
  remain authoritative;
- hook failures are isolated and fail open, but an explicit decision from a
  successfully completed hook is enforced;
- Phase 6A plugin trust and enablement are prerequisites for all discovery and
  execution.

Grok-specific marketplace, telemetry, LSP, branding, compatibility folders,
and MCP behavior are excluded.

## 3. Scope

Phase 6B delivers:

- deterministic discovery and parsing of plugin `SKILL.md` files;
- bounded skill frontmatter and body handling;
- skill catalog description injection into the system context;
- explicit skill invocation with argument substitution;
- skill-specific tool narrowing;
- hook configuration parsing for command and HTTP handlers;
- `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`,
  `PostToolUse`, `Stop`, `PreCompact`, and `PostCompact` dispatch;
- matcher-based filtering and deterministic handler ordering;
- bounded timeouts, output capture, process cleanup, and failure isolation;
- prompt blocking, tool approval decisions, argument rewriting, post-tool
  output replacement, stop continuation, and context injection;
- canonical audit events that exclude raw secrets and payloads;
- reload reconciliation at snapshot-safe boundaries;
- parent/child capability narrowing;
- unit, integration, recovery, and installed-command smoke tests.

Phase 6B does not deliver:

- MCP server startup, discovery, or tool calls (Phase 6C);
- plugin installation, updates, marketplace support, or remote downloads;
- arbitrary executable code embedded in `SKILL.md`;
- hooks from untrusted or disabled plugins;
- hooks that change a tool name, session identity, policy, approval scope,
  sandbox mode, subagent profile, or plugin snapshot;
- automatic privilege elevation based on a skill's `allowed-tools` field;
- filesystem watchers or mid-turn adoption of a reloaded plugin generation;
- a stable Rust dynamic-plugin ABI.

## 4. Architecture and ownership

```text
Arc<PluginSnapshot generation N>
              |
       component materializer
         /                 \
 SkillCatalog          HookRuntime
 descriptions/body     config/handlers
         |                 |
 system context +      lifecycle dispatcher
 built-in skill tool       |
         |          command runner / HTTP runner
         \                 /
          immutable TurnConfig
                   |
     PolicyEngine -> approval -> sandbox
                   |
             canonical journal
```

`lato-extensions` gains two internal domains:

- `skills`: descriptor types, frontmatter parser, snapshot materialization,
  prompt rendering, explicit invocation, and argument substitution;
- `hooks`: config types, event payload/result types, matcher compilation,
  dispatcher, command runner, HTTP runner, timeout/output bounds, and runtime
  reconciliation.

`lato-agent` owns the exact lifecycle dispatch points and translates hook
results into existing session/tool/compaction control flow. The current
synchronous `lato-agent/src/hooks.rs` stub is replaced by the asynchronous,
snapshot-backed adapter; it is not retained as an alternate hook path.

`lato-protocol` owns versioned, serializable journal records for skill and hook
activity. CLI and TUI layers render existing session errors and diagnostics but
do not execute extensions.

All catalogs and handler sets are immutable per plugin generation. The hook
runtime may own live subprocess or HTTP client resources, but it cannot mutate
the descriptor set associated with an issued snapshot.

## 5. Skill discovery and identity

For each trusted, enabled plugin in the frozen snapshot, Phase 6B walks only
the skill roots resolved and containment-checked by Phase 6A. A skill root may
point directly to a directory containing one `SKILL.md`; otherwise Lato walks
sorted child directories for `SKILL.md` to Grok's maximum depth of five.

Candidate paths are canonicalized, proven to remain within the canonical
plugin root, and sorted by canonical path. An unreadable, escaped, oversized,
or malformed candidate is excluded with a bounded diagnostic; other skills
remain available.

The public identity is `<plugin-name>:<skill-name>`. The qualified identity is
used in prompt descriptions, explicit invocation, audit records, and collision
handling. An unqualified name is accepted only when it resolves to exactly one
skill in the frozen catalog; ambiguity returns a deterministic error listing
the qualified candidates. Within one plugin, the first canonical path wins a
duplicate skill name and the duplicate is diagnosed.

Skill names use Grok's 1-64 character lowercase ASCII letter, digit, and hyphen
grammar. Frontmatter and directory fallback names are trimmed, lowercased,
non-alphanumeric runs become one hyphen, and edge hyphens are removed before
validation. If frontmatter omits or cannot yield a valid `name`, a valid
normalized candidate-directory name is used. A candidate is excluded only
when neither source produces a valid identity.

The Grok-compatible parsing bounds are:

- `SKILL.md`: 256 KiB;
- frontmatter: 4 KiB;
- description and `when-to-use`: 1,024 characters each after normalization;
- body peek used to derive a missing description: 2 KiB;
- directory walk depth: five;
- argument hint: 512 bytes;
- allowed-tool entries: 128 entries, 128 bytes each;
- model-visible catalog: 128 skills and 64 KiB total rendered text;
- invoked skill body: 128 KiB after argument expansion.

Exceeding a per-file structural bound excludes that skill. Exceeding a catalog
rendering bound deterministically truncates after sorted complete entries and
records how many entries were omitted; it never emits a partial entry.

## 6. `SKILL.md` contract

Phase 6B accepts YAML frontmatter followed by Markdown. The supported fields
are deliberately bounded:

```yaml
---
name: code-audit
description: Review a change for correctness and regressions.
when-to-use: Use after a non-trivial code change.
argument-hint: "[path]"
allowed-tools:
  - read_file
  - search
user-invocable: true
disable-model-invocation: false
---
```

Unknown fields are ignored for forward compatibility. Scalar metadata is
trimmed and string-coerced where Grok does so; `allowed-tools` accepts either a
YAML list or a comma/whitespace-delimited string while preserving grouped tool
specifications. A malformed optional field is diagnosed and dropped rather
than dropping the skill. Missing `description` falls back to bounded body
prose for explicit lookup. Missing booleans default to
`user-invocable: true` and `disable-model-invocation: false`; only YAML `true`
or the exact string `"true"` is true, matching Grok compatibility behavior.
The parser also preserves `paths`, `license`, `compatibility`, string-valued
`metadata`, `model`, and `effort` when present. `paths` may narrow future
announcements, while model/effort overrides remain inert in Phase 6B because
they would alter execution authority outside this scope.

`allowed-tools` is a narrowing declaration. At invocation it is intersected
with the tool catalog already visible to the session and, for a child session,
with the child's profile allowlist. Missing or empty `allowed-tools` adds no
extra skill-specific restriction. Naming an unavailable tool neither exposes
nor authorizes it. Tool aliases are resolved by the same catalog resolver used
by ordinary tool calls; unresolved names are diagnosed and omitted.

The Markdown body is treated as instructions, never executable configuration.
Only plugin skills with an explicit frontmatter `description` or
`when-to-use` are eligible for model-visible listing, matching Grok's plugin
skill rule. Their compact identity, description, invocation eligibility, and
`when-to-use` text are rendered into the normal system context. Full bodies
are loaded from the immutable materialized catalog only when the built-in
`skill` tool invokes one. A body-derived description remains useful for UI and
explicit lookup but does not make a plugin skill model-discoverable.

## 7. Description injection and explicit invocation

At turn construction, Lato renders a deterministic `<available_skills>` block
from the turn's frozen catalog. Entries are sorted by qualified identity and
contain no absolute path or plugin secret. Skills with
`disable-model-invocation: true` are omitted from model-visible descriptions
but remain available to an explicit user invocation when `user-invocable` is
true.

Phase 6B adds one built-in `skill` tool with this conceptual input:

```json
{
  "skill": "example-plugin:code-audit",
  "args": "src/runtime.rs"
}
```

The tool resolves the name against the frozen turn catalog, checks model
invocation eligibility, expands arguments, and returns the body wrapped as:

```xml
<skill name="example-plugin:code-audit" description="..." path="...">
...expanded Markdown body...
</skill>
```

Expansion follows Grok behavior: `$ARGUMENTS` expands to the full argument
string; `$ARGUMENTS[N]` and `$N` use zero-based whitespace-tokenized arguments
without an artificial single-digit limit; missing positions become empty
strings. `${SKILL_DIR}`/`${CLAUDE_SKILL_DIR}`,
`${SESSION_ID}`/`${CLAUDE_SESSION_ID}`, and
`${LATO_PLUGIN_ROOT}`/`${CLAUDE_PLUGIN_ROOT}` expand from immutable invocation
context. Grok's plugin-data token is not supported because Phase 6A explicitly
excluded plugin data directories. Unknown dollar tokens remain unchanged. If
the body consumes no argument token, non-empty arguments are appended as
`**ARGUMENTS:** ...`. Expansion is textual only and never launches a shell.
The body is rejected if expansion exceeds the invoked-body bound.

An explicit user-facing slash-command adapter may call the same built-in tool,
but it must not have separate discovery or authorization behavior. A model
cannot invoke a skill marked `disable-model-invocation`, and a user cannot
invoke one marked `user-invocable: false`.

While a skill body is active for the current model step, its effective tool
catalog is the intersection described above. The next ordinary step restores
the enclosing turn's catalog. A skill cannot carry tool grants into later
turns or child sessions.

## 8. Hook configuration and ordering

Each trusted, enabled plugin may contribute the hook configuration path
resolved by Phase 6A. The canonical file shape is:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "read_*|search",
        "hooks": [
          { "type": "command", "command": "./hooks/check", "timeout": 5 },
          { "type": "http", "url": "https://hooks.example.invalid/check" }
        ]
      }
    ]
  }
}
```

The supported event names are the eight canonical names in scope. Parsing also
accepts Grok-compatible case and separator aliases, which normalize to one
canonical event before storage and audit. Unknown events, invalid matchers,
unsupported handler types, and malformed handler entries are isolated as
bounded diagnostics.

Matchers are optional. A missing matcher, an empty matcher, or `*` matches all.
A pattern containing only ASCII alphanumerics, `_`, and `|` is an exact name or
exact-name alternative list. Any other pattern is an unanchored regular
expression, with user-supplied anchors honored. Both forms are also tested
against Lato's registered compatibility aliases for the immutable tool name.
An invalid regular expression excludes that matcher group rather than widening
it to match all. `UserPromptSubmit` and `Stop` ignore configured matchers, as
Grok does; the other in-scope events test the event's documented match value.
Patterns are bounded to 1 KiB and 64 simple-form alternatives.

Dispatch order is stable:

1. plugin name;
2. canonical hook configuration path;
3. event group order in the parsed file;
4. handler order within the group.

Only handlers from the turn's frozen snapshot are dispatched. Handlers run
sequentially within one event so argument rewrites and gate decisions have an
unambiguous order. Independent event dispatches may run concurrently only when
they belong to different idle sessions and share no mutable process handle.

## 9. Handler runners and resource bounds

### 9.1 Command handlers

Command handlers preserve Grok's execution rules. A simple relative command
path resolves from the hook configuration's source directory; an absolute path
is used as written; and a command containing shell syntax runs through the
platform shell. The process working directory is the session workspace. This
surface is available only because Phase 6A has already established plugin
trust. Lato applies its existing child-network restriction and process scope.

The canonical event payload is supplied as one JSON object on standard input.
Handler `env` entries are supported. Reserved identity keys are stripped from
plugin configuration, then Lato overwrites them at spawn with authentic event,
hook, session, and workspace values. Other process environment behavior
matches ordinary trusted Lato child processes.

Standard output and error are captured with Grok's one-MiB runner cap, while
model-influencing reasons, context, and replacements have the smaller bounds
below. Timeout or cancellation terminates the registered process group and
reaps it. Session end also terminates all owned handler process groups within
its total shutdown budget.

Exit code zero is a successful response. Exit code two is Grok's explicit
blocking convention and is decoded as a deny when the response body does not
already provide a more specific valid decision. Any other exit, signal,
malformed JSON, spawn error, or truncated structured response is a handler
failure and follows fail-open isolation.

### 9.2 HTTP handlers

HTTP handlers use `POST` with a JSON body equal to the canonical event payload
and expect one bounded JSON result. Grok's HTTP safety contract is retained:
only `https` URLs are accepted; redirects are disabled; unspecified,
link-local, carrier-grade NAT, and private/internal address ranges are blocked
after DNS resolution. Loopback remains allowed so trusted local development
hooks work. The URL is expanded from configured and authenticated identity
environment immediately before validation. Response capture uses the same
one-MiB runner cap and log previews are separately bounded.

Phase 6B sends no plugin-defined headers and no ambient authorization header.
The implementation must document the same resolution-time DNS-rebinding limit
as the pinned Grok baseline; adding connection-time address pinning is a safe
hardening only if it preserves public and loopback behavior.

### 9.3 Timeouts

Timeout values in configuration are integer seconds. Defaults preserve Grok's
event classes:

- `SessionStart`, `PreToolUse`, `PreCompact`, and ordinary observer work: 5s
  per handler;
- `UserPromptSubmit`: 30s per handler;
- `PostToolUse` and `Stop`: 600s per handler;
- `SessionEnd`: 1500ms default for a handler with no explicit timeout.

An absent or zero configured timeout selects the event default. Other events
honor the configured positive integer seconds, matching Grok. `SessionEnd`
alone clamps an explicit timeout to 60s. The enclosing session-close path has
its own bounded total shutdown deadline. Cancellation of the enclosing session
or turn always wins over a handler timeout. Timeout is a failed handler, not an
implicit deny.

## 10. Canonical payload and result model

Every payload includes `schemaVersion`, canonical `event`, plugin generation,
session and turn identifiers where available, workspace identity, and a
bounded event-specific object. Payload construction redacts known secret
fields before serialization. The event-specific content is:

- `SessionStart`: session source, model/provider identity, and effective
  capability names;
- `SessionEnd`: reason and final status;
- `UserPromptSubmit`: submitted prompt text and input metadata;
- `PreToolUse`: immutable tool name and proposed arguments;
- `PostToolUse`: immutable tool name, final authorized arguments, success/error
  status, and bounded tool output;
- `Stop`: proposed stop reason and bounded last-assistant context;
- `PreCompact`: compaction trigger, source range metadata, and bounded summary
  inputs;
- `PostCompact`: resulting range metadata and bounded summary output.

Handler results retain Grok's compatible wire shape. Pre-tool gates use:

```json
{
  "hookSpecificOutput": {
    "permissionDecision": "allow",
    "permissionDecisionReason": "optional bounded text",
    "updatedInput": { "path": "safe.txt" },
    "additionalContext": "optional bounded text"
  }
}
```

`permissionDecision` accepts `allow`/`approve`, `ask`, `defer`, and
`deny`/`block`. The legacy top-level `decision` and `reason` form is also
accepted, but the nested decision takes precedence. An omitted decision is an
allow unless the hook explicitly says `defer`, matching Grok's parser.

`UserPromptSubmit`, `Stop`, and `PostToolUse` use the Grok top-level
`decision: "block"` plus `reason`. Stop additionally accepts
`continue: false`, `stopReason`, and nested `additionalContext`.
PostToolUse additionally accepts nested `additionalContext` and
`updatedToolOutput`. `updatedMCPToolOutput` is parsed for wire compatibility
but cannot apply before Phase 6C provides an MCP result type.

Reasons are clipped to 256 characters, hook-influenced feedback/context to
10,000 characters, output replacements to 64 KiB, and the serialized event
payload to 128 KiB. Unknown or event-inapplicable fields are ignored and
diagnosed. A malformed optional field is dropped without discarding an
otherwise valid deny/ask decision; any mutation from a response whose runner
health is broken is dropped.

## 11. Event semantics

### 11.1 Session lifecycle

`SessionStart` runs once after the initial plugin snapshot and effective
capabilities are frozen, before the first model request. It is an observer:
gate fields and mutations are ignored. A bounded `systemMessage` may be shown
as hook feedback, but it is not silently inserted as model instructions.
Failures are recorded and startup continues.

`SessionEnd` is best-effort and runs exactly once for every session that
reached `SessionStart`, including cancellation and startup failure after hook
dispatch began. Its result cannot revive, prolong, or change the final session
status. The bounded total shutdown budget prevents hook cleanup from wedging
process exit.

### 11.2 User prompts

`UserPromptSubmit` runs after input normalization and before the prompt is
committed to conversation history or sent to a provider. A healthy top-level
`decision: "block"` or command exit two rejects the prompt without appending
it. Every other healthy result accepts it. Pre-tool permission decisions,
argument rewrites, and context mutation are ignored for prompt hooks. Phase 6B
does not allow a hook to rewrite user prompt text.

### 11.3 Tool calls

`PreToolUse` runs after provider tool-name resolution and initial schema parse,
but before final policy evaluation, approval, sandbox planning, or execution.
The tool name is immutable. Handlers receive the latest rewritten arguments in
sequence:

```text
provider request
  -> resolve immutable tool name
  -> parse original arguments
  -> sequential PreToolUse handlers
  -> validate final arguments against tool schema
  -> recompute PolicyEngine decision
  -> recompute approval requirement and scope
  -> recompute sandbox plan
  -> execute
```

`updatedInput` replaces the current complete argument object; it is not a
partial merge. A later successful rewrite replaces an earlier rewrite and is
audited. No approval or policy decision made for the old arguments is reused.
A healthy `deny` immediately blocks the call and discards accumulated rewrites
and context. `ask` is remembered while later handlers continue, so a later
deny can still win and a later ask replaces an earlier ask. If no hook denies,
the final ask routes through the existing approval flow for the final rewritten
request. `allow` is advisory and cannot override policy, sandbox, missing
schema, or the child profile. `defer` makes no gate decision.

`PostToolUse` runs once after every attempted built-in tool execution, including
tool errors, with the final authorized arguments. It cannot convert a failed
execution into a successful side effect or undo an effect. Additional context
is supplied to the next model step. A valid nested `updatedToolOutput` replaces
only the model-visible tool result, then passes through Lato's existing output
truncation and journal redaction again. The last valid built-in replacement
wins. Original outcome metadata and hashes remain in audit. A healthy
top-level block becomes bounded blocking feedback for the model but does not
roll back the tool. Hook failure preserves the original output.

### 11.4 Stop

`Stop` runs when the agent would otherwise finish a turn. A healthy
`decision: "block"` and/or nested additional context requests another model
continuation. At most eight stop-hook continuations are permitted per turn,
matching Grok's safety cap. At the cap, Lato stops and records the cap event.
`continue: false` prevents continuation and its optional `stopReason` explains
why; it wins over accumulated block/context signals. Handler failure does not
prevent stopping.

### 11.5 Compaction

`PreCompact` runs after a compaction plan is selected but before history is
rewritten. `PostCompact` runs only after the compacted history has been durably
committed. Both are observer events in the pinned Grok baseline: decision,
context, and mutation fields are ignored, so neither can cancel compaction,
rewrite source ranges, alter token budgets, replace a summary, or roll back
history. Failure is recorded and compaction continues.

## 12. Decision combination and failure isolation

Handlers execute sequentially over the current event state. Each successful
response is applied immediately. The combination rules are:

- Stop handlers all run; `continue: false` prevents the resulting continuation;
- `deny` terminates a gate-capable event;
- only PreToolUse supports `ask`; handlers continue and the last ask wins unless
  a later deny terminates the gate;
- `allow` records an affirmative plugin opinion but still yields to Lato's
  authority checks;
- `defer` leaves the gate unchanged;
- contexts append in dispatch order until the aggregate 64 KiB event cap;
- the last valid rewrite or replacement wins, with every replacement audited.

A handler failure never prevents later handlers from running unless the
enclosing turn/session was cancelled. Failures include configuration exclusion,
spawn/connect errors, non-success exits other than the explicit exit-two gate,
timeouts, output overflow, invalid UTF-8 where text is required, malformed
JSON, and an unusable result document. They produce bounded diagnostics and
audit records, then default to no mutation. Invalid optional fields in an
otherwise usable Grok-compatible document are diagnosed and dropped
individually as described above.

Panics in extension-owned tasks are caught at the join boundary and treated as
failures. No hook mutex is held across process, network, approval, provider, or
tool awaits.

## 13. Audit and redaction

The canonical journal adds versioned records for:

- skill catalog materialized;
- skill invoked or rejected;
- hook dispatch started;
- hook completed with decision;
- hook failed or timed out;
- hook arguments rewritten;
- hook output replaced;
- stop continuation requested or capped;
- hook runtime generation adopted and retired.

Each record includes stable plugin/skill or handler identity, snapshot
generation, event, session/turn/tool-call identifiers where available,
duration, outcome category, configured/effective timeout, truncation flags,
and SHA-256 hashes of canonical input and output. The journal does not persist
raw prompts, full tool arguments, hook standard streams, full tool output,
environment values, authorization headers, or hook-returned context. Bounded
human-readable reasons are passed through the existing secret redactor before
storage.

Audit ordering follows lifecycle ordering: dispatch-start precedes handler
completion; a rewrite/replacement record follows the successful handler and
precedes the next handler; final policy/approval/tool journal records refer to
the hash of the final rewritten arguments. Failed handlers remain observable
without changing the effective event state.

## 14. Reload, session, and child semantics

At `SessionStart`, Lato materializes the skill catalog and hook descriptor set
from the session's Phase 6A snapshot. At each turn boundary it binds those
immutable values into `TurnConfig`. A `lato/plugins/reload` published during an
active turn is staged for the next turn. It cannot change the current prompt's
skill descriptions, hook chain, timeout, or decision sequence.

When a session adopts generation N+1, it builds a candidate skill catalog and
hook runtime first. Parse failures exclude only their component. Publication
to that session is atomic. Retired HTTP clients are dropped and retired
command-handler processes receive bounded shutdown after all turns still
referencing generation N release it. No new event is dispatched into a retired
generation.

A child session receives a derived frozen catalog from its parent's current
snapshot, intersected with the child's effective profile and policy-visible
capabilities. It never re-discovers plugins independently and cannot inherit a
skill or hook excluded from the parent. `allowed-tools` is intersected again
for the child. Hook payload capabilities describe the child's effective set,
not the parent's. Reload adoption by a parent does not mutate an already
running child turn; normal next-turn adoption rules apply independently.

## 15. Security invariants

Phase 6B must preserve all of these invariants:

1. An untrusted or disabled plugin contributes no skill text and executes no
   hook.
2. A snapshot generation is immutable for an entire turn.
3. Skill tool declarations can only remove capabilities.
4. Hooks cannot change a tool name or any authority-bearing session field.
5. Rewritten arguments receive fresh schema, policy, approval, and sandbox
   evaluation.
6. A hook `allow` cannot overrule Lato authority.
7. Child sessions receive no capability absent from both parent and child
   effective sets.
8. Command paths remain inside the plugin root and command processes are
   bounded and reaped.
9. HTTP destinations pass SSRF validation and redirects remain disabled.
10. Hook output is bounded before parsing and again before model exposure.
11. Audit records expose hashes and redacted reasons, not raw secrets or
    extension payloads.
12. Reload never mutates an issued catalog or handler chain.

## 16. Error presentation

User-visible errors are concise and actionable:

- explicit skill lookup errors name the requested identity and qualified
  alternatives when safe;
- blocked prompts and tools show the redacted hook reason and plugin identity;
- malformed plugin components appear in plugin reload/session diagnostics;
- fail-open hook failures do not interrupt normal interaction, but are visible
  through diagnostics and the journal;
- output and timeout messages state the bound that was exceeded without
  including captured secret-bearing content.

Repeated identical runtime failures from one handler are rate-limited in the
UI while every occurrence remains represented by a compact journal record.

## 17. Verification plan

### 17.1 Unit tests

- bounded YAML frontmatter parsing and prose-description fallback;
- deterministic skill naming, qualification, ambiguity, and collisions;
- containment checks, symlink escape rejection, and size limits;
- catalog rendering bounds and complete-entry truncation;
- `$ARGUMENTS`, `$1`-`$9`, `$$`, and expansion overflow;
- `allowed-tools` narrowing and unavailable-tool omission;
- hook event alias normalization and matcher grammar;
- deterministic handler ordering and decision combination;
- command exit-two deny, other exit failures, timeout, overflow, cancellation,
  process-tree reap, and restricted environment;
- HTTP scheme, DNS, redirect, SSRF, response, and timeout bounds;
- result validation, context aggregate bounds, and last-rewrite-wins behavior;
- audit hashing, redaction, ordering, and absence of raw payloads.

### 17.2 Agent integration tests

- `SessionStart/End` exactly-once behavior across success, startup failure, and
  cancellation;
- prompt block/allow/failure paths without accidental history mutation;
- PreToolUse rewrite followed by fresh schema/policy/approval/sandbox checks;
- immutable tool-name enforcement;
- PostToolUse on success and failure, replacement rebounding, and audit of the
  original outcome;
- Stop continuation, force stop, and the eight-continuation cap;
- PreCompact/PostCompact observer behavior at their exact commit boundaries;
- mid-turn reload isolation and next-turn adoption;
- retired-generation bounded cleanup;
- parent/child catalog and hook narrowing;
- untrusted and disabled plugins remaining inert.

### 17.3 Command-level smoke

An installed `lato` binary is exercised against a temporary trusted workspace
with:

- one enabled plugin skill whose description appears and whose body is
  explicitly invoked;
- one command hook that rewrites safe tool arguments;
- one loopback HTTPS hook with a test certificate, exercising Grok's production
  loopback allowance without weakening private-network validation;
- one timeout/failure hook proving fail-open isolation;
- a plugin reload proving the active turn stays on generation N and the next
  turn adopts N+1;
- a child session proving it cannot regain a parent-only tool through a skill
  or hook.

The release gate runs formatting, workspace tests, clippy with warnings denied,
the command-level smoke, and `cargo install --path .`. Documentation records
the final commands, counts, and any platform-specific unchanged retry.

## 18. Acceptance criteria

Phase 6B is complete when:

- trusted enabled plugin skills are discovered deterministically from the
  frozen plugin snapshot;
- compact descriptions are injected and full bodies appear only through
  bounded explicit invocation;
- skill tool declarations only narrow the effective catalog;
- all eight lifecycle events execute command and HTTP hooks with the specified
  ordering, timeouts, and isolation;
- prompt/tool gates, argument rewrite, output replacement, stop continuation,
  and compaction semantics match this specification;
- rewritten tools are fully revalidated and reauthorized;
- reload and child-session behavior preserve immutable snapshot and authority
  invariants;
- audit records are deterministic, bounded, and secret-safe;
- malformed, failed, timed-out, or malicious extensions cannot wedge the
  session or escape their authority boundary;
- the full verification and installed-command gate passes;
- derived-source attribution and user documentation are updated.

## 19. Follow-on boundary

Phase 6C may consume the same trusted snapshot and runtime lifecycle to start
stdio and streamable HTTP MCP servers. It must define its own server shutdown,
progressive `search_tool`/`use_tool` discovery, permission/approval/output
bounds, and parent/child inheritance rules. Nothing in Phase 6B implicitly
authorizes or starts MCP.
