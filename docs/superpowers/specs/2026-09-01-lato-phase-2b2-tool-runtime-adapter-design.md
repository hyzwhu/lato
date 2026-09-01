# Lato Phase 2B-2 Tool Runtime Adapter Design

**Date:** 2026-09-01

**Status:** Design sections approved; written-spec review pending

**Scope:** Make `ToolCatalog` the authoritative model-definition and execution boundary for all existing built-in tools while retaining the legacy dispatcher behind per-tool adapters temporarily.

## Context

Phase 2A introduced the provider-independent `Tool`, `ToolCatalog`, `ToolContext`, `ToolOutput`, and `ToolError` contracts in `lato-core` and `lato-tools`. Production execution still bypasses those contracts: `SessionActor` obtains model-facing JSON from `v1_tool_definitions()` and calls a string-matched `dispatch()` function directly. This duplicates tool identity in two paths and requires actor edits whenever a tool is added.

Phase 2B-2 establishes one real tool membrane without rewriting the nine existing tool implementations at once. Each built-in tool becomes an independently registered `Tool`; its first implementation delegates to the existing dispatcher so approval counters, sandboxing, file locks, aliases, command behavior, and output behavior remain compatible. The adapter can later be replaced tool by tool without changing the actor or model protocol.

## Goals

- Make `ToolCatalog` the sole source for both model-visible definitions and executable tool resolution.
- Route every existing built-in tool call through `dyn Tool` and a typed `ToolContext`.
- Allow a new or overriding tool to be registered without modifying `SessionActor` or a string `match`.
- Preserve the existing tool schemas, model-facing names, approval flow, sandbox policy, file locking, history format, events, and bounded output behavior.
- Normalize legacy wire aliases at one boundary instead of leaking them into core identities.
- Return stable, typed tool failures internally while preserving compatible text at the actor/model boundary.
- Introduce meaningful session, turn, call, and cancellation context for later observability and remote execution.
- Preserve all pre-existing uncommitted user changes through isolated patches and explicit diff verification.

## Non-goals

- Rewriting all built-in tools as native `Tool` implementations in this phase.
- Removing `dispatch.rs` or `registry.rs` immediately.
- Changing model-facing tool names or JSON schemas.
- Changing CLI flags, configuration files, history serialization, ACP messages, or user-visible result formatting.
- Introducing automatic retries for tools.
- Implementing hard mid-system-call cancellation for every legacy operation.
- Adding an AgentField reasoner graph. This phase is deterministic Rust routing and contract adaptation; AgentField remains a future remote agent/tool runtime integration point.

## Alternatives Considered

### A. Catalog-authoritative per-tool compatibility adapters — selected

Register an independent adapter for each built-in tool. `ToolRuntime` generates model definitions from the catalog, normalizes wire names, resolves the canonical entry, and invokes `Tool::invoke`. The adapter delegates to the existing dispatcher.

This is the smallest reversible change that makes the new abstraction real. It preserves proven behavior and permits gradual native replacement.

### B. Rewrite every built-in tool natively now

Move all nine implementations, policy checks, locks, and output behavior directly into separate `Tool` implementations. This is the cleanest final form but combines too many behavioral changes before public beta and overlaps heavily with user-modified files.

### C. Register one monolithic dispatch router

Wrap the existing dispatcher as a single `Tool` that routes all names internally. This minimizes code movement but does not provide independent tool identity, replacement, metadata, authorization, or observability. It would preserve the architectural problem rather than solve it.

## Architecture

```text
Model tool call
    │ wire name + JSON arguments
    ▼
SessionActor
    │ repetition check, interactive approval, ToolContext
    ▼
ToolRuntime
    │ normalize wire name, resolve canonical name
    ▼
ToolCatalog
    │ Arc<dyn Tool>
    ▼
LegacyDispatchTool (one registered instance per built-in tool)
    │ existing ToolCall
    ▼
dispatch + SessionTrust + sandbox + FileLocks
    │ raw result
    ▼
ToolOutput / ToolError
    │ existing actor output bounding, events, history
    ▼
Model tool result
```

`SessionActor` no longer imports or calls `dispatch()` or `v1_tool_definitions()`. It owns or receives an `Arc<ToolRuntime>`, asks that runtime for the model definitions on every sampling request, and sends approved calls through the runtime.

