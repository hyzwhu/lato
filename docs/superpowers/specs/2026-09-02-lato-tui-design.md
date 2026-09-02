# Lato TUI Design

## Purpose

Replace Lato's readline-based interactive mode with a native, full-screen Rust TUI based on the supplied welcome-screen and three-column design. The TUI must expose real Lato sessions, streamed assistant output, tool activity, cancellation, and configuration while preserving every existing headless and protocol-oriented CLI path.

The interface ships in Simplified Chinese and English. It follows the system locale by default, accepts an explicit CLI override, and persists language changes made inside the TUI.

## Scope

The following invocations launch the TUI:

- `lato`
- `lato resume <session-id>`

The following interfaces retain their current non-TUI behavior:

- `lato -p ...`
- `lato sessions`
- `lato login ...`
- `lato doctor ...`
- `lato acp`
- All JSON and stdio protocol contracts

The TUI implements the supplied two-screen information architecture: a centered welcome screen followed by a session/chat/tool workspace with a command-palette overlay. Browser technologies from the prototype are reference material only and are not runtime dependencies.

## Technology

Use native Rust with:

- `ratatui` for layout, widgets, styling, and test buffers
- `crossterm` for terminal setup, input, resize, paste, and alternate-screen control
- Tokio channels and tasks for asynchronous UI/backend coordination
- `unicode-width` and `unicode-segmentation` for correct Chinese and English cursor and layout behavior

Use stable crate APIs. Do not adopt Codex's patched Crossterm or Ratatui unstable features unless a verified terminal defect makes one necessary.

## Architecture

Create a focused `src/tui/` module. The TUI owns presentation state and rendering, while CLI startup and the existing ACP-backed session client remain responsible for authentication, model configuration, trust, persistence, and agent execution.

The major boundaries are:

1. **Interactive bootstrap** prepares Lato home, locale, model, workspace trust, and a new or resumed `InteractiveAcpClient`.
2. **TUI application** owns `AppState`, translates terminal input into actions, reduces events, and requests redraws.
3. **Session controller** maps UI actions such as submitting, cancelling, clearing, switching model, and approving a tool call onto existing Lato operations.
4. **Event adapter** converts ACP/session updates into typed `AppEvent` values without exposing raw JSON to widgets.
5. **Renderer** is a pure projection from `AppState` plus terminal dimensions to a Ratatui frame.

Long-running model and tool operations never block terminal input or drawing. Tokio tasks send updates through a bounded event channel. The application loop serializes state changes, ensuring deterministic UI behavior and straightforward testing.

## Module Responsibilities

The implementation should keep these responsibilities isolated, while allowing exact filenames to follow existing repository conventions:

- `tui::app`: application state, reducer, focus, overlays, responsive mode, and lifecycle
- `tui::event`: terminal, timer, and backend event types plus event multiplexing
- `tui::backend`: bridge to `InteractiveAcpClient`, cancellation, and ACP event normalization
- `tui::input`: Unicode-safe multiline editor, cursor movement, history, and paste handling
- `tui::i18n`: locale resolution, typed message keys, Chinese and English catalogs
- `tui::render`: top-level layout selection and terminal-size fallback
- `tui::widgets`: welcome, sessions, transcript, tools, composer, status bar, command palette, and search
- `tui::terminal`: raw mode, alternate screen, panic-safe restoration, and test backend setup

The existing oversized `interactive()` function is decomposed so bootstrap and backend operations can be reused by the TUI. Unrelated CLI behavior is not refactored.

## Screens and Layout

### Welcome Screen

The welcome screen centers:

- Lato ASCII logo and current package version
- Workspace path
- Selected model
- API/model readiness status
- A single prompt editor
- A localized shortcut bar

Submitting a non-empty prompt creates or resumes the session, transitions to the main screen, and immediately starts the turn. A resumed invocation may transition directly to the main screen after initialization while retaining an initial recoverable error surface if resume fails.

