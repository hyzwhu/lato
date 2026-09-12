# TUI Live Progress Implementation Plan

**Goal:** Make streamed reasoning visible and keep truthful context metadata visible during generation.

**Architecture:** Track generation phase and reasoning segments in the reducer; render dynamic cards from actual stream events. A dedicated bottom status renderer shares available context estimates across welcome/chat/panels.

**Tech Stack:** Existing Rust, ratatui, crossterm, Tokio.

- [x] Correct stream ordering and track reasoning segment timing, completion and disclosure.
- [x] Add a tick-based activity indicator, live reasoning tail, F2 expansion and stable bottom context/status rows.
- [x] Test event sequences and 46/60/90/120-column rendering, including unknown context and running tool states.
- [x] Extend PTY fixture to stream reasoning incrementally; run regression tests and install with `cargo install --path .`.

Validation: 103 application tests, 127 model parser/adapter tests, 13 runtime-session tests, 16 skills-runtime tests, CLI regressions and the incremental reasoning PTY scenario pass. Local release installed with `cargo install --path .`.
