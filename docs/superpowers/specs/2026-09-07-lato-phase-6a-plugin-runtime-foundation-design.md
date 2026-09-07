# Lato Phase 6A: Plugin Runtime Foundation Design

Date: 2026-09-07

Status: approved

## 1. Purpose

Phase 6A ports the Grok Build plugin runtime foundation into Lato. It adds
manifest loading, deterministic multi-source discovery, project-plugin trust,
immutable per-session snapshots, live reload, and capability narrowing. It
does not execute plugin hooks or MCP servers and does not yet inject skill
instructions. Those consumers arrive in Phase 6B and Phase 6C over the stable
snapshot contract defined here.

Before Phase 6A production changes, a short Phase 5 release gate freezes the
merged Phase 5B/5C subagent behavior as a command-level baseline. The gate adds
no product capability.

## 2. Upstream baseline and porting policy

The behavioral baseline is Grok Build commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138`, especially:

- `xai-grok-agent/src/plugins/manifest.rs`;
- `xai-grok-agent/src/plugins/discovery.rs`;
- `xai-grok-agent/src/plugins/trust.rs`;
- `xai-grok-agent/src/plugins/registry.rs`;
- `xai-grok-shell/src/session/acp_session_impl/hooks_plugins.rs`;
- `xai-grok-shell/src/extensions/session_admin.rs`;
- the associated manifest, discovery, trust, registry, reload, and folder-trust
  tests.

Lato will structurally port these modules into a new `lato-extensions` crate.
Substantially derived production files must retain source and license headers
and be listed in `docs/superpowers/reference/lato-upstream-sources.md`.

Only thin adapters may differ from upstream behavior:

- Lato's existing `SessionTrust` provides the project folder-trust verdict.
- Lato's `PolicyEngine`, sandbox, tool runtime, and subagent profiles remain the
  final capability authority.
- Lato's session actor and immutable turn configuration determine when a live
  session adopts a newly published snapshot.
- Grok-specific marketplace, install registry, LSP, telemetry, branding,
  Claude-compatibility paths, and plugin data directories are excluded.

## 3. Scope

Phase 6A delivers:

- canonical `plugin.json` parsing;
- convention-based discovery when a manifest is absent;
- CLI, project, and user plugin sources;
- canonical-path deduplication and deterministic name-conflict resolution;
- project plugin gating through folder trust;
- immutable, generation-stamped plugin snapshots;
- per-session and per-turn snapshot binding;
- the `lato/plugins/reload` protocol operation;
- atomic rebuild, publication, and fan-out to live sessions;
- parent-to-child snapshot derivation with capability narrowing;
- bounded diagnostics and canonical audit events;
- tests and documentation for the above.

Phase 6A does not deliver:

- plugin install, uninstall, update, marketplace, or remote download;
- plugin data directories;
- LSP integration;
- `SKILL.md` prompt injection or explicit skill invocation;
- hook execution;
- MCP server startup or tool invocation;
- plugin-defined subagent profiles;
- automatic approval of project plugins;
- dynamic Rust libraries or a stable Rust plugin ABI.

## 4. Architecture

The plugin runtime is an extension catalog, not an alternate execution path:

```text
CLI paths + project directory + user directory
                      |
              PluginDiscovery
                      |
       manifest parse / canonicalize / dedupe
                      |
            trust + enablement + conflicts
                      |
               PluginRegistryBuilder
                      |
            Arc<PluginSnapshot> generation N
                      |
          +-----------+------------+
          |                        |
   RuntimeSession             child derivation
          |                        |
 immutable TurnConfig       parent cap intersection
          |                        |
   future Skills/Hooks/MCP consumers