### Main Screen

At normal widths, the body uses the supplied proportions:

- Sessions: 20%
- Conversation: 55%
- Tool activity: 25%

The sessions panel shows persisted sessions, timestamps, the active session, and a new-session action. The conversation panel shows workspace/token metadata, transcript content, collapsible reasoning entries when such events are available, inline tool references, the composer, response duration, and stop action. The tools panel shows actual tool name, argument summary, elapsed time, running/completed/error state, and a truncated result or error preview.

Invisible or low-contrast separators, indentation, and panel dimming reproduce the supplied modern-restraint visual direction. The palette uses the design's charcoal background, amber active/warning accent, soft blue completion/output accent, primary text, and muted text, subject to terminal color capability.

### Responsive Modes

Responsive layout is deterministic from terminal width:

- Wide mode displays all three columns.
- Medium mode keeps conversation and sessions visible and exposes tools as a focusable drawer.
- Narrow mode keeps the conversation visible and exposes sessions and tools as focusable drawers.
- Below the minimum usable height or width, the renderer shows a localized resize message and exit shortcut instead of corrupted content.

Exact thresholds are constants covered by layout tests and chosen from the minimum widths required by both translation catalogs.

## Interaction

- `Tab` and `Shift+Tab` cycle panel focus.
- `j` and `k` scroll the focused non-editor panel.
- Arrow keys, Home/End, word movement, Backspace/Delete, Enter, and bracketed paste operate in editors using grapheme-safe positions.
- `Cmd+K` on macOS and `Ctrl+K` elsewhere open the command palette; both may be accepted when the terminal reports them reliably.
- `Esc` closes the topmost overlay, leaves search, or returns focus in that order.
- `/` opens transcript/session search when focus is not in an editor.
- `Ctrl+C` cancels an active turn. When idle, it exits after restoring the terminal.
- The command palette includes new session, switch session, switch model, switch language, clear conversation, status, approve one mutating tool call, and exit.

Existing slash-command capabilities remain available either as composer commands or command-palette actions. Unknown commands produce a localized inline error without terminating the application.

## Internationalization

Stable user-visible UI text must not be scattered through rendering code. A typed message-key interface returns text from complete `zh-CN` and `en` catalogs.

Locale precedence is:

1. `--lang zh-CN|en`
2. Persisted language in `~/.lato/config.json`
3. System locale (`LC_ALL`, `LC_MESSAGES`, then `LANG` where available)
4. English fallback

Language changes from the command palette take effect immediately and persist atomically. The CLI override applies to the current run and becomes the active persisted preference so subsequent launches are consistent.

Only stable product UI is translated. User content, model output, tool names, file paths, provider errors, and protocol payloads retain their source language. Dynamic error wrappers add a localized prefix without altering the underlying diagnostic.

All truncation, wrapping, selection, and cursor calculations use display width and grapheme boundaries. Snapshot coverage includes English strings that are longer than their Chinese equivalents.

## State and Data Flow

The application uses a single `AppState` containing screen, locale, workspace/model metadata, responsive mode, focus, overlays, sessions, active transcript, tool calls, composer state, search state, current turn status, and recoverable errors.

Inputs enter through one event queue:

- Terminal key, paste, and resize events
- Periodic ticks for timers and animation
- Backend initialization and session events
- Streamed text deltas
- Tool-start, tool-finish, and tool-error events
- Cancellation and command completion events

The reducer updates state synchronously. Effects are emitted as typed commands and executed asynchronously by the controller. Effect results re-enter as events. This unidirectional loop prevents widgets from mutating backend state and makes reducer and rendering tests deterministic.

Submitting a prompt appends the user entry immediately, creates an in-progress assistant entry, clears the composer, and starts elapsed-time tracking. Stream deltas append to that entry. Tool events are keyed by call ID and update both the inline transcript reference and right-hand card. Completion or failure finalizes the turn and returns focus to the composer unless the user moved it.

