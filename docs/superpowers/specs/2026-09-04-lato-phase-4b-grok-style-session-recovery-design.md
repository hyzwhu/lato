# Lato Phase 4B Grok-Style Session Recovery Design

## 1. Status and decision

Phase 4B adds a recoverable, replaceable model-history projection beside Lato's append-only canonical event journal. The design follows the persistence split in Grok Build at commit `bb7f39d5858cbf5e00de639367f59debbdcb0138`:

- an append-only update stream remains the source of truth;
- the current model conversation is a derived JSONL file that may be replaced atomically;
- compaction checkpoints preserve replacement boundaries for replay and rewind;
- a missing or damaged derived conversation can be rebuilt from the source-of-truth stream.

For Lato, `events.jsonl` remains authoritative. Phase 4B does not rotate, truncate, archive, or delete canonical records after a checkpoint. The phrase "log compaction" therefore means compacting the model-visible history and materializing a faster recovery projection, not destroying the audit log.

Lato keeps the stricter Phase 4A guarantees around dense sequencing, bounded reads, exact tool-request hashes, and unresolved side effects. Grok Build's architecture is adopted without weakening those existing safety contracts.

## 2. Upstream behavior being followed

The relevant Grok Build structure is:

- `crates/codegen/xai-grok-shell/src/session/storage/mod.rs`: documents `updates.jsonl` as the durable source of truth and `chat_history.jsonl` as a rebuildable derived cache;
- `crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs`: appends chat messages, replaces the complete chat history, quarantines damaged history, and rebuilds via a temporary file plus rename;
- `crates/codegen/xai-grok-shell/src/session/persistence.rs`: serializes `ReplaceChatHistory` and compaction-checkpoint writes through one persistence actor;
- `crates/codegen/xai-chat-state/src/persistence.rs`: exposes append, complete-history replacement, flush, and backup-gated destructive replacement as the chat-state persistence boundary;
- `crates/codegen/xai-grok-shell/src/session/compaction.rs`: writes a compacted-history checkpoint file and then records its checkpoint marker in the update stream;
- `crates/codegen/xai-grok-shell/src/session/helpers/replay.rs`: reconstructs history by replaying the authoritative update stream and applying compaction checkpoints and rewind markers.

Grok Build's `ChatStateSnapshot` is primarily an in-memory state value for rewind and fork. Its durable recovery design is not a byte-offset snapshot that supersedes the event stream. Phase 4B intentionally follows that distinction.

## 3. Goals

Phase 4B must:

1. make normal session resume load current model messages from the derived history while canonical validation independently checks journal integrity and safety state;
2. preserve `events.jsonl` as the complete audit and recovery authority;
3. make the derived history replaceable for future context compaction and rewind;
4. rebuild derived history deterministically when it is absent, stale, or damaged;
5. preserve Phase 4A's fail-closed handling of canonical corruption and unresolved side effects;
6. serialize journal, derived-history, checkpoint, and metadata writes through one session writer;
7. make crashes at every write boundary converge to either the previous valid projection or a deterministic rebuild;
8. establish the durable checkpoint contract required by later `/compact` and rewind work without adding those user commands in this phase.

## 4. Non-goals

Phase 4B does not add:

- model-generated summarization or automatic context-window compaction;
- `/compact`, rewind, fork, or replay UI;
- deletion, rotation, compression, or archival of `events.jsonl`;
- SQLite, full-text search, or a global session index;
- recovery that retries an unresolved side effect;
- persistence of token-by-token model or reasoning deltas;
- a second component allowed to append to a live session journal;
- user-configurable thresholds or storage-retention policy.

## 5. Considered approaches

### 5.1 Chosen: Grok-style source stream plus replaceable projection

Keep the canonical event stream append-only and add a derived current-history file. Append ordinary committed model messages to both layers in source-first order. Replace the derived file atomically when replay, repair, future compaction, or future rewind changes the current conversation. Record replacement checkpoints in the authoritative journal.

This gives fast normal resume, complete audit history, and a clean future rewind boundary. It most closely matches Grok Build.

