# TUI continuity and beta 2

The interactive invocation owns one terminal guard from startup through quit. Provider/model selection, credentials, OAuth notices, folder trust and errors use asynchronous TUI dialogs. Existing noninteractive CLI commands remain available for scripting.

Model and login commands open cancellable dialogs over the conversation. Model discovery runs asynchronously, lists can be filtered, and secret inputs are masked. The selected stream is swapped only after configuration succeeds, keeping the session and transcript. Session navigation uses the existing ACP host. Configuration/session changes are refused while a turn is active.

The composer and search cursor use the rendered content rectangle, Unicode grapheme widths and a horizontally scrolling viewport. Slash starts a command; Ctrl+F opens search. Existing colors/layout are retained.

Validation covers dialog keyboard/cancellation/secret rendering, terminal cursor coordinates, Unicode and long input, slash dispatch, stream switching and session navigation. Run workspace tests and formatting, install locally, then publish master and v0.1.0-beta.2 to the github remote. The existing release workflow builds five platform archives and publishes a prerelease.
