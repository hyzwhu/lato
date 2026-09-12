# Resume Session Selector Design

**Status:** Implemented (2026-09-12)

## Goal

Make `lato resume <reference>` restore an existing session when the reference is either its exact session ID or its exact title. When several sessions share that title, let the user choose which session to restore.

## Resolution Rules

1. An exact session ID match always wins, even if another session has the same text as its title.
2. If there is no ID match, resolve exact title matches.
3. A single title match resumes immediately.
4. Multiple title matches open an interactive chooser. Candidates are ordered by most recently updated first, display the title, update time, and complete session ID, and initially select the most recent candidate.
5. Escape cancels without starting or changing a session.
6. No match reports that no session has the supplied ID or title.

Title matching is exact and case-sensitive. Fuzzy and substring matching are outside this fix so that a command cannot unexpectedly resume the wrong session.

## Startup Flow

The interactive resume path lists persisted session summaries before configuring a model, asking for workspace trust, or choosing a sandbox. A pure resolver classifies the supplied reference as a direct match, an ambiguous set, or missing. Direct matches continue with the resolved session ID. Ambiguous matches are presented through the existing TUI dialog machinery and continue only after selection. Missing or cancelled resolution exits cleanly.

The actual session hydration remains in the existing ACP `session/resume` path; this change only resolves the user-facing reference to a stored ID before hydration.

## Error Handling

- Failure to list persisted sessions is returned as a normal CLI error.
- A missing reference is reported before unrelated setup prompts.
- Cancelling the chooser is treated like cancelling another startup dialog and exits successfully.
- If the selected session becomes unavailable before hydration, the existing fail-closed `session/resume` error remains authoritative.

## Tests

- Exact session ID resolves directly.
- A unique exact title resolves directly.
- An ID match takes precedence over a title collision.
- Duplicate titles produce candidates ordered by recency.
- Unknown references report no match.
- Interactive selection resumes the chosen duplicate and cancellation does not start a session.
- Existing ID-based resume behavior and context hydration continue to pass.