```

The new `lato-extensions` crate owns plugin-domain parsing and snapshot
construction. It may depend on stable core policy and identity types, but it
must not depend on CLI/TUI presentation, provider implementations, or session
actor internals.

`lato-agent` owns session adoption and reload orchestration. `lato-protocol`
owns the versioned `lato/plugins/reload` request and response shape. The root
CLI/ACP layers route the operation but do not perform discovery themselves.

## 5. Plugin manifest

The canonical manifest is `plugin.json` at the plugin root. When it is absent,
a directory is a convention plugin only if it contains at least one recognized
component:

- `skills/`;
- `hooks/hooks.json`;
- `.mcp.json`.

The convention plugin name is derived from the directory name. Names must be
1-64 characters of lowercase ASCII letters, digits, and hyphens, without a
leading or trailing hyphen.

The initial manifest accepts these forward-compatible fields:

```json
{
  "name": "example-plugin",
  "version": "1.2.3",
  "description": "Example extension",
  "author": { "name": "Example" },
  "homepage": "https://example.invalid",
  "repository": "https://example.invalid/repo",
  "license": "Apache-2.0",
  "keywords": ["example"],
  "skills": ["skills"],
  "hooks": "hooks/hooks.json",
  "mcpServers": ".mcp.json"
}
```

Unknown fields are ignored for forward compatibility. Component paths may be
a string or a list where applicable. Every resolved path is canonicalized and
must remain beneath the canonical plugin root after resolving `..` and
symlinks. A path that cannot be proven contained is excluded and diagnosed.

Inline hook and MCP objects may be preserved as inert manifest metadata for
future compatibility, but Phase 6A does not execute them.

## 6. Discovery sources and precedence

Phase 6A scans sources in this priority order:

1. every CLI `--plugin-dir <path>` root, scope `CliOverride`;
2. `.lato/plugins/*/` from the workspace root, scope `Project`;
3. `$LATO_HOME/plugins/*/`, scope `User`.

CLI paths name a plugin root directly. Project and user locations are parent
directories whose immediate child directories are candidates. Recursive
plugin nesting is not inferred.

Every candidate root is canonicalized. The first occurrence of a canonical
root wins, so aliases and symlinks cannot load one plugin twice. Candidate
enumeration is sorted by canonical path for reproducibility.

For duplicate plugin names, the lower-numbered scope above wins. Within one
scope, the first canonical path wins. Losing candidates are absent from the
effective registry, while the winner retains a bounded conflict diagnostic
that names the losing scope and path.

## 7. Trust and enablement

Trust follows the selected Grok Build behavior:

- CLI override plugins are trusted because specifying the path is an explicit
  process invocation choice.
- User plugins under `$LATO_HOME/plugins` are trusted.
- Project plugins are trusted only when the current workspace folder-trust
  verdict allows project extensions.

Untrusted project plugins remain discoverable for diagnostics but never enter
the active set. They cannot contribute skills, hooks, MCP servers, profiles,
or executable configuration. A pre-populated enabled setting cannot bypass
the trust gate.

Phase 6A records enabled state in the registry so future management commands
have a stable location. CLI overrides are active by default. Project and user
plugins follow persisted enabled/disabled configuration when present; newly
discovered project and user plugins default to disabled, matching Grok Build's
safe activation behavior. Trust and enablement are independent: a plugin must
be both trusted and enabled to be active.

Plugin trust is not execution authority. A component exposed by an active
plugin must still pass the normal `PolicyEngine`, approval, sandbox, session
capability, and subagent profile checks when Phase 6B or 6C consumes it.

## 8. Immutable snapshots

`PluginSnapshot` is an immutable value behind `Arc` and contains:

- a monotonic process-local generation;
- build timestamp;
- workspace identity and project-trust verdict;
- normalized discovery inputs;
- all surviving discovered plugins;
- the active plugin subset;
- bounded load and conflict diagnostics;
- the session-only CLI plugin roots used to build it.

A session receives a snapshot at startup. At the start of each turn, Lato
copies the current snapshot `Arc` into immutable turn configuration. Every
operation in that turn reads the same generation even if a reload completes
elsewhere.

Snapshots contain resolved descriptors and component references, not live
filesystem watchers or mutable executors. Future hook and MCP runtimes own
their resources separately and reconcile them when a session adopts a new
snapshot.

## 9. Reload semantics

`lato/plugins/reload` follows Grok Build's explicit reload behavior:

1. Resolve the current folder-trust verdict.
2. Re-read plugin configuration and all configured discovery sources.
3. Force a full rebuild instead of an unchanged-input shortcut.
4. Parse, canonicalize, deduplicate, resolve conflicts, and apply trust and
   enablement into a candidate snapshot.
5. Publish the candidate as the next generation only after the global build
   succeeds.
6. Broadcast the published snapshot to all live sessions.
7. Return the generation, discovered count, active count, and diagnostics.

An idle session adopts the new snapshot as soon as it handles the ordered
reload notification. A session with an active turn retains that turn's frozen
snapshot and stages the new generation for the next turn. Reload never mutates
an already-issued snapshot.

Per-session CLI plugin directories are not leaked into the shared process
snapshot. When a shared reload is fanned out, each session rebuilds or merges
its own CLI override roots before adopting the generation, matching Grok
Build's session-specific plugin directory preservation.

If the registry-wide build fails before publication, the previous shared
generation remains authoritative. A malformed individual plugin is isolated,
excluded, and reported without blocking other valid plugins.

## 10. Policy and subagent narrowing

Phase 6A exposes a pure derivation operation that future component runtimes use:

```text
active plugin capabilities
intersection session grant
intersection PolicyEngine-visible capabilities
intersection subagent profile allowlist
intersection workspace allowance
```

The result can only remove plugins or components. It cannot mark an untrusted
plugin trusted, enable a disabled plugin, add a component absent from the
parent snapshot, or introduce a capability absent from the parent grant.

Child sessions receive an independently derived immutable snapshot at child
creation. A parent reload does not alter a running child snapshot or grant new
capabilities. A later child created after the parent adopts a new generation
may inherit that generation, subject to the same intersections.

## 11. Events, diagnostics, and errors

Phase 6A adds canonical, bounded audit events for:

- snapshot built and published;
- snapshot adoption by a session;
- reload requested, completed, or failed;
- plugin rejected for manifest, path, trust, enablement, or conflict reasons;
- child snapshot derived with capability removals.

Events include plugin ID, scope, generation, stable reason code, and sanitized
paths where relevant. They do not include secret environment values, MCP
headers, credentials, or arbitrary plugin file contents.

Manifest parse errors, invalid names, unreadable roots, escaping paths, broken
symlinks, and duplicate paths are isolated to their candidate. Registry
publication errors preserve the last known-good generation. Diagnostics have
per-plugin and per-snapshot count and byte ceilings so a hostile directory
cannot grow session state without bound.

## 12. Phase 5 release gate

The gate is measured against the merged Phase 5B/5C commit before Phase 6A
production changes. It uses a fresh temporary `LATO_HOME` and must not consume
live provider credentials.

The gate includes:

1. formatting and the focused Phase 5B/5C runtime, agent, workspace, tool, ACP,
   and presentation suites;
2. the full workspace test suite when practical;
3. `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
4. source and installed `lato --version` checks;
5. source and installed `lato doctor --json`, with JSON schema validation;
6. source and installed offline headless prompt, expecting exactly `hi`;
7. journal creation and parse validation under the fresh home;
8. a real command-level subagent chain that exercises `spawn`, `inspect`, and
   `wait`, verifies isolated worker workspace behavior, and observes a terminal
   child result;
