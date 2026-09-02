# Lato Phase 4A Canonical Event Journal Design

## 1. Status and decision

Phase 4A replaces the current `HistoryItem` transcript as the authoritative session store with a bounded, append-only canonical journal. The journal is owned by a single writer, is flushed before related state becomes externally visible, and is replayed strictly enough that corrupted or divergent history cannot silently become model context.

This phase intentionally separates durable canonical records from the live client event stream. Model and reasoning deltas remain live events; they are not persisted as recovery truth. Complete messages, tool calls and results, policy decisions, and terminal state are persisted.

The selected approach is:

- define the storage contract and canonical journal types in `lato-core`;
- implement a local JSONL writer and replay reader in a new `lato-store` crate;
- make `lato-runtime::SessionLoop` the sole journal sequencer and durability coordinator;
- adapt `lato-agent` to commit canonical conversation, tool, and policy records;
- lazily import legacy transcripts when a session is first resumed;
- defer snapshots, compaction, indexes, and automatic recovery of uncertain side effects.

## 2. Upstream design comparison

The repository pins these reference baselines:

- Codex `633ab199cfd724aa78013c006b27a2b3d049fc3b`
- Grok Build `bb7f39d5858cbf5e00de639367f59debbdcb0138`

Codex's rollout recorder provides the primary session-history pattern: canonical rollout items, a single background writer, ordered pending writes, explicit flush barriers, retry after reopening a failed writer, and append-only JSONL that remains inspectable. Codex also persists a completed model tool-call item before queueing the tool execution.

Grok Build's workflow journal supplies the stricter recovery mechanisms: dense sequence validation, canonical request hashes, byte and entry caps enforced before append, refusal to restore symlinks or non-files, recovery of only a torn final record, fatal intermediate corruption, and loud replay divergence.

Lato combines those mechanisms without copying either product model wholesale:

- Codex-style canonical records and single-writer barriers;
- Grok Build-style bounds, strict validation, canonical hashing, and torn-tail handling;
- an additional write-ahead durability barrier before non-idempotent tool execution.

Any production code copied or structurally derived during implementation must be added to `docs/superpowers/reference/lato-upstream-sources.md` with the exact upstream path, commit, license, reuse mode, and carried tests.

## 3. Goals

Phase 4A must:

1. make canonical session history append-only and replayable;
2. keep journal order deterministic under concurrent driver activity;
3. persist important state before broadcasting its corresponding visible event;
4. prevent automatic repetition of a non-idempotent tool after an uncertain crash;
5. fail closed on corruption, divergence, unsafe paths, and restore-budget overflow;
6. preserve current CLI and ACP session list/resume behavior;
7. import existing transcripts without deleting or modifying them;
8. leave stable boundaries for later snapshots and compaction.

## 4. Non-goals

Phase 4A does not implement:

- snapshots or snapshot selection;
- context compaction or history rewriting;
- SQLite or another secondary index;
- a new public replay command or UI;
- persistence of token-by-token model or reasoning deltas;
- automatic retry of an uncertain tool call;
- workflow, subagent, MCP, or plugin replay;
- compression or archival of journal files;
- migration away from `LegacyTurnDriver` or `ModelPortStreamAdapter`.

## 5. Crate and dependency boundaries

The intended dependency direction is:

```text
lato-agent ──> lato-runtime ──> lato-core <── lato-store
     │                │
     └────────────────┴── runtime wiring receives Arc<dyn EventStore>
```

### 5.1 `lato-core`

`lato-core` owns provider-independent, serializable types:

- `JournalEnvelope`;
- `JournalRecord`;
- `JournalRecordId`;
- `JournalCursor`;
- `JournalError` with stable codes and retryability;
- the object-safe asynchronous `EventStore` contract;
- canonical hashing helpers or the canonical value representation required by the contract.

The core contract does not know file paths, JSONL, `HistoryItem`, ACP, or a concrete runtime.

### 5.2 `lato-store`

`lato-store` provides `FileEventStore`. It owns:

- secure session-directory and journal-file access;
- the single writer task and its bounded command channel;
- append, flush, sync, replay, validation, and torn-tail repair;
- journal byte and entry limits;
- atomic creation used by legacy migration;
- deterministic session listing across journal directories.

It depends only on `lato-core` plus infrastructure libraries.

### 5.3 `lato-runtime`

`SessionLoop` remains the sole owner of session ordering. It assigns dense journal sequence numbers, sends canonical records to the writer, awaits the requested durability barrier, and only then emits the corresponding live event.

The runtime depends on the `EventStore` contract, not `FileEventStore`. Tests can inject an in-memory or faulting store.

### 5.4 `lato-agent`

