# Business Workflow integration

The previous turn made verified progress: HTTP/SSE integration, actual business-module/MCP execution, approvals and local deployment are complete. The current objective still requires Workflow to preserve application identity and authority.

Implement immutable delegation bindings shared by a host's task runner and Workflow managers. Application children receive the parent's instructions and registered modules, intersect the parent's exact tool catalog with child capabilities, and retain inherited filesystem restrictions. General-mode Workflow roles use domain-neutral instructions/results instead of coding-only output fields. Existing coding profiles remain compatible.

- [x] Bind application configuration/modules to child runners and Workflow managers; enable the `workflow` tool in custom configurations only after this path exists.
- [x] Forward delegated tool approvals and progress to the owning session without rewriting the exact policy fingerprint. Keep per-call approval, cancellation, budgets and output-schema checks.
- [x] Bind persisted Workflow records to application identity; incompatible records cannot resume under another application or a legacy manager.
- [x] Expose Workflow discovery/status/pause/resume/stop through HTTP; natural-language starts still traverse the ordinary Workflow tool policy/revision gate.
- [x] Exercise real Workflow business chains, denied/forged child calls, child approvals, schema contracts, narrowed roles, cancellation and persistence. Add a runnable business Workflow example.
- [x] Run appropriate regression checks, build before running shell-hook tests, install using the tested lockfile, and verify the installed command.

Scope: built-in generic task spawning and AgentField remote delegation remain separate extensions. Trusted Workflow scripts retain their existing bounded scratch/template host functions; model workers only receive inherited tools. This increment must not enable a broad coding catalog merely to make business workers pass existing tests.

Implementation decisions: general workers use the inherited shared workspace; explicit coding worktree isolation remains coding-only. Business `read-write` workers include selected external modules, while read-only roles omit generic mutation-capable invocation brokers. Workflow results are retained with an explicit 256 KiB truncation notice. Resume now checks the same active-run and budget ceilings as launch, and shutdown closes admission before draining children.

Goal acceptance: application configuration and exact tool scope; SDK/MCP business modules; Skill invocation; application-aware bounded Workflow orchestration; authenticated asynchronous HTTP/SSE entry point; exact approvals and cancellation; durable identity checks; runnable synthetic examples; tested local deployment. Multi-tenant gateways, desktop workbench UI, and representative live-model business evaluations are separate product work.

Validation completed: 1,587 tests passed across 133 suites, strict Clippy and formatting passed. Installed with `cargo install --path . --locked`. Both installed-binary business-module and two-agent Workflow HTTP/MCP smoke checks passed. See `docs/testing/reports/2026-10-21-business-workflows.md`.