9. cancellation and bounded root shutdown coverage, including shell process
   tree termination;
10. verification that `spawn_subagent` is absent and `spawn`, `send`, `wait`,
    `cancel`, and `inspect` are model-visible;
11. `cargo install --path .` after the tests pass;
12. README and acceptance-result updates with the exact commit, commands,
    counts, skipped checks, and residual limitations.

Any failed test, unauthorized side effect, leaked process, workspace escape,
or unverifiable command-level subagent result blocks the Phase 5 baseline.
Unavailable live-provider or optional cross-platform checks are recorded as
skipped or unavailable and are never reported as passing.

## 13. Testing strategy

### 13.1 Manifest tests

- canonical manifest parsing and unknown-field tolerance;
- convention fallback and directory-name validation;
- single and multiple component paths;
- `..`, absolute-path, symlink, missing-path, and canonicalization failure
  handling;
- inline hook/MCP metadata remains inert.

### 13.2 Discovery and registry tests

- CLI, project, and user source discovery;
- source precedence and deterministic same-scope ordering;
- canonical-path and symlink deduplication;
- name conflict winner and diagnostics;
- trusted/enablement matrix;
- pre-enabled untrusted project plugin exclusion;
- stable plugin IDs and snapshot contents.

### 13.3 Reload and session tests

