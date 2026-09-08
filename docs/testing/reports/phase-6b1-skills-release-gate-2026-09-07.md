# Phase 6B1 Skills Release Gate

Date: 2026-09-07

Result: PASS

Phase 6B1 is based on Grok Build commit
`bb7f39d5858cbf5e00de639367f59debbdcb0138`. This gate covers trusted plugin
skill discovery, immutable turn binding, explicit invocation, one-model-step
tool narrowing, hash-only extension audit records, and the installed command.
Hooks and MCP execution are not part of this checkpoint.

## Focused audit verification

```text
$ cargo test -p lato-core journal
4 passed; 0 failed; 4 filtered out

$ cargo test -p lato-agent --test skills_runtime audit
2 passed; 0 failed; 10 filtered out
```

The integration checks observe the persisted journal from inside the model
fixture: catalog materialization is committed before the first request, and a
successful invocation is committed before the next request. Success and
rejection records contain hashes rather than the skill body, invocation
arguments, or expanded message. Extension audit records replay without adding
conversation messages or changing terminal state.

## Repository quality gate

The first workspace attempt exhausted the local volume while linking
(`errno=28`). Only this isolated worktree's rebuildable `target/` was removed
with `cargo clean`; the successful run disabled incremental and test debug
artifacts to stay within the host's storage limit.

```text
$ cargo fmt --all -- --check
PASS

$ CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 cargo test --workspace
932 passed; 0 failed; 0 ignored; 0 measured
91 test/doc-test result blocks

$ CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 cargo clippy --workspace --all-targets -- -D warnings
Finished dev profile; 0 warnings; PASS
```

Two pre-existing gate fixtures were stale and were repaired in isolated,
test-only commits before the final green run:

- `52cc1c7 test: repair sandbox write fixture` changes the real `write_file`
  fixture field from obsolete `content` to the canonical `contents` schema;
- `8f29c11 test: follow scoped tool runtime wiring` updates the static wiring
  assertion from the pre-skill unscoped calls to `model_definitions_scoped`,
  `prepare_scoped`, and `decision`.

## Local deployment

```text
$ CARGO_INCREMENTAL=0 cargo install --path .
Replacing /Users/huangyongzhao/.cargo/bin/lato
Replaced package lato v0.1.0-beta.2 (executable lato)

$ command -v lato
/Users/huangyongzhao/.cargo/bin/lato

$ lato --version
lato 0.1.0-beta.2
```

## Installed-command smoke

```text
$ LATO_SMOKE_BINARY="$(command -v lato)" cargo test --test phase6b_skills_smoke -- --nocapture
running 1 test
test phase6b_installed_command_smoke_exercises_skill_invocation ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

The bounded localhost fixture starts the installed binary in a temporary,
trusted Git workspace with a CLI plugin. It verifies that request one contains
the qualified authored description, asks the built-in `skill` tool to expand
two arguments, then verifies request two contains the expanded XML body and
only the scoped `read_file` tool. The fixture returns
`phase6b-skills-smoke-ok`, joins the server thread, waits for the child process,
validates the persisted audit records, and drops all temporary workspace,
plugin, home, and session resources.
