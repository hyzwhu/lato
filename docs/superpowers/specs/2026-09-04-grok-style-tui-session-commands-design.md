# Grok-style TUI session commands

## Goal

Make Lato's TUI session commands follow the interaction model used by
`grok-build`. `/new` must create a fresh session and present the start screen.
`/rename` must rename the active session through the composer rather than an
independent input dialog.

## Command behavior

### `/new` and `/clear`

`/new` starts a fresh session. `/clear` remains its alias, matching
`grok-build`; it does not erase or overwrite the previous persisted session.
The command clears the current in-memory conversation presentation, asks the
backend to create the replacement session, and changes to the Welcome screen
only after the backend confirms success. The new session becomes the selected
session and the refreshed session list includes both the old and new sessions.

If session creation fails, the TUI keeps the current session selected and shows
the backend error. It must not display a successful fresh-session state before
the backend acknowledgment.

### `/rename`

The canonical executable form is `/rename <title>`. The command preserves the
title's user-entered case, trims surrounding whitespace, and sends a rename for
the active session. A successful rename keeps the current conversation open,
updates the corresponding session-list item, and appends a system confirmation
message. A failure keeps the existing visible title and conversation and shows
the backend error.

Typing exactly `/rename` uses composer-based completion. When the active
session has a non-empty title, the first Enter fills `/rename <current title>`;
the next Enter submits it. When there is no usable current title, submitting
the bare command shows a usage error. No separate rename dialog is opened.

Title sanitization and length rules remain owned by the existing backend. This
change does not add `grok-build`'s `/rename --auto` behavior.

## State and data flow

The slash-command handler emits backend commands but does not claim completion.
For a new session, the backend acknowledgment drives a single state transition
that resets messages, tools, scrolling, focus, transient errors, and the
composer, then selects Welcome. For a rename, the acknowledgment updates the
session summary and adds a localized confirmation message while remaining on
the current screen.

Composer completion gains the narrow ability to expand the exact `/rename`
command with the active session title. Existing command-name completion and
arrow-key navigation remain unchanged.

## Testing

Focused tests cover:

- `/new` and `/clear` dispatch fresh-session creation without changing the
  visible session prematurely.
- A successful fresh-session event selects the new ID, resets transient
  conversation state, and shows Welcome.
- Exact `/rename` completion inserts the current title on first Enter and sends
  the rename on the next Enter.
- `/rename <title>` preserves title case.
- Successful rename updates the session list, keeps the conversation, and adds
  a confirmation message.
- Backend errors do not produce success-state transitions.

After focused tests, run the relevant Rust test suite and install the verified
binary locally with `cargo install --path .`.
