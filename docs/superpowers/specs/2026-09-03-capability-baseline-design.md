# Lato 20 × 3 capability baseline

The user approved a fixed-model, 20-real-task, three-trial evaluation on 2026-09-03. This implementation measures issue repair, not feature development or security robustness.

## Frozen design

- Source: `princeton-nlp/SWE-bench_Verified`, test split. Preserve instance IDs and upstream base commits; send only the original problem statement and a uniform local-environment instruction to Lato. Never send reference patches, test patches, hints, or graders.
- Selection: 10 SymPy, 6 pytest, 3 Sphinx, 1 Flask issues, chosen before inference for locally reproducible Python environments and a spread of subsystem/difficulty. This is a purposive local adaptation, not an official SWE-bench score or representative coding-agent benchmark.
- Model: `openai-codex/gpt-5.6-luna`, already configured locally. Three fresh trials per instance; 600-second agent timeout and 180-second test timeout. No model fallback or automatic retry of completed trials. Provider failures count separately and remain in the attempted-trial denominator.
- Each issue must pass preflight: every upstream FAIL_TO_PASS test fails on the base plus test patch, every PASS_TO_PASS test passes, and all required tests pass after the reference patch. Infrastructure failures are reported; never quietly replace cases based on agent outcomes.
- Use pinned source archives and fresh local Git snapshots without upstream history. Each inference workspace contains only base code and its local Python environment. Grading occurs in a separate fresh snapshot with the original evaluation tests restored.
- Preserve the complete candidate patch, including added tests and untracked source files. Protect evaluation tests against candidate replacement. Retain stdout/stderr, journal, process status, durations, test outcomes, environment dependency lock, source/archive hashes, binary hash, and current Lato Git state. Missing token/cost data is null, not zero.
- Reuse the existing credential store without copying credentials into fixtures or artifacts. Match session journals by the unique workspace path. Run sequentially to avoid refresh contention and simplify attribution.
- Report trial pass rate, first-trial success, task success in at least 2/3 trials, and all-three success; retain all 20 tasks in coverage. Separately report invalid fixtures, provider failures, agent timeouts, and unresolved tasks. Passing requires required tests and no protected-test modifications, not the model's completion claim.

## Implementation and validation

Use a small Python standard-library runner and isolated dependency environments. Unit tests cover outcome mapping, missing/skipped tests, fixture validation, complete diff capture, and deadline handling. Keep runner tests offline. Verify existing Rust tests and install locally with `cargo install --path .` after implementation, per AGENTS.md. Freeze the installed binary before inference. Do not repair Lato or change prompts mid-baseline.

The host has no running Docker daemon. Native execution avoids a large image build and must be disclosed as a limitation. The workspace sandbox is not a hardened hidden-test or network-isolation boundary; this run does not make security claims.

## Calibration record

All final 20 cases passed base/reference validation before their formal trials. Pytest cases use Python 3.9.25; other cases use Python 3.11.15. Nested test temporary directories are outside each Git repository but inside the agent sandbox root. Upstream parametrized IDs truncated at whitespace are mapped to every matching concrete node; all must pass.

Sphinx issue 11510 was excluded before inference because its reference patch still failed document-name assertions under docutils 0.18.1, 0.19 and 0.20.1. It was replaced by Sphinx issue 10673, which passed preflight. Selection history is retained in the manifest.

The initial in-progress startup probe was interrupted because an unset LATO_HOME disabled journal persistence. It produced no graded outcome. Its workspace, log and run metadata are retained separately; the formal 60-trial run explicitly sets the existing credential home. No Lato implementation, model, or task prompt was changed for this repair.
