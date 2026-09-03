# Capability Baseline Implementation Plan

> Execution: inline, following the user's approval of the 20-task, three-trial baseline.

**Goal:** Produce reproducible empirical evidence of Lato's real issue-repair performance.

**Architecture:** A frozen public manifest contains prompts and base revisions. A private run directory holds upstream evaluation data, source archives, dependency environments, preflight evidence, trial artifacts, and summaries. A standard-library Python runner separates preparation, inference, and grading.

**Tech Stack:** Python 3.11, Git, uv-managed virtual environments, pytest, installed Lato CLI.

## Global constraints

Fixed model `openai-codex/gpt-5.6-luna`; 20 issues; 3 fresh trials each; 600-second agent deadline; 180-second test deadline. No credential copies. Preserve unrelated work. Native adaptation, not an official benchmark score. Missing usage data is null.

## Tasks

- [x] Freeze source task manifest and document selection limitations.
- [x] Implement common process, source snapshot, patch, test-outcome and reporting helpers in `evals/capability/runner.py`; implement the test event collector in `evals/capability/lato_eval_plugin.py`.
- [x] Implement prepare/preflight commands: fetch pinned source archives, install isolated dependencies, validate base failures and reference passes for every required upstream test. Persist invalid-case explanations.
- [x] Implement run/report commands: fresh workspaces, fixed binary/model, sequential bounded execution, full diff and attributable journal capture, independent grading, resumable per-trial result files, coverage-aware summaries.
- [x] Add `evals/capability/test_runner.py` for missing/skipped tests, false positives, patch capture, timeout cleanup, and report denominators; run `python3 -m unittest discover -s evals/capability -p 'test_*.py'`.
- [x] Run Rust regression tests, `cargo install --path .`, and freeze binary hash before inference.
- [x] Preflight all 20 cases; fix environment problems without changing Lato, prompts, or task goals. Record remaining infrastructure failures transparently.
- [x] Run all valid cases three times; review failures and generate a Chinese report with aggregate and per-case results, artifact links and limitations.

## Completion

All 60 trials completed: 45 full-workflow passes, 47 candidate patches passing the designated upstream tests. Four tasks failed all three attempts; 14 tasks passed all three. All 15 failed attempts have evidence-backed diagnoses. Frozen input hashes and 60 distinct session journals were verified. Raw evidence is retained under `/Users/huangyongzhao/.local/share/lato-evals/2026-09-03`; the Chinese report is `docs/testing/2026-09-03-capability-baseline.md`.

Final validation: 7 evaluator tests and 423 Rust tests passed. Local installation uses `cargo install --path .`. No Lato runtime fixes were made during this baseline.
