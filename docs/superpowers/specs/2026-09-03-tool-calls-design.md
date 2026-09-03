# Collapsible tool calls

Tool calls appear as collapsed summaries with a disclosure marker, name, status, and elapsed time. Each call retains its own expanded state as results arrive. Expanded calls show complete arguments and results, preserving newlines and wrapping Unicode text to the panel width.

Tab focuses the tools panel. Up/Down and j/k select calls; Enter/Space toggle details; Left/Right collapse/expand. PgUp/PgDn scroll the panel by a viewport with one row of overlap, including inside long results. Home/End select the first/last call. Selection and scrolling are independent from chat and never submit the composer. A scrollbar and localized footer expose navigation.

New calls follow the latest entry while tools are not focused; active inspection is preserved while tools are focused. Clear/resume resets navigation. Resize recomputes wrapping and clamps the viewport. Preserve the existing colors and responsive layouts.

Verify reducer lifecycle/reset behavior, keyboard routing with a pending composer, and rendered output for overflow, multiline Unicode, collapse, paging, and resizing. Run relevant Cargo tests and install with `cargo install --path .`.
