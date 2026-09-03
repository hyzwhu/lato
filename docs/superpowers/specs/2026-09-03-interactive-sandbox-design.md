# Interactive sandbox selection

The user approved explicit interactive sandbox control after reporting that a
trusted Lato session could list its parent but could not create `../abc`.

## References and decision

Checked on 2026-09-03:
- Codex separates sandbox scope from approval policy, including interactive CLI
  overrides: https://learn.chatgpt.com/docs/agent-approvals-security
- Grok Build also separates these controls and exposes `--sandbox`:
  https://docs.x.ai/build/features/sandbox and
  https://docs.x.ai/build/features/permissions
- The locally installed `codex --help` confirms sandbox and approval flags are
  independent, and supports additional writable directories.

Use startup selection with Lato's existing off/workspace/read-only profiles.
Alternatives are per-call elevation (requires new grant and retry semantics), or
additional writable directories (requires extending the cross-platform policy
validator and sandbox backends). Neither is needed for this bounded change.

## Behavior

- `lato --sandbox off|workspace|read-only` starts an interactive session.
- `lato resume ID --sandbox PROFILE` accepts the flag before or after `resume`.
- Other subcommands reject the option; headless behavior stays compatible.
- Without the flag, a bilingual startup picker defaults to workspace and offers
  read-only and off. Off explicitly states that writes outside the workspace are
  possible. Esc cancels startup. Selection applies only to this invocation.
- Folder trust controls automatic versus per-call approval independently of the
  selected sandbox. Neither trusting a folder nor approving a call changes scope.
- `/permissions` and `/status` show scope and approval mode, with a resume command
  for restarting with another scope. Runtime changes are outside this change.
- Existing execution grants and sandbox backends enforce the chosen profile.
  There is no automatic fallback to off after an execution failure.
- On resume, the current invocation chooses permissions; old journal contents
  cannot restore broader permissions.

## Verification

Cover CLI parsing and conflicts, trust/profile independence, real sibling writes
through the tool runtime, read-only rejection, and PTY startup/permission display.
Run relevant Rust suites and the PTY smoke, then `cargo install --path .`.
