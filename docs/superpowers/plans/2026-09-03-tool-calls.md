# Collapsible Tool Calls Implementation Plan

**Goal:** Make every tool call collapsible and allow navigation through complete tool output.

**Architecture:** A dedicated `src/tui/tool_panel.rs` owns viewport state, key routing, wrapping, and tool rendering. `AppState` owns the panel state and each tool's expanded flag. Rendering updates viewport measurements through mutable application state, including modal backgrounds.

**Tech Stack:** Rust, ratatui 0.29, crossterm, existing Unicode segmentation/width crates.

## Constraints

Preserve existing styles, responsive layouts, and unrelated working-tree changes. Keep tools navigation separate from chat and preserve inspection during backend updates. No new dependencies. Deploy locally after tests.

## Tasks

- [x] Add tool expansion and independent viewport state; route tool keys before composer keys; reset navigation on clear/resume.
- [x] Render collapsed summaries and full wrapped details; maintain selection visibility, bounded paging, scrollbar, localized hints, and resize handling.
- [x] Verify lifecycle/reset, render overflow and Unicode, and keyboard routing without composer submission; update README controls.
- [x] Run `cargo test -p lato`, inspect the diff, run `cargo install --path .`, and verify `lato --version`.

## Validation results

- `cargo test -p lato`: 101 tests passed.
- `python3 tests/tui_pty_smoke.py target/debug/lato`: all three scenarios passed.
- `cargo fmt --all --check` and `git diff --check`: passed.
- `cargo install --path .`: replaced the local executable successfully.
- `lato --version`: `lato 0.1.0-beta.2`; executable resolved to `/Users/huangyongzhao/.cargo/bin/lato`.