## Canonical Identity and Wire Compatibility

Internal built-in names use the `builtin` namespace:

| Model-facing name | Canonical name | Initial implementation |
|---|---|---|
| `read_file` | `builtin:read_file` | legacy dispatcher adapter |
| `list_dir` | `builtin:list_dir` | legacy dispatcher adapter |
| `grep` | `builtin:grep` | legacy dispatcher adapter |
| `write_file` | `builtin:write_file` | legacy dispatcher adapter |
| `search_replace` | `builtin:search_replace` | legacy dispatcher adapter |
| `run_terminal_command` | `builtin:run_terminal_command` | legacy dispatcher adapter |
| `web_fetch` | `builtin:web_fetch` | legacy dispatcher adapter |
| `spawn_subagent` | `builtin:spawn_subagent` | legacy dispatcher adapter |
| `todo_write` | `builtin:todo_write` | legacy dispatcher adapter |

The wire-name normalizer accepts the current unqualified names and the `Lato:<name>` compatibility prefix. The legacy `write` alias maps explicitly to `builtin:write_file`. Unknown or malformed names fail closed with `tool.not_found`; they never fall through to arbitrary dispatcher strings.

Descriptors store canonical names. Model-definition generation removes the `builtin:` namespace and emits the existing model-facing names and schemas. Aliases are accepted for invocation but are not advertised to the model.

## Components

### `ToolRuntime`

`ToolRuntime` is the only actor-facing tool service. It owns an immutable `ToolCatalog` and provides:

```rust
pub struct ToolRuntime {
    catalog: ToolCatalog,
}

impl ToolRuntime {
    pub fn model_definitions(&self) -> Vec<serde_json::Value>;

    pub async fn invoke(
        &self,
        context: ToolContext,
        wire_name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError>;
}
```

`model_definitions` is derived from resolved catalog descriptors, not from a parallel registry. `invoke` normalizes a wire name, resolves the catalog entry, performs the pre-call cancellation check, and invokes the selected `Tool`.

The runtime is immutable after construction. This makes a session's advertised and executable tool set stable and avoids synchronization around mid-turn registration.

### `ToolRuntimeBuilder`

The builder assembles a runtime in explicit layers:

```text
built-in tools
    < product extensions
    < project extensions
    < session extensions
```

Later layers may replace the same canonical name according to existing `ToolCatalog` replacement semantics. Registration validates each descriptor and rejects invalid or ambiguous entries before the session begins. A normal caller can build the default built-in runtime; hosts that need extensions can register `Arc<dyn Tool>` values and pass the resulting runtime to the actor without editing actor code.

### `BuiltinToolEnvironment`

The compatibility adapters share an immutable execution environment:

```rust
pub struct BuiltinToolEnvironment {
    pub cwd: PathBuf,
    pub locks: Arc<FileLocks>,
    pub trust: SessionTrust,
}
```

`SessionTrust::clone` shares the underlying one-shot approval counter, so `allow_once` remains exactly-once across the adapter boundary. The environment is bound once when the session runtime is built.

### `LegacyDispatchTool`

Each adapter instance owns one validated descriptor, its canonical identity, its legacy dispatcher name, and the shared environment. It implements `Tool` independently even though the first implementation delegates to the common dispatcher.

Before dispatch it verifies cancellation and that its bound legacy name matches the descriptor. It then constructs the existing `ToolCall`, invokes `dispatch`, and converts the result to `ToolOutput`. It does not repeat interactive approval or actor output bounding.

This adapter is temporary by implementation, not by contract. A native `ReadFileTool` can later replace `builtin:read_file` in the catalog with no actor or wire-protocol change.

## Actor Integration and Lifecycle

A session receives a stable `session_id`. Each accepted user sampling cycle receives a monotonically unique `turn_id`, and every tool invocation uses the model-provided call ID when valid. Missing or unusable call IDs receive a locally generated typed ID rather than an empty identifier.

The actor constructs:

```text
ToolContext {
    session_id,
    turn_id,
    call_id,
    cancellation,
}
```

The lifecycle is:

1. create the session and default or injected runtime;
2. accept a prompt and allocate its turn identity;
3. request model definitions from the same runtime that will execute calls;
4. receive a tool call and perform repetition detection;
5. persist the call and run the current interactive approval flow;
6. construct `ToolContext` and invoke `ToolRuntime`;
7. convert success or typed failure to the existing bounded textual result;
8. emit current events and persist the result;
9. stop launching calls when cancellation is observed.

