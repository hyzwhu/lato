# Slash command completion design

## Goal

When the TUI composer contains `/`, show every available slash command. As the user continues typing the command name, filter the list in real time. The behavior must be identical on the welcome screen and the main conversation screen.

## Scope

- Add an inline command suggestion popup above the bottom composer.
- Cover every currently supported slash command: `/help`, `/new`, `/clear`, `/sessions`, `/rename`, `/delete`, `/model`, `/login`, `/doctor`, `/search`, `/lang`, `/language`, `/approve`, `/status`, `/permissions`, `/exit`, and `/quit`.
- Keep the existing Cmd/Ctrl+K command palette available as a separate shortcut.
- Do not add new slash commands or change the meaning of existing commands.

## Interaction

- Typing `/` into an otherwise empty composer opens the popup with all slash commands.
- Typing more command-name characters filters candidates by case-insensitive prefix. For example, `/mo` leaves `/model`.
- Up and Down move the selected candidate while the popup is open.
- Enter completes the selected command into the composer. A second Enter executes it. If the composer already exactly matches a command, Enter executes it directly.
- Escape closes the popup without clearing or changing the composer.
- Backspace and other normal editing operations recompute the candidates. Returning to `/` restores the full list.
- The popup closes when there is no matching command, the input is not a slash-command prefix, or the input has entered an argument section such as `/rename My Session`.
- Pasting a slash-command prefix follows the same visibility and filtering rules as typing.

## Architecture

Introduce one shared slash-command registry containing each command name and its localized short description. The registry is the source used by completion rendering and `/help` output, and command dispatch remains responsible for executing commands. Alias entries remain visible because they are accepted inputs.

`AppState` derives the visible candidate set from the current composer value and stores only transient navigation state, such as the selected candidate and whether the user dismissed the popup for the current input. Composer edits normalize the selection so it never points outside the filtered list.

The renderer reserves or overlays a bounded area immediately above the composer and displays as many matching commands as fit. The selected row uses the existing amber highlight so the feature remains visually consistent with the command palette. Narrow terminals retain the composer and show a clipped, scroll-following candidate list rather than forcing a larger minimum layout.

## Command flow

Keyboard and paste events first update the composer, then refresh command completion state. When candidates are visible, Up, Down, Enter, and Escape are consumed by completion. Other composer keys retain their existing behavior. Submission continues through the existing slash-command dispatcher after completion no longer consumes Enter.

## Error handling

An unmatched slash prefix hides suggestions but remains editable. Submitting it continues to produce the existing unknown-command error. Completion performs no backend work and cannot change sessions, credentials, permissions, or conversation state.

## Testing

- Unit-test all-command discovery for `/`, case-insensitive prefix filtering, argument suppression, unmatched prefixes, and selection normalization.
- Test keyboard behavior for Up, Down, Enter completion, second-Enter execution, Escape dismissal, Backspace, and paste.
- Render-test that suggestions appear from both welcome and main screens and that the selected candidate is visible in constrained layouts.
- Run the focused TUI tests, the TUI CLI integration tests, and the PTY smoke test when the environment supports it.
- Run `cargo install --path .` after tests so the locally installed `lato` command contains the feature.