## Cancellation and Approvals

Cancellation must propagate to the active Agent turn rather than merely hiding its output. The session client gains or exposes a cancellation operation backed by the existing runtime cancellation path. Late updates from a cancelled turn are ignored by turn ID.

Mutating-tool approvals must be presented inside the TUI rather than reading from stdin while raw mode is active. An approval request becomes a modal event with approve/deny actions. The existing one-shot `/approve` behavior remains available as a command-palette action.

## Error Handling and Terminal Safety

- Authentication, model, and resume initialization failures appear as recoverable welcome-screen errors with relevant actions.
- Prompt and tool failures finalize only the current turn and remain visible in the transcript/tool card.
- Malformed or unknown backend events are logged and ignored unless they prevent completion of the active request.
- Bounded channels apply backpressure without dropping semantic completion, error, cancellation, or approval events. Adjacent text deltas may be coalesced before rendering.
- Non-TTY input/output retains the current error directing users to headless mode.
- Terminal setup uses an RAII guard that restores raw mode, alternate screen, mouse/paste modes, and cursor visibility on normal return and unwinding.
- A panic hook attempts restoration before delegating to the previous hook.
- Configuration writes use the existing atomic temporary-file and rename pattern.

## Compatibility

The feature must preserve:

- Existing CLI argument semantics and exit codes outside interactive mode
- Existing session storage and resume identifiers
- Existing model/provider configuration and credential storage
- Existing workspace trust and tool policy behavior
- ACP stdio behavior
- Headless output contracts, including JSON modes

The old readline UI is removed from the default interactive path after the TUI reaches parity. Supporting helpers may remain only if still used by setup/login flows.

## Testing and Verification

### Unit Tests

- Reducer transitions for welcome, submit, stream, tool lifecycle, cancellation, overlays, and exit
- Locale precedence, persistence, fallback, and complete catalog keys
- Grapheme-safe input editing and display-width calculations for Chinese, English, emoji, and combining characters
- Responsive-mode thresholds and focus traversal
- ACP JSON-to-typed-event conversion, including malformed payloads

### Rendering Tests

Use Ratatui's test backend and stable snapshots for:

- Chinese and English welcome screens
- Wide three-column main screen
- Medium two-panel mode with tools drawer
- Narrow conversation mode with sessions/tools drawers
- Command palette, search, approval modal, errors, active response, and tool states

Snapshots normalize timers, paths, version text, and terminal capabilities so they are deterministic.

### Integration Tests

- Mock ACP session streams assistant text and tool lifecycle events into the TUI controller
- Active cancellation reaches the backend and suppresses late turn updates
- New and resumed sessions display real persisted data
- Language switching updates and persists without restart
- Existing non-interactive CLI test suite remains green

### PTY Smoke Tests

A pseudo-terminal smoke test launches the compiled binary, submits input, resizes the terminal, opens/closes an overlay, cancels or completes a turn, exits, and verifies that canonical terminal mode and cursor state are restored. Both locale variants receive a launch/exit smoke path.

### Final Local Deployment

Run formatting, Clippy, targeted tests, full practical workspace tests, and `cargo install --path .`. Validate the installed `lato` binary for TUI launch/resume and representative headless commands before reporting completion.

## Acceptance Criteria

The work is complete when:

1. The installed `lato` command renders the supplied visual structure as a native Rust TUI.
2. Real streamed assistant text, session data, tool calls, results, failures, cancellation, and approvals are interactive in the TUI.
3. Chinese and English can be selected from CLI and TUI, persist correctly, and render without width or cursor defects in covered layouts.
4. Narrow terminals degrade predictably and undersized terminals show a usable resize message.
5. The terminal is restored after normal exit, cancellation, backend failure, and panic-tested paths.
6. Existing headless, diagnostic, login, session-list, resume identifier, and ACP contracts do not regress.
7. Automated tests pass and `cargo install --path .` produces the locally deployed binary used for final smoke testing.