### 5.2 Rejected: byte-offset snapshot plus journal-tail replay

Store a full `SessionProjection`, a journal byte offset, and a checksum, then seek directly to the tail on resume. This is compact, but it creates a new snapshot authority that does not match Grok Build's update-stream/derived-history split. It also complicates validation of the skipped journal prefix.

### 5.3 Rejected: destructive journal rewrite or segment deletion

Replace or delete the canonical prefix once a snapshot is durable. This reclaims space, but removes audit and cross-compaction recovery information, expands crash-consistency risk, and contradicts the selected Grok Build behavior.

## 6. On-disk layout

Each canonical session directory becomes:

```text
sessions/<session-id>/
├── events.jsonl
├── history.jsonl
├── history.meta.json
└── compaction_checkpoints/
    └── <checkpoint-id>.json
```

`events.jsonl` keeps its Phase 4A schema and authority.

`history.jsonl` contains only the current provider-independent model conversation. Each line is a `HistoryProjectionEntry` with:

- `schema_version`;
- the journal sequence and record ID that produced or selected the entry;
- the complete `ModelMessage`;
- a canonical entry hash.

Including the source identity in every line allows stale metadata, duplicated tail appends, and replacement boundaries to be detected without guessing from message content.

`history.meta.json` contains:

- schema version and session ID;
- projection generation, starting at zero and increasing on full replacement;
- last incorporated journal sequence and record ID;
- entry count;
- whole-file byte length and SHA-256 digest;
- active checkpoint ID, if the projection begins from a compacted replacement;
- creation/update timestamp.

`compaction_checkpoints/<id>.json` contains the complete replacement history, the journal boundary it replaces through, the prompt/turn boundary, the prior checkpoint ID, and a content digest. It contains model-visible conversation data and receives the same restrictive permissions as the journal.

Temporary files use unique sibling names. Startup removes only validated, abandoned Phase 4B temporary files for the same session; it never follows symlinks or removes an unrecognized path.

## 7. Core contracts

### 7.1 Canonical record

Phase 4B adds `JournalRecord::HistoryProjectionReplaced` with:

- checkpoint ID and checkpoint content digest;
- replaced-through journal sequence and record ID;
- replacement entry count and history digest;
- replacement reason (`context_compaction`, `rewind`, or `repair`);
- prior checkpoint ID when present.

The record is a marker in the canonical stream. It does not embed the entire replacement history. A marker is valid only when its checkpoint artifact is already durable and matches its digest.

Recreating a missing, stale, or corrupt derived cache does not emit this marker because it does not change logical history. Phase 4B implements and tests the replacement/checkpoint contract as the persistence foundation for the later user-facing operations, but normal cache rebuild is deliberately marker-free.

### 7.2 Store API

`EventStore` remains the canonical journal contract. A new object-safe `HistoryProjectionStore` contract owns:

- load and validate current history;
- append committed conversation entries;
- durably replace complete history;
- write and read checkpoint artifacts;
- quarantine a damaged projection;
- rebuild projection from a validated `SessionProjection`.

`FileEventStore` implements both contracts so one writer can enforce ordering. `lato-runtime` depends on the traits; it does not know JSONL paths.

### 7.3 In-memory snapshot

`lato-core` adds a serializable `SessionSnapshot` containing the complete current conversation, journal cursor, session/turn terminal state, last accepted input, unresolved calls, and active checkpoint metadata. It is an immutable transfer value used by projection replacement and reserved for later rewind/fork work. Credentials, runtime environment variables, approval grants, and transient deltas are excluded.

## 8. Write ordering

The existing session writer remains the only disk writer. For an ordinary complete conversation item:

```text
append canonical record to events.jsonl
→ satisfy the record's Phase 4A durability barrier
→ append HistoryProjectionEntry to history.jsonl
→ update history.meta.json atomically
→ acknowledge projection success or a structured projection warning
→ broadcast related visible state
```

