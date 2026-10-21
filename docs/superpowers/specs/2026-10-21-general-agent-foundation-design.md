# General Agent Foundation

The accepted direction is a reusable Lato runtime with optional coding capabilities and business modules exposed as tools, MCP integrations, skills, and workflows. This first increment makes the existing host configurable and embeddable without changing its default coding behavior.

## Alternatives and decision

1. Recommended: reuse the ACP host and policy/tool runtime with an immutable application configuration and explicit tool catalog. This exercises the existing session and journal paths and keeps migration small.
2. Fork a separate business-agent loop. Easier initially, but duplicates cancellation, approvals, persistence, and provider handling.
3. Build a desktop workbench and HTTP service first. Useful later, but does not establish a reliable business execution boundary.

## First increment

- Public `AgentConfiguration`: coding/general mode, application instructions, optional canonical tool allowlist, and optional workspace instruction loading. General mode requires an explicit tool list; an empty list means no tools. Coding defaults remain compatible.
- Load an optional trusted `$LATO_HOME/agent.json` once at host construction. Invalid configuration fails session creation; never silently enable the default coding catalog.
- Public host configuration and business-tool registration APIs, usable only before creating sessions. Business tools use the same schema validation, approval, execution grant, and output handling as existing tools.
- Restrict the actual runtime catalog, including canonical-name resolution. Hidden tools cannot be invoked by a forged model call.
- General mode initially excludes built-in child-task, workflow, and AgentField delegation tools until application configuration inheritance is implemented. MCP gateways may be explicitly enabled, granting their whole configured remote surface; remote authorization remains the server's responsibility. Trusted plugin hooks retain their existing behavior and are not sandboxed by a tool allowlist.
- Application configuration is fingerprinted in the initial system message. Resume rejects a different fingerprint, including removal of the application configuration. No journal schema change. Existing coding sessions remain resumable under the legacy default.
- CLI interactive, headless, and ACP use the same host-level configuration. ACP session info exposes mode and selected tools without application instructions.

## Verification and delivery

Use deterministic model streams to verify the real host/model/tool round trip, sequential calls to two business modules, structured argument rejection, approval denial, unknown-tool rejection, configuration isolation, and persistence/resume compatibility. Add an executable business-tool embedding example and document configuration and limitations. Run focused tests, broader affected-crate regression checks, formatting and lint, then `cargo install --path .` and installed-binary smoke checks.

## Subsequent increments

Application-aware delegation and workflow integration; product-facing service API with authentication and tenant isolation; plugin packaging and discovery; task progress/artifact UI; real provider and representative business-task evaluations. These are follow-on work, not completion claims for this increment.