The agent layer projects current actor behavior into typed records. It converts complete conversation items, tool lifecycle boundaries, and redacted policy decisions into `JournalRecord` values. It also projects replayed canonical messages back into the current `HistoryItem` representation until the actor-native migration removes that adapter.

## 6. Canonical journal model

### 6.1 Envelope

Each JSONL line contains one `JournalEnvelope`:

```text
JournalEnvelope
├─ schema_version
├─ record_id
├─ session_id
├─ turn_id: Option<TurnId>
├─ journal_sequence
├─ timestamp_ms
└─ record: JournalRecord
```

`journal_sequence` starts at zero and is dense within one physical journal. It is independent from live `EventEnvelope.sequence`, because live events also contain non-durable deltas.

Record IDs are stable and unique within a session. A resumed session continues from the last valid journal sequence and cannot reuse an existing record ID.

### 6.2 Records

Phase 4A defines these record families:

- `SessionStarted`: stable session metadata required for validation and listing;
- `TurnInputAccepted`: the normalized user input that started or steered a turn;
- `ConversationItemCommitted`: one provider-independent `ModelMessage` after it is complete;
- `PolicyDecisionCommitted`: a redacted policy/approval stage, decision, capability set, and exact-call fingerprint;
- `ToolCallRequested`: the complete provider-independent model tool call, including model-visible arguments and their canonical hash;
- `ToolCallPrepared`: the approved execution intent: canonical tool identity, call ID, exact-call fingerprint, idempotency, side-effect classification, sandbox obligation, and output limits;
- `ToolCallCompleted`: call ID plus normalized and already-truncated `ToolOutput` or execution `ToolError`;
- `ToolCallRejected`: call ID plus the normalized validation, policy, or approval rejection projected back to the model;
- `TurnCompleted`, `TurnFailed`, and `TurnCancelled`;
- `SessionStopped`;
- `LegacyTranscriptImported`: source format version, imported item count, and content digest.

The journal necessarily contains user prompts, model-visible tool arguments, and model-visible tool results, so it is sensitive local data. File permissions and path protections are security boundaries, not optional hygiene. Lato must not add credential-store values, bearer headers, secret process environment variables, or unrestricted execution environments to journal records. Policy audit records follow the existing Phase 3 redaction rules and store argument hashes rather than raw arguments. Model-visible tool-call content remains in canonical conversation history because it is required to reconstruct model context.

`PolicyDecisionCommitted` has explicit stages for policy evaluation, approval request, approval resolution, and single-use grant consumption. An approval request must be durable before its prompt becomes visible, and the resolution must be durable before an approved tool advances to preparation.

`ConversationItemCommitted` stores complete user and non-tool assistant messages. Tool-call and tool-result model messages are projected directly from `ToolCallRequested` and `ToolCallCompleted`/`ToolCallRejected`; they are not duplicated as additional conversation records.

### 6.3 Canonical request hash

Tool request hashes are computed from:

- the canonical tool identity and version;
- call ID;
- canonicalized JSON arguments with recursively sorted object keys;
- capabilities;
- side-effect and idempotency declarations;
- sandbox obligation;
- model/session/turn identity relevant to exact-call approval.

The hash algorithm and encoded prefix are versioned. Replay must reject a prepared/completed pair whose call IDs or hashes disagree.

## 7. Live events versus durable records

Two internal operations are exposed to a turn driver:

- `emit_live(EventPayload)` sends transient presentation events such as `ModelDelta` and `ReasoningDelta` without journal persistence;
- `commit(JournalRecord, Durability)` sends a canonical record to the session loop, waits for the writer acknowledgement, and then allows the runtime to emit its corresponding typed live event.

`commit` is asynchronous. It cannot be implemented as an unacknowledged channel send because tool execution must wait for the write-ahead barrier.

The live stream is not a byte-for-byte replay log. Resume reconstructs durable state from canonical records. A future trace facility may separately record raw deltas for diagnostics.

## 8. Writer and durability protocol

### 8.1 Single writer

Each active session has one writer task and a bounded command channel. The writer owns the file handle, byte count, next expected sequence, pending suffix, and terminal failure state. No other component appends directly to a live journal.

Commands are:

- append one or more records and flush;
- append and `sync_data`;
- explicit flush;
- shutdown and drain.

Written records leave the pending queue only after their complete JSON line and newline have been written. On an I/O failure the writer drops the handle, preserves the unwritten suffix, reopens once, and retries the barrier. A second failure is returned as a fatal journal error.

### 8.2 Durability classes

Phase 4A uses two barriers:

- `Flush`: append complete lines and flush the userspace writer before a related state event is broadcast;
- `SyncData`: append, flush, and request durable file-data synchronization.

`SyncData` is required for:

- `ToolCallPrepared` when the tool is mutating, external, non-idempotent, or otherwise capable of an unrecoverable side effect;
- every `ToolCallCompleted` for such a call;
- turn completion, failure, or cancellation;
- session stop;
- completion of an imported legacy journal.

The first durable synchronization of a newly materialized journal also synchronizes its parent directory where the platform supports directory syncing. This prevents a durable record from referring to a journal directory entry that was never made durable.

Read-only, explicitly idempotent tool calls use `Flush` in Phase 4A, but Phase 4A still never retries an incomplete call automatically.

### 8.3 Ordering relative to visibility

For canonical state, the order is:

```text
allocate journal identity
→ validate capacity and sequence
→ append and satisfy barrier
→ update in-memory projection
→ emit related live event
```

If the journal barrier fails, the in-memory projection and client stream must not advance past the failed record. The turn terminates with a structured persistence failure.

## 9. Tool side-effect protocol

The tool boundary is:

```text
commit ToolCallRequested for the complete model tool-call item
→ evaluate policy
→ commit PolicyDecisionCommitted(Evaluated)
→ when required, commit PolicyDecisionCommitted(ApprovalRequested)
→ show approval request and await the user
→ commit PolicyDecisionCommitted(ApprovalResolved)
→ commit ToolCallPrepared with required barrier
→ invoke tool
→ normalize and truncate output
→ commit ToolCallCompleted with required barrier
→ project the stored result into model history
```

If validation, policy, or approval rejects the call, `ToolCallRejected` is committed before the rejection becomes visible or is projected into model history, and `ToolCallPrepared` is never written. A consumed one-shot grant is also committed before tool preparation.

Recovery classifies each prepared call:

- `ToolCallRequested` followed by `ToolCallRejected`: project the stored rejection and never execute the tool;
- matching `ToolCallPrepared` and `ToolCallCompleted`: use the stored result and never execute the tool again;
- `ToolCallPrepared` without `ToolCallCompleted`: project `OutcomeUnknown` and terminate the interrupted turn with a structured recovery error;
- mismatched call ID or hash: fail replay as divergent;
- preparation without a request, completion without preparation, or rejection after preparation: fail replay as corrupt.

`OutcomeUnknown` is derived during replay; it is not appended retroactively as if the original process observed the outcome. Phase 4B may define user-guided resolution or safe retry rules. Phase 4A never guesses whether a side effect happened.

## 10. Replay and recovery

### 10.1 Bounded open

Before reading, `FileEventStore` must:

- validate the session ID before path construction;
- reject symlinks and non-regular journal files;
- use no-follow behavior where the platform supports it;
- compare metadata before and after open to detect path substitution;
- reject files beyond the configured byte cap before allocation;
- read at most cap plus one byte;
- enforce an entry-count cap while decoding.

The initial default caps are constants owned by `lato-store` and are tested at their boundaries. Phase 4A does not expose user configuration for raising them.

### 10.2 Validation

Replay validates:

- supported schema version;
- exact session ID match;
- dense sequence beginning at zero;
- unique record IDs;
- monotonic record identity generation;
- valid turn relationships;
- valid tool prepared/completed pairing;
- request-hash equality;
- terminal-state consistency.

A complete malformed line, an invalid record in the middle of a file, a sequence gap, duplicate identity, or divergent hash is fatal. Replay returns no partial session projection.

### 10.3 Torn final line

Only the final unterminated record receives special handling:

- whitespace-only tail: truncate it;
- valid JSON record without a newline: validate it, retain it, append the newline, and synchronize;
- invalid partial JSON: treat it as a torn write, truncate to the previous complete line, and synchronize;
- any equivalent malformed content followed by a newline: treat it as complete corruption and fail.

This makes crash recovery predictable without hiding earlier damage.

### 10.4 Projection

Replay returns both validated envelopes and a `SessionProjection` containing:

- canonical model history;
- last journal sequence and record identity state;
- current session/turn terminal state;
- unresolved tool calls;
- the latest accepted input needed for diagnostics;
- import metadata.

`lato-agent` converts canonical model history to `HistoryItem` only at the compatibility edge. `lato-store` never depends on `HistoryItem`.

## 11. Legacy transcript migration

Current files remain at:

```text
$LATO_HOME/sessions/<session-id>.jsonl
```

New journals use:

```text
$LATO_HOME/sessions/<session-id>/events.jsonl
```

On resume:

1. if a valid new journal exists, it is authoritative and the legacy file is ignored;
2. if only a legacy file exists, read and validate the entire transcript before creating any journal;
3. convert each `HistoryItem` to provider-independent `ModelMessage` values;
4. write a complete journal to a uniquely named temporary file in the target directory;
5. append `LegacyTranscriptImported`, flush, synchronize file data, and synchronize the directory where required for rename durability;
6. replay the temporary file with the normal validator;
7. atomically rename it to `events.jsonl`;
8. retain the original transcript unchanged.