Canonical success never depends on the derived file. If the journal append succeeds but the derived append or metadata update fails, the turn follows the existing canonical visibility contract and records a structured projection warning. The projection is marked dirty in memory and must be rebuilt before the next resume or explicit flush can claim projection health.

For complete-history replacement:

```text
validate replacement in memory
→ write checkpoint artifact to a unique temp file
→ flush and sync checkpoint contents
→ atomically rename checkpoint and sync its directory
→ append HistoryProjectionReplaced to events.jsonl with SyncData
→ write new history.jsonl and history.meta.json temp files
→ flush and sync both
→ atomically publish history, then metadata
→ sync the session directory
→ swap the in-memory projection
→ emit completion
```

Publishing metadata last is the commit point for the derived generation. A crash before it leaves either the old valid generation or a detectable file/metadata mismatch. The checkpoint marker remains authoritative and lets startup reconstruct the unpublished derived file.

Replacement is refused while a tool outcome is unresolved, a turn is active, or an approval is waiting. The initial Phase 4B rebuild path runs before a resumed session accepts new input.

## 9. Load, validation, and rebuild

Resume always opens and validates `events.jsonl` with Phase 4A rules first. A valid derived projection is an optimization, never a way around canonical validation. The replay implementation gains two explicit modes: `ValidateWithDigest` validates every canonical envelope, reduces safety/terminal state, and calculates the current-history digest without retaining model messages; `ProjectFull` additionally materializes the complete model history and is used when the derived file must be rebuilt.

After canonical replay:

1. load `history.meta.json` and reject symlinks, non-regular files, wrong session IDs, unsupported versions, unsafe sizes, or impossible cursor values;
2. validate `history.jsonl` bounds, entry count, dense source ordering where applicable, per-entry hashes, byte length, and whole-file digest;
3. compare the canonical validation result's cursor, entry count, active checkpoint, and streaming history digest with the derived metadata;
4. use the derived messages when all anchors match;
5. otherwise quarantine a damaged projection and rebuild it atomically from the already validated canonical projection.

Canonical validation remains necessary in Phase 4B because it verifies the audit log and unresolved side-effect state. A healthy derived projection avoids materializing model messages from the complete event stream and avoids the legacy-history conversion pass. Phase 4B does not skip canonical bytes or weaken validation merely to claim startup speed.

A missing derived file is not corruption. A malformed canonical journal is corruption and still returns no partial session. A checkpoint marker with a missing, unsafe, oversized, or digest-mismatched checkpoint artifact is canonical divergence and fails closed.

Projection quarantine follows Grok Build's damaged-history pattern. The first damaged `history.jsonl` is atomically preserved as `history.jsonl.corrupt`; subsequent damage does not overwrite that first artifact. Metadata is preserved alongside it when possible. Quarantine failure does not authorize destructive replacement; rebuild must fail with a structured storage error if recoverability cannot be preserved.

## 10. Bounds and resource policy

Phase 4A's 64 MiB and 100,000-record canonical replay caps remain unchanged. The derived history and each checkpoint have independent conservative caps equal to the canonical byte cap and record cap. Limits are checked before allocation and before replacing a valid projection.

Because Grok Build retains its authoritative update stream, Phase 4B does not use checkpoints to evade the canonical cap. Raising the cap, segmenting the journal, or adding cold archives requires a separate design with explicit retention and security analysis.

No background timer is needed. Ordinary projection entries are written with their source records. Cache rebuild atomically republishes the canonical logical history without a checkpoint marker. Logical full replacement is available through the tested checkpoint contract; future compaction and rewind operations will call it.

## 11. Failure semantics

Stable error codes are added under `projection.*`:

- `projection.corrupt` — the derived history or metadata is structurally invalid;
- `projection.divergent` — derived anchors disagree with canonical state;
- `projection.checkpoint_missing` — a canonical replacement marker lacks its artifact;
- `projection.checkpoint_mismatch` — a checkpoint digest or boundary disagrees;
- `projection.limit_exceeded` — a projection/checkpoint crosses its bound;
- `projection.quarantine_failed` — damaged data could not be preserved before rebuild;
- `projection.write_failed` — append, replacement, sync, or atomic publication failed.