Actor ownership of repetition checks, interactive approval, history, events, and output bounding remains unchanged. Tool ownership of non-bypassable execution policy, locks, and sandbox constraints also remains unchanged. This preserves a deliberate two-layer policy boundary.

The existing constructor continues to build the default runtime for compatibility. A second constructor or factory accepts a prebuilt runtime for product, project, and session extensions. Both constructors converge on one initialization path.

## Descriptor Metadata

Existing descriptions and JSON schemas remain byte-for-byte or structurally equivalent at the model boundary. New descriptor metadata is conservative and explicit:

- source is `Builtin` and version is `1.0.0`;
- file readers declare workspace-read capability and no mutation;
- file writers declare workspace-write capability and resource-keyed execution where the descriptor supports it;
- terminal execution declares process capability, external mutation potential, serial execution, and non-idempotence;
- web fetch declares network capability and workspace-read-equivalent side effects only when no local mutation occurs;
- subagent spawning declares task capability and non-idempotence;
- todo updates declare task capability and the actual mutation semantics of the existing implementation;
- every descriptor has finite nonzero timeout and output limits.

Metadata must not claim safety, parallelism, or idempotence that the legacy implementation cannot prove. The initial catalog is validated by tests before it is exposed to the actor.

## Error Model

The runtime uses stable internal error codes:

| Code | Meaning | Default retryability |
|---|---|---|
| `tool.not_found` | unknown or unavailable tool name | never |
| `tool.invalid_arguments` | malformed or missing arguments | never |
| `tool.policy_denied` | approval, trust, sandbox, or policy rejection | never |
| `tool.cancelled` | request cancelled before or during useful work | never |
| `tool.execution_failed` | legacy implementation failed | never |
| `tool.timeout` | declared execution deadline exceeded | tool policy |

Known policy and argument failures are classified without requiring actor-side string parsing. Legacy errors that cannot yet be classified safely become `tool.execution_failed` and retain their diagnostic message.

At the current actor boundary, a `ToolError` is rendered into the existing `ERROR: ...` textual result so model behavior, history, and UI output remain compatible. Structured codes and retryability remain available for logging and later policy logic.

## Cancellation and Task Ownership

The runtime rejects a pre-cancelled context before resolution or dispatch. The adapter checks again immediately before starting legacy work. Once cancellation is observed, the actor must not start subsequent calls in the same cancelled turn.

Legacy operations that already support cooperative cancellation receive the context token when the existing API permits it. Operations without a cancellation parameter may finish their current blocking section, but their result is discarded when the context is cancelled. No detached retry or follow-up task is introduced.

True propagation into terminal processes, network requests, and spawned subagents is a later native-tool responsibility. This phase guarantees cancellation at the tool membrane without falsely claiming that every operating-system action can already be interrupted.

## Output Semantics

The adapter returns the raw successful content in `ToolOutput`, with empty metadata unless the legacy implementation exposes structured data. It does not duplicate truncation or artifact spilling.

The actor remains responsible for the existing `bound_tool_output` behavior, tool-result events, and history persistence. Therefore visible output size, spill files, and result formatting do not change in this phase.

## File Change Boundaries

Expected new files in `crates/lato-tools`:

- `src/runtime.rs` for `ToolRuntime` and name normalization;
- `src/builder.rs` for layered construction;
- `src/builtin_adapter.rs` for built-in descriptors and compatibility tools;
- integration tests for catalog/runtime behavior.

Expected narrow changes:

- `crates/lato-tools/src/lib.rs` exports the new API;
- `crates/lato-agent/src/actor.rs` stores or receives the runtime, derives model definitions from it, creates `ToolContext`, and invokes it.

`dispatch.rs` and `registry.rs` remain the compatibility behavior and schema references in this phase and should not be behaviorally rewritten. Because `actor.rs`, `dispatch.rs`, and `registry.rs` contain pre-existing user modifications, implementation must capture baseline patches and hashes. Only intentional actor integration hunks may be staged; unrelated user changes remain unstaged and intact.

## Testing

### Catalog and definition tests

