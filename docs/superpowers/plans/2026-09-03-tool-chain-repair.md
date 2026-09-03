# Tool chain repair

The user authorized diagnosis, repair against the local Codex and grok-build
repositories, testing, and local installation. Preserve existing uncommitted work.

## Evidence and design

- Codex `codex-api/src/sse/responses.rs::process_responses_event` consumes
  complete `response.output_item.done` items without requiring an added event.
- grok-build `xai-grok-sampler/src/stream/responses.rs::observe_for_recovery`
  treats completed items and terminal response output as authoritative.
- grok-build `stream/chat_completions.rs` accumulates tools in a numeric BTreeMap.
- Lato's generic HTTP parser ignores completed Responses items/output; its
  Codex mapper requires pending state and discards terminal output. Both HTTP
  transports wait for EOF even after a terminal event. JSON is parsed line by
  line, losing pretty-printed responses. Provider history conversion drops text
  when an assistant message also contains tools.

Use a shared Responses event mapper with call-ID deduplication, keep streaming
text delivery, end transport reads at terminal events, parse complete JSON
responses as documents, and retain text in provider history. Avoid replacing the
runtime or changing policy/approval behavior.

## Implementation and validation

1. Add failing parser and transport regressions in lato-ai for done-only items,
   terminal output, duplicate completion, pretty JSON, provider failures, numeric
   call ordering, and an HTTP connection held open after completion.
2. Repair `codex/events.rs`, reuse it in `stream.rs`, and terminate `codex/sse.rs`
   at completion. Preserve text in `api.rs` provider conversions.
3. Run focused tests. Add a headless CLI fixture which performs actual workspace
   writes/reads and verifies the second model request carries matching outputs.
4. Run workspace tests, formatting and clippy; install with `cargo install --path .`.
5. Smoke-test the installed binary and record results. Do not claim live vendor
   validation from offline HTTP fixtures.

## Additional findings during acceptance

The live configured Codex smoke test initially returned only the working directory
without contacting the model. Narrow `src/cli.rs` local fact matching to complete
queries, add a regression, and run all four end-to-end fixtures with the Chinese
"create in the current directory" prompt. The live model then created the exact
requested file and returned VERIFIED.

Full-suite repetition exposed a pre-existing journal import temp-name collision.
Append an atomic per-process sequence to timestamp-based import paths and strengthen
the existing concurrent-import test to sixteen callers, retaining convergence and
no-overwrite assertions.