Derived absence, staleness, or ordinary corruption is recoverable only after the canonical journal passes full validation. Checkpoint absence or mismatch is fatal because the authoritative stream explicitly references it.

`ToolCallPrepared` without `ToolCallCompleted` remains `journal.incomplete_side_effect`. No projection or checkpoint may turn that state into a completed, rejected, or retryable call.

## 12. Compatibility and migration

Existing Phase 4A session directories have only `events.jsonl`. Their first successful resume performs normal canonical replay and creates `history.jsonl` plus metadata atomically. No journal record is added merely for creating a byte-identical derived cache.

Legacy top-level transcript import continues to run before Phase 4B projection materialization. The legacy source remains untouched. Once a canonical journal exists, it remains authoritative over both the legacy transcript and the new derived history.

Session listing and metadata commands do not need to open `history.jsonl`. Delete removes the whole session directory using the existing exact-target safety checks, including checkpoints and quarantined projection artifacts.

## 13. Testing strategy

### 13.1 Core contract tests

- `SessionSnapshot`, projection entry, metadata, checkpoint, and replacement-record serde round trips;
- stable digests under canonical JSON object-key ordering;
- credentials, grants, and transient deltas cannot enter snapshot types;
- replacement reasons and schema versions reject unknown invalid values predictably.

### 13.2 Store tests

- source-first ordinary append ordering;
- atomic history and metadata replacement;
- checkpoint-file-before-marker ordering;
- crash injection before and after every flush, sync, rename, marker append, and metadata publication;
- missing/stale projection rebuild;
- torn final derived line quarantine and rebuild;
- malformed middle line, wrong session, wrong cursor, bad entry hash, bad whole-file digest, and metadata/file mismatch;
- first-corrupt-artifact-wins quarantine behavior;
- quarantine failure gates destructive rewrite;
- symlink, non-file, path substitution, byte-cap, and record-cap rejection;
- checkpoint missing, mismatch, unsupported version, and unsafe path failures;
- writer restart and reopen preserve FIFO ordering.

### 13.3 Runtime and agent tests

- complete messages become derived entries only after their canonical records are durable;
- model/reasoning deltas never enter history projection;
- resume hydrates the same model history with a valid, missing, stale, or rebuilt projection;
- a projection failure cannot hide an already committed canonical record;
- unresolved prepared tool calls fail identically with and without a derived projection;
- replacement is rejected during an active turn, approval wait, or unresolved call;
- ACP, headless, and TUI resume all use the same reconstruction path.

### 13.4 End-to-end gates

- migrate and resume a pre-Phase-4B journal;
- interrupt a session at injected persistence boundaries, restart it, and compare the recovered history with uninterrupted execution;
- corrupt only the derived file and verify successful quarantine/rebuild;
- corrupt the canonical journal and verify fail-closed resume;
- run `cargo test --workspace`;
- run `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
- install with `cargo install --path .`;
- run installed-binary offline new-session and resume smoke tests with an isolated `LATO_HOME`.

## 14. Source ledger requirements

Implementation structurally derived from the Grok Build files listed in section 2 must carry exact source headers and add rows to `docs/superpowers/reference/lato-upstream-sources.md`. The pinned upstream commit is `bb7f39d5858cbf5e00de639367f59debbdcb0138`, licensed under Apache-2.0.

The implementation must not copy Grok-specific remote session, account, telemetry, cloud archive, billing, branding, or product UI behavior.

## 15. Acceptance criteria

Phase 4B is complete when:

1. every canonical session has an optional, validated, rebuildable current-history projection;
2. existing sessions migrate lazily without modifying their canonical records;
3. ordinary writes and full replacements obey the single-writer ordering above;
4. damaged derived data is preserved and rebuilt, while damaged canonical data still fails closed;
5. checkpoint artifacts and canonical markers cannot diverge silently;
6. side-effect recovery semantics are unchanged;
7. all workspace tests and Clippy pass;
8. the resulting binary is installed locally and the installed `lato` passes offline create/resume smoke tests.