- all nine canonical built-ins register exactly once;
- the advertised tool-name set equals the executable built-in set;
- advertised JSON schemas remain structurally equivalent to current definitions;
- aliases resolve correctly but are not advertised;
- invalid descriptors and duplicate entries fail during build according to layer policy;
- a higher layer can replace a lower-layer tool deterministically.

### Invocation tests

- every built-in traverses `ToolRuntime`, `ToolCatalog`, and `Tool::invoke`;
- `read_file`, `Lato:read_file`, `write`, and canonical internal resolution behave as specified;
- unknown names produce `tool.not_found`;
- malformed arguments produce `tool.invalid_arguments` when classification is known;
- a pre-cancelled context performs no legacy dispatch;
- custom tools can be registered and invoked without actor modification;
- shared `SessionTrust` consumes one-shot approval exactly once;
- deny-write, sandbox, and file-lock behavior remain enforced.

### Actor and compatibility tests

- production `SessionActor` contains no direct call to `dispatch()`;
- production `SessionActor` contains no direct call to `v1_tool_definitions()`;
- existing offline headless tool loops still complete;
- tool-call history, result events, output truncation, and spill behavior remain unchanged;
- CLI and ACP session construction both receive the default runtime unless a runtime is explicitly injected;
- cancellation prevents subsequent tool execution in the turn.

### Repository gates

- focused `lato-core`, `lato-tools`, and `lato-agent` tests;
- full workspace tests;
- `cargo fmt --check`;
- strict `cargo clippy --workspace --all-targets -- -D warnings`;
- `git diff --check`;
- source and dependency checks that keep `lato-core` independent of provider, CLI, and tool implementation crates;
- `cargo install --path .` as required by repository instructions;
- installed `lato` help and real headless tool-call smoke tests.

## Compatibility and Rollback

- Model-facing names, descriptions, schemas, arguments, and result text stay compatible.
- CLI, ACP, configuration, history, and event formats do not change.
- Existing dispatch, sandbox, trust, lock, and output code remains the behavioral authority.
- The old registry may remain temporarily for non-production compatibility tests, but actor production traffic cannot use it after this phase.
- The migration is reversible by restoring the two isolated actor call sites while leaving the new runtime unused.
- Native tool implementations can replace adapters individually through catalog registration, without another actor migration.

## Source Reuse and Provenance

The implementation may copy or structurally derive compatible code from the locally pinned Codex and Grok Build repositories, with license and provenance recorded where required. The user has explicitly authorized direct reuse when it accelerates a correct implementation.

Relevant upstream design points include:

- Grok Build's finalized toolset and dispatch preparation: resolve a client name to a registry entry, canonicalize parameters, build an execution context, and dispatch through an object-safe tool handle;
- Grok Build's stable tool identity, model description, capabilities, and JSON execution contract;
- Codex's tool registries and namespaced custom-tool routing, including preservation of model call identity across execution.

Pinned upstream commits remain:

- Codex: `633ab199cfd724aa78013c006b27a2b3d049fc3b`
- Grok Build: `bb7f39d5858cbf5e00de639367f59debbdcb0138`

## AgentField Assessment

AgentField live contract `2026-03-24-v1` and the local control plane were checked during design. Phase 2B-2 contains deterministic schema projection, name resolution, policy preservation, and tool dispatch. It does not benefit from a model-backed multi-reasoner call graph, and no provider key is required for implementation or offline verification.

The typed session, turn, call, cancellation, capability, and error boundaries intentionally leave room for a later AgentField-backed remote tool or agent implementation. Such an implementation will register behind `Tool` rather than introduce AgentField concepts into `SessionActor`.

## Completion Criteria

- `ToolCatalog` is the production source for both advertised and executable tools.
- All nine existing built-ins execute through independent `dyn Tool` registrations.
- New and overriding tools can be registered without actor code changes.
- Actor production code directly calls neither `dispatch()` nor `v1_tool_definitions()`.
- Typed identity, error, and cancellation context crosses the tool membrane.
- Existing approval, trust, sandbox, lock, history, event, and output behavior passes compatibility tests.
- Focused tests, full workspace tests, formatting, strict Clippy, and diff checks pass.
- The locally installed `lato` command passes help and real tool-use smoke tests.
- All pre-existing user modifications remain intact and unstaged except for explicitly reviewed integration hunks.