- successful rebuild increments generation exactly once;
- malformed candidate isolation;
- registry-wide failure preserves the previous generation;
- fan-out reaches every live session;
- session-specific CLI roots survive shared reload without entering other
  sessions;
- active turn retains its snapshot while the next turn sees the new one;
- removed plugins disappear only after snapshot adoption;
- concurrent reload requests serialize without generation rollback.

### 13.4 Policy and child tests

- session capability intersection only narrows;
- read-only profiles cannot inherit executable plugin components;
- untrusted and disabled plugins never reappear during derivation;
- existing children retain their generation after parent reload;
- new children receive the parent's currently adopted generation;
- sibling sessions cannot inherit each other's CLI override roots.

## 14. Documentation and compatibility

README gains the Phase 5 measured gate and a concise Phase 6A plugin section
covering source locations, manifest shape, trust, enablement, reload, and the
fact that Phase 6A does not execute skills, hooks, or MCP.

The acceptance record gains the exact Phase 5 baseline evidence. The upstream
source ledger records every structurally ported Grok Build file.

No existing CLI, TUI, headless, ACP, journal, tool, or subagent output may
change except for the new opt-in plugin arguments and reload protocol method.
Stored plugin snapshot and event shapes are schema-versioned before they are
treated as durable compatibility contracts.

## 15. Acceptance criteria

Phase 6A is complete when:

1. The Phase 5 release gate is recorded and green for all mandatory checks.
2. The installed `lato` binary reflects the gated commit before Phase 6A work.
3. All three plugin sources discover deterministically with Grok Build
   precedence.
4. Manifest and convention discovery reject escaping component paths.
5. Untrusted project plugins never enter the active registry.
6. Session and turn snapshots remain immutable and generation-consistent.
7. `lato/plugins/reload` atomically rebuilds and fans out a new snapshot while
   preserving live turns and session-specific CLI roots.
8. Policy and child derivation can only narrow plugin capabilities.
9. Hooks, skills, and MCP remain inert consumers in this phase.
10. Focused tests, workspace tests, formatting, and Clippy pass.
11. Source attribution, README, and acceptance documentation are updated.
12. `cargo install --path .` succeeds and the installed binary passes the
    offline command smoke.

## 16. Phase 6B and 6C interface boundary

Phase 6B consumes only trusted, enabled skill and hook descriptors from a
session snapshot. It adds `SKILL.md` discovery and injection plus
`SessionStart/End`, `UserPromptSubmit`, `PreToolUse/PostToolUse`, `Stop`, and
`PreCompact/PostCompact` execution with timeouts, argument rewrite validation,
failure isolation, and audit records.

Phase 6C consumes only trusted, enabled MCP descriptors from the same snapshot.
It owns stdio and streamable HTTP transports, bounded lifecycle and shutdown,
progressive `search_tool`/`use_tool` discovery, policy and approval integration,
output truncation, and parent-child MCP capability narrowing.

Neither phase may bypass the Phase 6A trust, enablement, generation, or
capability-intersection contracts.