An incomplete migration temporary file is never resumed as a session. A later migration attempt removes only its own validated temporary target and starts again. If both formats exist, migration is not repeated.

Session listing merges legacy and journal-backed IDs, deduplicates them, and preserves existing user-facing ordering semantics.

## 12. Error handling

Journal errors use stable codes, including:

- `journal.io`;
- `journal.full`;
- `journal.parse`;
- `journal.unsafe_restore`;
- `journal.sequence`;
- `journal.duplicate_record`;
- `journal.schema_unsupported`;
- `journal.session_mismatch`;
- `journal.divergence`;
- `journal.incomplete_side_effect`;
- `journal.migration_failed`.

I/O failures during a durability barrier are fatal to the active turn. Parse, sequence, identity, safety, and divergence errors are non-retryable until the underlying journal is repaired or replaced. A first transient writer failure may be retried internally by reopening the file; callers observe only the final result.

No error path silently falls back to the legacy transcript after a new journal has been created.

## 13. API integration

`spawn_session` and `RuntimeSession` gain journal-aware constructors while retaining test-friendly constructors backed by an in-memory store. Existing CLI and ACP code wires one `FileEventStore` rooted at `LATO_HOME`.

Resume performs replay before the session loop starts. The loop receives the validated projection and next journal identity. `SessionStarted` is not duplicated for an existing journal; a distinct resume-related live event may be added later, but Phase 4A does not change the public ACP surface unnecessarily.

The existing transcript append path is removed from new-turn completion after journal integration. The legacy reader remains only for lazy migration and can be deleted in a later cleanup phase once the compatibility window closes.

## 14. Testing strategy

### 14.1 Core contract tests

- every record and envelope round-trips through serde;
- stable error codes and retryability;
- canonical JSON hashes are independent of object-key order;
- hash changes when identity, arguments, capability, sandbox, or side-effect metadata changes;
- projection reconstructs text, tool-call, tool-result, failure, cancellation, and terminal state.

### 14.2 Shared `EventStore` contract suite

The same suite runs against the in-memory store and `FileEventStore`:

- append/replay order;
- dense sequences and unique IDs;
- flush and sync acknowledgements;
- capacity rejection before mutation;
- failed append does not advance in-memory state;
- shutdown drains accepted records;
- resuming continues the next sequence.

### 14.3 File and security tests

- directory and file permissions;
- traversal session IDs;
- symlink journal and swapped-path rejection;
- byte and entry cap boundaries;
- complete malformed middle line fails;
- incomplete final line truncates;
- valid unterminated final line is retained and terminated;
- no partial projection is returned after fatal validation.

### 14.4 Fault injection

Inject failures:

- before and after line write;
- before and after flush;
- before and after `sync_data`;
- immediately before tool invocation;
- after the tool side effect but before completion record;
- after completion record but before model-visible conversation commit;
- during legacy temporary-file write, validation, sync, and rename.

Assertions must prove that non-idempotent side effects are never automatically repeated and that only a final torn record is repaired.

### 14.5 Integration and acceptance

- new CLI and ACP sessions create journals, not legacy transcripts;
- session list contains legacy and journal sessions once each;
- legacy resume imports atomically and preserves the original file;
- journal resume hydrates the same model history in CLI and ACP;
- model and reasoning deltas remain streaming and are absent from the canonical journal;
- tool request is durable before execution and stored output is reused during projection;
- journal failure prevents the related visible state event;
- existing acceptance matrix remains green;
- `cargo fmt --all -- --check`;
- `cargo test --workspace --no-fail-fast`;
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
- `cargo install --path .` followed by installed-binary doctor and offline smoke tests.

## 15. Acceptance criteria

Phase 4A is complete when:

1. every new session has one bounded canonical journal with a single writer;
2. complete conversation history is restorable without the old transcript;
3. canonical state is flushed before related client visibility;
4. non-idempotent tool preparation and completion use durable barriers;
5. an incomplete non-idempotent call cannot be silently replayed;
6. replay repairs only a torn final record and rejects all other structural corruption;
7. restore limits prevent memory or disk growth from stranding the session;
8. legacy sessions migrate atomically and remain recoverable from their untouched source file;
9. CLI and ACP retain equivalent list/resume behavior;
10. workspace tests, Clippy, local installation, and offline smoke tests pass.

## 16. Follow-on phases

Phase 4B may add snapshot creation and replay from the latest valid snapshot, plus explicit resolution for `OutcomeUnknown`. Phase 4C may add compaction records, secondary indexes, archival, and bounded raw trace storage. Those phases must preserve the Phase 4A journal as the canonical audit trail and must not rewrite history as if compaction or recovery decisions never occurred.
