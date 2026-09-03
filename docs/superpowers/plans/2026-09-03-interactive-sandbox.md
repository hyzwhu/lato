# Interactive Sandbox Implementation Plan

**Goal:** Allow an explicit sandbox selection for new and resumed interactive sessions.

**Architecture:** Pass `Option<SandboxArg>` from CLI parsing to the existing TUI
startup. Resolve it before constructing the ACP client and its immutable tool
runtime. Keep the trust decision independent and reuse existing enforcement.

**Tech Stack:** Rust, Clap, Tokio, Ratatui, existing Lato policy/runtime crates.

## Global constraints

- No dependency changes, automatic permission escalation, or persisted full access.
- Preserve headless defaults and rejection of unrelated subcommand flags.
- Keep all prompts within the existing TUI lifetime.
- Install locally after tests using `cargo install --path .`.

## Tasks

- [x] Extend `src/args.rs` with interactive/resume sandbox fields and a global
  sandbox flag; allow it only for these modes and headless. Add table-driven
  tests covering all profiles, resume flag order, defaults, and conflicts.
- [x] Add `src/permissions.rs` for localized choices, trust construction and
  status text; wire it from `src/main.rs`, `src/cli.rs`, and `src/tui/mod.rs`.
  Test that each explicit profile survives both trust decisions.
- [x] Add real tool execution regressions proving sibling writes succeed with
  explicit off and fail under workspace; read-only rejects in-workspace writes.
  Use temporary sibling directories and the built-in runtime.
- [x] Update `tests/tui_pty_smoke.py` to select scope during first-run setup and
  assert `/permissions`. Document examples and resume semantics in `README.md`.
- [x] Run `cargo fmt --all -- --check`, relevant Rust suites, and
  `python3 tests/tui_pty_smoke.py`; inspect the diff, fix failures, then run
  `cargo install --path .` and verify the installed CLI help.

## Validation results

- `cargo test -p lato -p lato-workspace -p lato-policy -p lato-tools -p lato-agent`: passed.
- `python3 tests/tui_pty_smoke.py`: all three scenarios passed, including an off
  session resumed as read-only with preserved conversation and independent trust.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo install --path .`: succeeded; replaced `~/.cargo/bin/lato`.
- Installed CLI help advertises interactive and resume sandbox selection.
