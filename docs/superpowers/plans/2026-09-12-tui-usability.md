# TUI Usability Implementation Plan

**Goal:** Deliver discoverable commands, real file context, user-invoked skills and a multiline chat-first interface.

**Architecture:** Keep ratatui and the current event reducer. Share command metadata between slash and palette; isolate Unicode editing, bounded file discovery and backend skill APIs. Integrate results through typed state and backend events.

**Tech Stack:** Rust, ratatui, crossterm, Tokio, existing ACP runtime.

## Constraints

Preserve existing uncommitted workflow work. File limits are 64 KiB each and 256 KiB total. Candidate scans run outside the input event loop. All user skills use the runtime's user invocation semantics. Chinese and English remain supported.

## Tasks

- [x] Extend `src/tui/input.rs` with grapheme-safe range replacement, wrapped viewport, logical vertical movement and line boundaries. Run `cargo test -p lato --bin lato tui::input` for Unicode wrapping, cursor visibility and boundary behavior.
- [x] Add `src/tui/context.rs`: bounded ignored-file-aware index, cursor-local @ token parsing, reference encoding and bounded canonical workspace reads. Test spaces, middle-of-token replacement, ignore rules, binary/oversize files and symlink escape.
- [x] Expose skill listing and user invocation via protocol, host, runtime and client. Test user visibility, qualified identities, arguments and tool-scope behavior using fixture skills.
- [x] Integrate command/file/skill selection in TUI state and keyboard handlers. Tab accepts without sending; Esc dismisses before any cancel; commands share palette dispatch. Test navigation, unknown command draft retention and actual backend requests.
- [x] Render a chat-first layout with optional focused side panels, 1–8 input rows, capped candidate panels, selection count and contextual controls. Test 60/90/120 columns, small heights, Chinese and multiline cursor visibility.
- [x] Add history with draft restoration, update README, extend real PTY fixture coverage, run formatting and relevant tests. Install via `cargo install --path .` and check installed binary.

## Verification commands

```sh
cargo fmt --all -- --check
cargo test -p lato --bin lato
cargo test -p lato --test tui_cli
cargo build -p lato
python3 tests/tui_pty_smoke.py target/debug/lato
cargo install --path .
lato --version
```

Review test failures against intended keyboard changes; preserve session, approval and cancellation regression coverage. Integrate independently owned input/file/backend work before the final test pass.
